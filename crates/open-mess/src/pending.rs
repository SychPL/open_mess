//! Pending actions (spec 8.4-8.6): every write (send, edit, reaction) is an action stored here in
//! all its states, and only [`Pending::decide`] and the write tools run one. `autonomous` runs at
//! once; `confirm` waits for the user's approval (outside an agent's reach) and runs only if the
//! target still matches what the approval showed; `never` is refused.
//!
//! The approval binds to a fingerprint: SHA-256 of the conversation's sorted participant ids and
//! the target message's text, as the module describes them. Before running an approved action the
//! core describes the target again; another fingerprint voids the action (spec 8.6, R5-R7).
//! An action that may have reached the platform without an answer is `uncertain` and never run
//! again by itself (spec 8.5, R7). Nothing here logs or returns message text.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use open_mess_host::limits::CALL_TIMEOUT;
use open_mess_host::{HostError, ModuleError, ModuleHandle, Target};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::watch;

use crate::Error;
use crate::accounts::{Account, Accounts};
use crate::permissions::{self, Write};
use crate::tools::{ToolError, find, module, within};

/// How long an action waits for the user's approval.
pub const APPROVAL_TTL: Duration = Duration::from_secs(10 * 60);
/// Actions waiting for approval per account; more is an error, so a loop can't bury the user.
pub const MAX_WAITING: i64 = 20;
/// Actions in any state are deleted this long after they were created.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Finished actions kept per account; past it the oldest go first, uncertain ones last (they warn
/// that a write may have gone out), so an agent writing in a loop can't fill the disk (spec 15).
pub const MAX_KEPT: i64 = 1000;
const EXPIRY_SWEEP: Duration = Duration::from_secs(30);
const RETENTION_SWEEP: Duration = Duration::from_secs(24 * 60 * 60);
/// What the approval shows of the module's description: titles and names, and the target text.
const MAX_NAME: usize = 256;
const MAX_TEXT: usize = 28_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Send,
    Edit,
    React,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Send => "send",
            Kind::Edit => "edit",
            Kind::React => "react",
        }
    }

    /// The write tool (and module tool) that makes it.
    pub fn for_tool(tool: &str) -> Option<Kind> {
        [Kind::Send, Kind::Edit, Kind::React].into_iter().find(|kind| kind.tool() == tool)
    }

    fn tool(self) -> &'static str {
        match self {
            Kind::Send => "send",
            Kind::Edit => "edit_message",
            Kind::React => "react",
        }
    }

    /// The message the action is about: the one replied to, edited or reacted to.
    fn message(self, args: &Value) -> Option<&str> {
        args.get(if self == Kind::Send { "reply_to" } else { "message" }).and_then(Value::as_str)
    }

    fn parse(name: &str) -> Result<Kind, Error> {
        [Kind::Send, Kind::Edit, Kind::React].into_iter().find(|kind| kind.name() == name).ok_or_else(|| bad_row("kind", name))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Waiting,
    /// Approved (or autonomous) and running.
    Approved,
    Done,
    Rejected,
    Expired,
    /// The target changed after the approval; nothing was sent.
    Voided,
    /// It may have reached the platform; not run again.
    Uncertain,
    Failed,
}

impl State {
    pub const ALL: [State; 8] =
        [State::Waiting, State::Approved, State::Done, State::Rejected, State::Expired, State::Voided, State::Uncertain, State::Failed];

    pub fn name(self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Approved => "approved",
            State::Done => "done",
            State::Rejected => "rejected",
            State::Expired => "expired",
            State::Voided => "voided",
            State::Uncertain => "uncertain",
            State::Failed => "failed",
        }
    }

    pub(crate) fn parse(name: &str) -> Result<State, Error> {
        State::ALL.into_iter().find(|state| state.name() == name).ok_or_else(|| bad_row("state", name))
    }
}

fn bad_row(what: &str, value: &str) -> Error {
    Error::BadConfig(format!("unknown pending action {what} {value:?}"))
}

/// A waiting action as its approval shows it: the exact account, conversation, participants and
/// content (spec 8.6).
#[derive(Clone, PartialEq, Eq)]
pub struct Preview {
    pub action: String,
    pub kind: Kind,
    /// The account's display name and id.
    pub account: String,
    pub conversation_title: String,
    /// The participants' names; empty for a channel.
    pub participants: Vec<String>,
    /// The text to send, or an edit's new text.
    pub text: Option<String>,
    pub reaction: Option<String>,
    /// The current text of the message replied to, edited or reacted to.
    pub target_text: Option<String>,
    /// That text was longer than an approval shows, so this action can only be rejected.
    pub target_cut: bool,
}

// Hand-written so that a `{:?}` never prints names or text.
impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preview").field("action", &self.action).field("kind", &self.kind).finish_non_exhaustive()
    }
}

/// What a write tool answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub action: String,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The pending actions of a daemon's accounts.
pub struct Pending {
    accounts: Arc<Accounts>,
    /// The waiting actions' previews, oldest first. Only in memory: the actions still waiting when
    /// the daemon stopped expire at its next start ([`Pending::recover`]).
    waiting: Mutex<Vec<Preview>>,
    /// Told whenever the waiting actions change, for the approval queue.
    changed: watch::Sender<()>,
}

impl Pending {
    pub fn new(accounts: Arc<Accounts>) -> Pending {
        Pending { accounts, waiting: Mutex::new(Vec::new()), changed: watch::Sender::new(()) }
    }

    /// At daemon start: actions left waiting expire (their previews are gone), and actions that
    /// were running may have reached the platform.
    pub fn recover(&self) -> Result<(), Error> {
        let store = self.accounts.store();
        store.conn().execute(
            "UPDATE pending SET state = 'expired', args = '', detail = 'open_mess restarted before it was approved' WHERE state = 'waiting'",
            [],
        )?;
        store.conn().execute(
            "UPDATE pending SET state = 'uncertain', args = '', detail = ?1 WHERE state = 'approved'",
            [format!("open_mess stopped while running it; {CHECK}")],
        )?;
        Ok(())
    }

    fn previews(&self) -> MutexGuard<'_, Vec<Preview>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The waiting actions, oldest first, as their approvals show them.
    pub fn waiting(&self) -> Vec<Preview> {
        self.previews().clone()
    }

    /// Changes whenever [`Pending::waiting`] does.
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    /// Creates the action a write tool asked for, under the account's or conversation's write
    /// permission: run at once, left waiting for approval, or refused.
    pub async fn create(&self, kind: Kind, account: &str, conversation: &str, args: Value) -> Result<Outcome, ToolError> {
        let account = find(&self.accounts, account)?;
        let write = permissions::effective(&self.accounts.store(), &account.id, Some(conversation)).map_err(failed)?.write;
        match write {
            Write::Never => Err(ToolError::NotAllowed("writing to this conversation is not allowed".into())),
            Write::Autonomous => {
                let action = self.insert(&account, conversation, kind, &args, "", State::Approved, None)?;
                let (state, detail) = self.execute(&action).await.map_err(failed)?;
                Ok(Outcome { action, state, detail })
            }
            Write::Confirm => {
                if self.count_waiting(&account.id)? >= MAX_WAITING {
                    return Err(too_many());
                }
                let handle = module(&self.accounts, &account)?;
                let target = within(&account.module, handle.describe_target(&account.module_account, conversation, kind.message(&args))).await?;
                let action = self.insert(&account, conversation, kind, &args, &fingerprint(&target), State::Waiting, Some(&handle))?;
                self.previews().push(preview(&action, kind, &account, &args, target));
                self.changed.send_replace(());
                Ok(Outcome { action, state: State::Waiting, detail: None })
            }
        }
    }

    /// The actions in `state`, over all accounts.
    pub fn count(&self, state: State) -> Result<usize, Error> {
        let count: i64 = self.accounts.store().conn().query_row("SELECT COUNT(*) FROM pending WHERE state = ?1", [state.name()], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or_default())
    }

    fn count_waiting(&self, account: &str) -> Result<i64, ToolError> {
        let count: i64 = self
            .accounts
            .store()
            .conn()
            .query_row("SELECT COUNT(*) FROM pending WHERE account = ?1 AND state = 'waiting'", [account], |row| row.get(0))
            .map_err(failed)?;
        Ok(count)
    }

    /// Stores a new action; a waiting one only while the account has fewer than [`MAX_WAITING`],
    /// and only while `described_by`, the module that described it for its approval, is still the
    /// account's module.
    #[allow(clippy::too_many_arguments)]
    fn insert(
        &self,
        account: &Account,
        conversation: &str,
        kind: Kind,
        args: &Value,
        fingerprint: &str,
        state: State,
        described_by: Option<&ModuleHandle>,
    ) -> Result<String, ToolError> {
        let action = new_id()?;
        // Checked under the store lock, which a module swap holds while it expires the waiting
        // actions (Accounts::rescan_modules): no action described by the old module is stored
        // after that.
        let store = self.accounts.store();
        let replaced = |handle: &ModuleHandle| !self.accounts.modules().handle(&account.module).is_some_and(|current| current.same(handle));
        if described_by.is_some_and(replaced) {
            return Err(ToolError::Module(format!("module {} was replaced while the action was prepared - try again", account.module)));
        }
        let inserted = store
            .conn()
            .execute(
                "INSERT INTO pending (id, account, conversation, kind, args, fingerprint, state, created_ms)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
                 WHERE ?7 <> 'waiting' OR (SELECT COUNT(*) FROM pending WHERE account = ?2 AND state = 'waiting') < ?9",
                rusqlite::params![action, account.id, conversation, kind.name(), args.to_string(), fingerprint, state.name(), now_ms(), MAX_WAITING],
            )
            .map_err(failed)?;
        if inserted == 0 {
            return Err(too_many());
        }
        // Best effort: the action is stored, and the next one tries again.
        let _ = store.conn().execute(
            "DELETE FROM pending WHERE rowid IN (SELECT rowid FROM pending WHERE account = ?1 AND state NOT IN ('waiting', 'approved')
             ORDER BY state = 'uncertain' DESC, created_ms DESC, rowid DESC LIMIT -1 OFFSET ?2)",
            rusqlite::params![account.id, MAX_KEPT],
        );
        Ok(action)
    }

    /// The user's decision on a waiting action; an approved one runs before this returns. A
    /// decision on an action that is no longer waiting changes nothing and returns its state.
    /// Only the approval UI, the configuration window (milestone E) and tests call this.
    pub async fn decide(&self, action: &str, approve: bool) -> Result<State, Error> {
        let decided = if approve { State::Approved } else { State::Rejected };
        let now = now_ms();
        // The text stays only while the action can still run.
        let changed = self.accounts.store().conn().execute(
            "UPDATE pending SET state = ?2, decided_ms = ?3, args = CASE WHEN ?2 = 'approved' THEN args ELSE '' END
             WHERE id = ?1 AND state = 'waiting' AND created_ms > ?4",
            rusqlite::params![action, decided.name(), now, now.saturating_sub(ttl_ms())],
        )?;
        if changed == 0 {
            self.expire_due()?;
            return self.state(action);
        }
        self.forget(action);
        if approve { Ok(self.execute(action).await?.0) } else { Ok(State::Rejected) }
    }

    /// The `pending` tool: the newest actions first, never their text.
    pub fn list(&self, account: Option<&str>, state: Option<State>, limit: u32) -> Result<Value, ToolError> {
        let store = self.accounts.store();
        let mut statement = store
            .conn()
            .prepare(
                "SELECT id, account, conversation, kind, state, created_ms, decided_ms, detail FROM pending
                 WHERE (?1 IS NULL OR account = ?1) AND (?2 IS NULL OR state = ?2) ORDER BY created_ms DESC, rowid DESC LIMIT ?3",
            )
            .map_err(failed)?;
        let rows = statement
            .query_map(rusqlite::params![account, state.map(State::name), limit], |row| {
                let mut item = Map::new();
                for (index, key) in ["action", "account", "conversation", "kind", "state"].into_iter().enumerate() {
                    item.insert(key.into(), row.get::<_, String>(index)?.into());
                }
                item.insert("created".into(), time(row.get(5)?).into());
                if let Some(decided) = row.get::<_, Option<i64>>(6)? {
                    item.insert("decided".into(), time(decided).into());
                }
                if let Some(detail) = row.get::<_, Option<String>>(7)? {
                    item.insert("detail".into(), detail.into());
                }
                Ok(Value::Object(item))
            })
            .map_err(failed)?;
        let items = rows.collect::<Result<Vec<_>, _>>().map_err(failed)?;
        Ok(json!({ "items": items }))
    }

    /// An action's state.
    pub fn state(&self, action: &str) -> Result<State, Error> {
        let state: Option<String> =
            self.accounts.store().conn().query_row("SELECT state FROM pending WHERE id = ?1", [action], |row| row.get(0)).optional()?;
        State::parse(&state.ok_or_else(|| Error::UnknownAction(action.into()))?)
    }

    fn forget(&self, action: &str) {
        let mut previews = self.previews();
        let before = previews.len();
        previews.retain(|preview| preview.action != action);
        if previews.len() != before {
            self.changed.send_replace(());
        }
    }

    /// Runs an approved action and records how it ended.
    async fn execute(&self, action: &str) -> Result<(State, Option<String>), Error> {
        let row = self.accounts.store().conn().query_row(
            "SELECT account, conversation, kind, args, fingerprint FROM pending WHERE id = ?1 AND state = 'approved'",
            [action],
            |row| Ok(Row { account: row.get(0)?, conversation: row.get(1)?, kind: row.get(2)?, args: row.get(3)?, fingerprint: row.get(4)? }),
        )?;
        let (state, detail) = self.run(&row, Kind::parse(&row.kind)?).await;
        self.accounts.store().conn().execute(
            "UPDATE pending SET state = ?2, detail = ?3, args = '' WHERE id = ?1 AND state = 'approved'",
            rusqlite::params![action, state.name(), detail],
        )?;
        Ok((state, detail))
    }

    async fn run(&self, row: &Row, kind: Kind) -> (State, Option<String>) {
        let account = match find(&self.accounts, &row.account) {
            Ok(account) => account,
            Err(_) => return (State::Failed, Some("the account was removed".into())),
        };
        let handle = match module(&self.accounts, &account) {
            Ok(handle) => handle,
            Err(err) => return (State::Failed, Some(err.to_string())),
        };
        // An autonomous action was never shown, so there is nothing to compare.
        if !row.fingerprint.is_empty() {
            let args = serde_json::from_str::<Value>(&row.args).unwrap_or_default();
            match describe(&self.accounts, &account, &row.conversation, kind.message(&args)).await {
                Err(err) => return (State::Failed, Some(format!("cannot check the target: {err}"))),
                Ok(target) if fingerprint(&target) != row.fingerprint => return (State::Voided, Some(VOIDED.into())),
                Ok(_) => {}
            }
        }
        let module_name = &account.module;
        match tokio::time::timeout(CALL_TIMEOUT, handle.call(&account.module_account, kind.tool(), &row.args)).await {
            // What the module returns besides success is not needed, and never shown.
            Ok(Ok(_)) => (State::Done, None),
            Ok(Err(HostError::Module(ModuleError::Uncertain(why)))) => (State::Uncertain, Some(format!("module {module_name}: {why}; {CHECK}"))),
            Ok(Err(HostError::Module(err))) => (State::Failed, Some(format!("module {module_name}: {err}"))),
            // The request may have gone out before the module stopped, or still go out from its queue.
            Ok(Err(HostError::Timeout | HostError::Crashed(_))) | Err(_) => {
                (State::Uncertain, Some(format!("module {module_name} did not answer in time; {CHECK}")))
            }
            Ok(Err(err)) => (State::Failed, Some(format!("module {module_name}: {err}"))),
        }
    }

    /// Expires the actions that waited longer than [`APPROVAL_TTL`], and forgets the previews of
    /// actions that are no longer waiting (an account removed meanwhile).
    pub fn expire_due(&self) -> Result<(), Error> {
        // Taken first: an action created meanwhile adds its preview only after the ids are read.
        let mut previews = self.previews();
        let store = self.accounts.store();
        store.conn().execute(
            "UPDATE pending SET state = 'expired', args = '', detail = 'not approved in time' WHERE state = 'waiting' AND created_ms <= ?1",
            [now_ms().saturating_sub(ttl_ms())],
        )?;
        let mut statement = store.conn().prepare("SELECT id FROM pending WHERE state = 'waiting'")?;
        let waiting: Vec<String> = statement.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;
        let before = previews.len();
        previews.retain(|preview| waiting.contains(&preview.action));
        if previews.len() != before {
            self.changed.send_replace(());
        }
        Ok(())
    }

    /// Deletes actions older than [`RETENTION`].
    pub fn delete_old(&self) -> Result<(), Error> {
        let cutoff = now_ms().saturating_sub(i64::try_from(RETENTION.as_millis()).unwrap_or(i64::MAX));
        self.accounts.store().conn().execute("DELETE FROM pending WHERE created_ms < ?1", [cutoff])?;
        Ok(())
    }

    /// Expires waiting actions every 30 s and deletes old ones daily, for the daemon's lifetime.
    pub async fn sweep(self: Arc<Self>) {
        let mut expiry = tokio::time::interval(EXPIRY_SWEEP);
        let mut retention = tokio::time::interval(RETENTION_SWEEP);
        loop {
            // ponytail: a failed sweep is tried again on the next tick; reported once a log sink exists.
            tokio::select! {
                _ = expiry.tick() => { let _ = self.expire_due(); }
                _ = retention.tick() => { let _ = self.delete_old(); }
            }
        }
    }
}

/// An approved action as stored.
struct Row {
    account: String,
    conversation: String,
    kind: String,
    /// The module tool's arguments, JSON.
    args: String,
    /// Empty for an autonomous action.
    fingerprint: String,
}

const VOIDED: &str = "the participants or the target message changed after the approval; nothing was sent";
const CHECK: &str = "it may have reached the platform - check the conversation before trying again";

fn failed(err: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(err.to_string())
}

fn too_many() -> ToolError {
    ToolError::NotAllowed("too many actions waiting for approval".into())
}

fn ttl_ms() -> i64 {
    i64::try_from(APPROVAL_TTL.as_millis()).unwrap_or(i64::MAX)
}

/// Milliseconds since 1970 as RFC 3339, UTC.
pub(crate) fn time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).unwrap_or_default()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

fn new_id() -> Result<String, ToolError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(failed)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn describe(accounts: &Accounts, account: &Account, conversation: &str, message: Option<&str>) -> Result<Target, ToolError> {
    let handle = module(accounts, account)?;
    within(&account.module, handle.describe_target(&account.module_account, conversation, message)).await
}

/// SHA-256 of the sorted participant ids and the target text: what an approval binds to.
fn fingerprint(target: &Target) -> String {
    let mut ids: Vec<&str> = target.participants.iter().map(|participant| participant.id.as_str()).collect();
    ids.sort_unstable();
    crate::modules::sha256_hex(json!([ids, target.target_text]).to_string().as_bytes())
}

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn preview(action: &str, kind: Kind, account: &Account, args: &Value, target: Target) -> Preview {
    let arg = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned);
    Preview {
        action: action.into(),
        kind,
        account: format!("{} ({})", account.display_name, account.id),
        conversation_title: cut(&target.conversation_title, MAX_NAME),
        participants: target.participants.iter().map(|participant| cut(&participant.display_name, MAX_NAME)).collect(),
        text: arg("text"),
        reaction: arg("reaction"),
        target_cut: target.target_text.as_deref().is_some_and(|text| text.chars().count() > MAX_TEXT),
        target_text: target.target_text.as_deref().map(|text| cut(text, MAX_TEXT)),
    }
}
