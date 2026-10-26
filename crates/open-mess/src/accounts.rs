//! Accounts (spec 8.3, 8.8). Each account belongs to exactly one module, the one that signed it
//! in; its tools and events go to that module only. Account ids are `<module>:<module_account>`,
//! where `module_account` is the module's own id for it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use keyring_core::api::CredentialStore;
use open_mess_host::{AccountInfo, HostError, ModuleError, SessionState};
use rusqlite::OptionalExtension;
use tokio::task::JoinSet;

use crate::Error;
use crate::modules::{ModuleState, Modules};
use crate::store::Store;

/// The longest module account id taken; it becomes part of credential names.
const MAX_ACCOUNT_ID: usize = 256;
/// Display names are cut to this many characters.
const MAX_DISPLAY_NAME: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub module: String,
    pub module_account: String,
    pub display_name: String,
    /// `online`, `offline` or `auth-required`, as the module last reported it.
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
}

impl Accounts {
    pub fn new(store: Store, modules: Modules, data_dir: PathBuf, credentials: Arc<CredentialStore>) -> Accounts {
        Accounts { store: Mutex::new(store), modules, data_dir, credentials }
    }

    /// Never held across an await.
    fn store(&self) -> MutexGuard<'_, Store> {
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

    /// Records what a module reported about one of its accounts. Unknown accounts are ignored.
    pub fn set_state(&self, module: &str, module_account: &str, state: SessionState) -> Result<(), Error> {
        self.store()
            .conn()
            .execute("UPDATE accounts SET state = ?1 WHERE module = ?2 AND module_account = ?3", [state_name(state), module, module_account])?;
        Ok(())
    }

    /// The state agents see: an account whose module does not run is offline (spec 8.3).
    pub fn effective_state(&self, account: &Account) -> String {
        match self.modules.state(&account.module) {
            Some(ModuleState::Active) => account.state.clone(),
            _ => state_name(SessionState::Offline).into(),
        }
    }

    /// Restores every account's session at daemon start, all modules at once.
    pub async fn restore(&self) -> Result<(), Error> {
        let mut restores = JoinSet::new();
        for account in self.list()? {
            match self.modules.handle(&account.module) {
                Some(handle) => {
                    let handle = handle.clone();
                    restores.spawn(async move {
                        let state = match handle.restore(&account.module_account).await {
                            Ok(()) => SessionState::Online,
                            Err(HostError::Module(ModuleError::AuthRequired(_))) => SessionState::AuthRequired,
                            Err(_) => SessionState::Offline,
                        };
                        (account, state)
                    });
                }
                None => self.set_state(&account.module, &account.module_account, SessionState::Offline)?,
            }
        }
        while let Some(restored) = restores.join_next().await {
            if let Ok((account, state)) = restored {
                self.set_state(&account.module, &account.module_account, state)?;
            }
        }
        Ok(())
    }

    /// Signs the account out (when its module runs), deletes its secrets and then its row (spec
    /// 8.8). Secrets that cannot be deleted keep the account, so removing it can be tried again.
    pub async fn remove(&self, id: &str) -> Result<(), Error> {
        let account = self.get(id)?.ok_or_else(|| Error::Account(format!("unknown account {id:?}")))?;
        if let Some(handle) = self.modules.handle(&account.module) {
            // A failed sign-out leaves a session on the server; the account goes anyway.
            let _ = tokio::time::timeout(open_mess_host::limits::CALL_TIMEOUT, handle.sign_out(&account.module_account)).await;
        }
        let (credentials, data_dir, module, module_account) =
            (Arc::clone(&self.credentials), self.data_dir.clone(), account.module.clone(), account.module_account.clone());
        tokio::task::spawn_blocking(move || open_mess_host::delete_account_secrets(&*credentials, &data_dir, &module, &module_account))
            .await
            .map_err(|err| Error::Account(format!("cannot delete the account's secrets: {err}")))?
            .map_err(|why| Error::Account(format!("cannot delete the account's secrets: {why}")))?;
        self.store().conn().execute("DELETE FROM accounts WHERE id = ?1", [id])?;
        Ok(())
    }
}

fn account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account { id: row.get(0)?, module: row.get(1)?, module_account: row.get(2)?, display_name: row.get(3)?, state: row.get(4)? })
}
