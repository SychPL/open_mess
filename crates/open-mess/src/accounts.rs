//! Accounts (spec 8.3, 8.8). Each account belongs to exactly one module, the one that signed it
//! in; its tools and events go to that module only. Account ids are `<module>:<module_account>`,
//! where `module_account` is the module's own id for it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use keyring_core::api::CredentialStore;
use open_mess_host::{AccountInfo, Attachment, HostError, ModuleError, SessionState};
use rusqlite::OptionalExtension;
use serde_json::Value;
use tokio::task::JoinSet;

use crate::Error;
use crate::modules::{ModuleState, Modules};
use crate::store::Store;
use crate::tools::{ToolError, shared};

/// The longest module account id taken; it becomes part of credential names.
const MAX_ACCOUNT_ID: usize = 256;
/// Display names are cut to this many characters.
const MAX_DISPLAY_NAME: usize = 256;
/// The state of an account being removed: no tool reaches it, no module report changes it, and a
/// removal cut short (the daemon stopped) can be run again.
pub const REMOVING: &str = "removing";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub module: String,
    pub module_account: String,
    pub display_name: String,
    /// `online`, `offline` or `auth-required`, as the module last reported it, or [`REMOVING`].
    pub state: String,
}

pub fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Online => "online",
        SessionState::Offline => "offline",
        SessionState::AuthRequired => "auth-required",
    }
}

/// The accounts table and what the accounts' tools run on: the modules, the data folder and the
/// credential store that holds the modules' secrets.
pub struct Accounts {
    store: Mutex<Store>,
    modules: Modules,
    data_dir: PathBuf,
    credentials: Arc<CredentialStore>,
    /// Held while an attachment is saved and while a removed account's attachments are deleted, so
    /// a download that ends during a removal leaves no file behind.
    attachments: Mutex<()>,
}

impl Accounts {
    pub fn new(store: Store, modules: Modules, data_dir: PathBuf, credentials: Arc<CredentialStore>) -> Accounts {
        Accounts { store: Mutex::new(store), modules, data_dir, credentials, attachments: Mutex::new(()) }
    }

    /// Never held across an await.
    pub(crate) fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn modules(&self) -> &Modules {
        &self.modules
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn list(&self) -> Result<Vec<Account>, Error> {
        let store = self.store();
        let mut statement = store.conn().prepare("SELECT id, module, module_account, display_name, state FROM accounts ORDER BY id")?;
        let accounts = statement.query_map([], account)?.collect::<Result<_, _>>()?;
        Ok(accounts)
    }

    pub fn get(&self, id: &str) -> Result<Option<Account>, Error> {
        let store = self.store();
        let found =
            store.conn().query_row("SELECT id, module, module_account, display_name, state FROM accounts WHERE id = ?1", [id], account).optional()?;
        Ok(found)
    }

    /// Records an account a module signed in (the sign-in flow of milestones E and F2); signing in
    /// again updates its name and state.
    pub fn add(&self, module: &str, info: AccountInfo) -> Result<Account, Error> {
        if info.id.is_empty() || info.id.len() > MAX_ACCOUNT_ID || info.id.chars().any(char::is_control) {
            return Err(Error::Account(format!("module {module} gave an account id that is empty, too long or has control characters")));
        }
        let display_name: String = info.display_name.chars().filter(|c| !c.is_control()).take(MAX_DISPLAY_NAME).collect();
        let account = Account {
            id: format!("{module}:{}", info.id),
            module: module.into(),
            module_account: info.id,
            display_name,
            state: state_name(SessionState::Online).into(),
        };
        self.store().conn().execute(
            "INSERT INTO accounts (id, module, module_account, display_name, state) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET display_name = excluded.display_name, state = excluded.state",
            [&account.id, &account.module, &account.module_account, &account.display_name, &account.state],
        )?;
        Ok(account)
    }

    /// Records what a module reported about one of its accounts. Unknown accounts and accounts
    /// being removed are left alone.
    pub fn set_state(&self, module: &str, module_account: &str, state: SessionState) -> Result<(), Error> {
        self.store().conn().execute(
            "UPDATE accounts SET state = ?1 WHERE module = ?2 AND module_account = ?3 AND state <> ?4",
            [state_name(state), module, module_account, REMOVING],
        )?;
        Ok(())
    }

    /// The state agents see: an account whose module does not run is offline (spec 8.3).
    pub fn effective_state(&self, account: &Account) -> String {
        match self.modules.state(&account.module) {
            _ if account.state == REMOVING => REMOVING.into(),
            Some(ModuleState::Active) => account.state.clone(),
            _ => state_name(SessionState::Offline).into(),
        }
    }

    /// Restores every account's session at daemon start, all modules at once.
    pub async fn restore(&self) -> Result<(), Error> {
        self.restore_where(|_| true).await
    }

    /// Reads the modules folder again (spec 8.3; the configuration window asks for it), and
    /// restarts the module `restart` if it was stopped after repeated crashes. Returns the modules
    /// that stopped, started or were replaced. Their accounts' waiting actions expire in the same
    /// step, before any of them can be approved and run on another module than the one that
    /// described them; their sessions are then restored, or marked offline.
    pub async fn rescan_modules(&self, restart: Option<&str>) -> Result<Vec<String>, Error> {
        let found = self.modules.load().await?;
        let changed = {
            let store = self.store();
            let changed = self.modules.adopt(&store, found, restart)?;
            for module in &changed {
                store.conn().execute(
                    "UPDATE pending SET state = 'expired', args = '', detail = 'its module stopped or was replaced before it was approved'
                     WHERE state = 'waiting' AND account IN (SELECT id FROM accounts WHERE module = ?1)",
                    [module],
                )?;
            }
            changed
        };
        self.restore_where(|module| changed.iter().any(|name| name == module)).await?;
        Ok(changed)
    }

    /// Restores the sessions of the accounts of the modules `module` picks.
    async fn restore_where(&self, module: impl Fn(&str) -> bool) -> Result<(), Error> {
        let mut restores = JoinSet::new();
        for account in self.list()?.into_iter().filter(|account| account.state != REMOVING && module(&account.module)) {
            match self.modules.handle(&account.module) {
                Some(handle) => {
                    restores.spawn(async move {
                        let state = match handle.restore(&account.module_account).await {
                            Ok(()) => SessionState::Online,
                            Err(HostError::Module(ModuleError::AuthRequired(_))) => SessionState::AuthRequired,
                            Err(_) => SessionState::Offline,
                        };
                        (account, handle, state)
                    });
                }
                None => self.set_state(&account.module, &account.module_account, SessionState::Offline)?,
            }
        }
        while let Some(restored) = restores.join_next().await {
            // A module replaced meanwhile has its own restore; this one's answer is out of date.
            if let Ok((account, handle, state)) = restored
                && self.modules.handle(&account.module).is_some_and(|current| current.same(&handle))
            {
                self.set_state(&account.module, &account.module_account, state)?;
            }
        }
        Ok(())
    }

    /// Writes an attachment downloaded for the account, unless the account is gone or being
    /// removed by now. Blocking.
    pub fn save_attachment(&self, id: &str, attachment: Attachment) -> Result<Value, ToolError> {
        let _saving = self.attachments.lock().unwrap_or_else(PoisonError::into_inner);
        match self.get(id) {
            Ok(Some(account)) if account.state != REMOVING => shared::save_attachment(&self.data_dir, id, attachment),
            Ok(_) => Err(ToolError::Module(format!("account {id:?} was removed"))),
            Err(err) => Err(ToolError::Failed(err.to_string())),
        }
    }

    /// Removes the account (spec 8.8). It is marked [`REMOVING`] first, so no new call reaches
    /// its module; then the module signs it out, after every call already queued for it (a module
    /// runs its calls in order, and no time limit here cuts that wait short, so a queued restore
    /// cannot store a token after the secrets are gone); then its secrets go, and last its
    /// permissions, pending actions, trigger rules, row and attachments. A removal cut short keeps the marked row, so it can be run again; secrets that
    /// cannot be deleted leave the account signed out (offline).
    pub async fn remove(&self, id: &str) -> Result<(), Error> {
        let account = self.get(id)?.ok_or_else(|| Error::Account(format!("unknown account {id:?}")))?;
        self.store().conn().execute("UPDATE accounts SET state = ?2 WHERE id = ?1", [id, REMOVING])?;
        if let Some(handle) = self.modules.handle(&account.module) {
            // A failed sign-out leaves a session on the server; the account goes anyway. The
            // host's own call limit bounds the call.
            let _ = handle.sign_out(&account.module_account).await;
        }
        let (credentials, data_dir, module, module_account) =
            (Arc::clone(&self.credentials), self.data_dir.clone(), account.module.clone(), account.module_account.clone());
        let deleted = tokio::task::spawn_blocking(move || open_mess_host::delete_account_secrets(&*credentials, &data_dir, &module, &module_account))
            .await
            .unwrap_or_else(|err| Err(err.to_string()));
        if let Err(why) = deleted {
            self.store().conn().execute("UPDATE accounts SET state = ?2 WHERE id = ?1", [id, state_name(SessionState::Offline)])?;
            return Err(Error::Account(format!("cannot delete the account's secrets: {why}")));
        }
        let _saving = self.attachments.lock().unwrap_or_else(PoisonError::into_inner);
        {
            // The row goes last: a removal cut short before it is run again.
            let store = self.store();
            // Signed in again later, the account starts from the default permissions.
            store.conn().execute("DELETE FROM permissions WHERE account = ?1", [id])?;
            store.conn().execute("DELETE FROM pending WHERE account = ?1", [id])?;
            store.conn().execute("DELETE FROM trigger_rules WHERE account = ?1", [id])?;
            store.conn().execute("DELETE FROM accounts WHERE id = ?1", [id])?;
        }
        // Best effort: a file an agent still has open stays until the attachments' retention.
        let _ = std::fs::remove_dir_all(shared::attachments_dir(&self.data_dir, id));
        Ok(())
    }
}

fn account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account { id: row.get(0)?, module: row.get(1)?, module_account: row.get(2)?, display_name: row.get(3)?, state: row.get(4)? })
}
