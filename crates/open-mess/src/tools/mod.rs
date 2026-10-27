//! The tool registry shared by MCP and the CLI.
//!
//! Every capability is a tool here, with one name, one parameter schema and one result shape for
//! both front ends (AGENTS.md, "One tool registry"). The shared tools (`shared.rs`) run on the
//! module of the account they name; without an account, `list_conversations`, `unread` and
//! `mentions` ask every account at once. Read tools see only what the agent permissions allow
//! (spec 8.6): an account or conversation the agent may not read is refused when named and left
//! out of lists. The writes never reach a module from here: they become pending actions
//! (pending.rs), which run under the write permission. Every call passes the [`Gate`], which an
//! update closes (spec 7.1).

pub mod shared;

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use open_mess_host::limits::CALL_TIMEOUT;
use open_mess_host::{HostError, ModuleHandle};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinSet;

use crate::accounts::{Account, Accounts, REMOVING};
use crate::modules::{ModuleInfo, ModuleState};
use crate::pending::{Kind, Pending, State};
use crate::permissions;
use crate::triggers::Counters;
use crate::triggers::channel::Sessions;
use crate::triggers::engine::now_ms;

/// A tool as both front ends describe it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub input_schema: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    /// A module could not run the tool, or failed; the message is safe to show.
    #[error("{0}")]
    Module(String),
    /// The agent permissions (spec 8.6) refuse the call.
    #[error("{0}")]
    NotAllowed(String),
    /// The daemon itself failed (its database, a file).
    #[error("{0}")]
    Failed(String),
}

fn failed(err: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(err.to_string())
}

/// Which tool calls the [`Gate`] lets in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GateMode {
    #[default]
    Open,
    /// Reads only; the write tools fail.
    WritesBlocked,
    /// Every call fails.
    Closed,
}

/// Lets tool calls in or refuses them, and counts those running, for an update that waits for
/// them to end (spec 7.1).
#[derive(Clone, Default)]
pub struct Gate(Arc<GateState>);

#[derive(Default)]
struct GateState {
    mode: Mutex<GateMode>,
    in_flight: AtomicUsize,
    /// When the last call ended, in milliseconds since 1970; 0 before the first.
    last_end_ms: AtomicU64,
}

impl Gate {
    pub fn set(&self, mode: GateMode) {
        *self.0.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    /// Calls running now, from when the registry took them until it answered.
    pub fn in_flight(&self) -> usize {
        self.0.in_flight.load(Ordering::SeqCst)
    }

    pub fn last_end_ms(&self) -> u64 {
        self.0.last_end_ms.load(Ordering::SeqCst)
    }

    /// Takes a call of `tool`, counted while the guard lives, or refuses it.
    fn enter(&self, tool: &str) -> Result<InFlight, ToolError> {
        // Counted before the mode is read: whoever closes the gate and then sees no call running
        // knows that every later call sees it closed.
        self.0.in_flight.fetch_add(1, Ordering::SeqCst);
        let call = InFlight(Arc::clone(&self.0));
        match *self.0.mode.lock().unwrap_or_else(PoisonError::into_inner) {
            GateMode::Closed => Err(update_in_progress()),
            GateMode::WritesBlocked if Kind::for_tool(tool).is_some() => Err(update_in_progress()),
            _ => Ok(call),
        }
    }
}

/// A call the [`Gate`] took.
struct InFlight(Arc<GateState>);

impl Drop for InFlight {
    fn drop(&mut self) {
        // Before the count drops: whoever sees no call running sees when the last one ended.
        self.0.last_end_ms.store(now_ms(), Ordering::SeqCst);
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn update_in_progress() -> ToolError {
    ToolError::Failed("update in progress, try again shortly".into())
}

/// The tools of this daemon.
pub struct Registry {
    started: Instant,
    /// None when the daemon runs no modules and has no accounts.
    accounts: Option<Arc<Accounts>>,
    /// The accounts' writes; None with the accounts.
    pending: Option<Arc<Pending>>,
    /// What the trigger engine delivered and dropped, for `status`.
    triggers: Counters,
    /// The MCP sessions that take trigger notifications.
    sessions: Sessions,
    gate: Gate,
    /// Set by the daemon once the tray is up and once the accounts' sessions were restored, for
    /// `status`: an update checks them (spec 7.2).
    ui_ready: Arc<AtomicBool>,
    accounts_restored: Arc<AtomicBool>,
}

impl Registry {
    /// The tools with no modules and no accounts; `started` is when the daemon started, for
    /// `status`.
    pub fn with_builtin(started: Instant) -> Registry {
        Registry {
            started,
            accounts: None,
            pending: None,
            triggers: Counters::default(),
            sessions: Sessions::default(),
            gate: Gate::default(),
            ui_ready: Arc::default(),
            accounts_restored: Arc::default(),
        }
    }

    pub fn new(started: Instant, accounts: Arc<Accounts>) -> Registry {
        let pending = Arc::new(Pending::new(Arc::clone(&accounts)));
        Registry::with_pending(started, accounts, pending)
    }

    /// [`Registry::new`] with the accounts' pending actions made by the caller, who shares them
    /// with the approval toasts and the configuration window.
    pub fn with_pending(started: Instant, accounts: Arc<Accounts>, pending: Arc<Pending>) -> Registry {
        Registry { accounts: Some(accounts), pending: Some(pending), ..Registry::with_builtin(started) }
    }

    /// The trigger engine's counters, for the engine to count into.
    pub fn trigger_counters(&self) -> &Counters {
        &self.triggers
    }

    /// The MCP sessions that declared the channel capability, kept by the API.
    pub fn channel_sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// The pending actions, for the approval UI and the configuration window.
    pub fn pending(&self) -> Option<&Arc<Pending>> {
        self.pending.as_ref()
    }

    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    pub fn ui_ready(&self) -> &Arc<AtomicBool> {
        &self.ui_ready
    }

    pub fn accounts_restored(&self) -> &Arc<AtomicBool> {
        &self.accounts_restored
    }

    pub fn list(&self) -> Vec<ToolInfo> {
        let mut tools = vec![ToolInfo {
            name: "status".into(),
            description: "State of the open_mess daemon, its modules and accounts.".into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }];
        tools.extend(shared::tools());
        tools
    }

    /// Runs a tool. `args` is the arguments object.
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let _call = self.gate.enter(name)?;
        self.run(name, args).await
    }

    async fn run(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        if name == "status" {
            no_params(&args)?;
            return self.status();
        }
        let args = shared::validate(name, args)?;
        let (Some(accounts), Some(pending)) = (&self.accounts, &self.pending) else {
            return match args.account {
                Some(id) => Err(unknown_account(&id)),
                None => Ok(json!({"items": []})),
            };
        };
        if let Some(kind) = Kind::for_tool(name) {
            // The schema makes `account` and `conversation` required.
            let account = args.account.unwrap_or_default();
            let conversation = args.module_args.get("conversation").and_then(Value::as_str).unwrap_or_default().to_owned();
            let outcome = pending.create(kind, &account, &conversation, args.module_args).await?;
            return serde_json::to_value(outcome).map_err(failed);
        }
        match (name, args.account) {
            ("accounts", _) => list_accounts(accounts),
            ("pending", account) => {
                if let Some(id) = &account {
                    find(accounts, id)?;
                }
                let state = args.module_args.get("state").and_then(Value::as_str).map(State::parse).transpose().map_err(failed)?;
                let limit = args.module_args.get("limit").and_then(Value::as_u64).and_then(|limit| u32::try_from(limit).ok()).unwrap_or(20);
                pending.list(account.as_deref(), state, limit)
            }
            (_, None) => fan_out(accounts, name, &args.module_args).await,
            ("get_attachment", Some(id)) => get_attachment(accounts, &id, &args.module_args).await,
            (_, Some(id)) => {
                Ok(shared::with_notice(call_account(accounts, &readable(accounts, &id, &args.module_args)?, name, &args.module_args).await?))
            }
        }
    }

    /// What the `status` tool answers; the daemon's own check during an update's probation reads it
    /// too (spec 2026-10-27-self-update, 7.2).
    pub fn status(&self) -> Result<Value, ToolError> {
        let (modules, accounts) = match &self.accounts {
            None => (Vec::new(), Vec::new()),
            Some(accounts) => {
                let list = accounts.list().map_err(failed)?;
                let list = list.iter().map(|account| json!({"id": account.id, "module": account.module, "state": accounts.effective_state(account)}));
                (accounts.modules().list(), list.collect())
            }
        };
        // Known only for an active module: a stopped one has no instance to ask.
        let crashes = |module: &ModuleInfo| {
            let accounts = self.accounts.as_ref().filter(|_| module.state == ModuleState::Active)?;
            accounts.modules().handle(&module.name).map(|handle| handle.crashes_since_load())
        };
        let modules: Vec<Value> = modules
            .into_iter()
            .map(|module| {
                json!({
                    "name": module.name,
                    "file": module.file,
                    "version": module.version,
                    "platform": module.platform,
                    "state": module.state.to_string(),
                    "hosts": module.hosts,
                    "crashes_since_load": crashes(&module),
                })
            })
            .collect();
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": self.started.elapsed().as_secs(),
            "modules": modules,
            "accounts": accounts,
            "triggers": {"channel_sessions": self.sessions.len(), "counters": self.triggers.to_json()},
            "ui_ready": self.ui_ready.load(Ordering::SeqCst),
            "accounts_restored": self.accounts_restored.load(Ordering::SeqCst),
        }))
    }
}

fn no_params(args: &Value) -> Result<(), ToolError> {
    match args {
        Value::Object(map) => match map.keys().next() {
            None => Ok(()),
            Some(key) => Err(ToolError::InvalidArgs(format!("unknown parameter {key:?}"))),
        },
        _ => Err(ToolError::InvalidArgs("arguments must be a JSON object".into())),
    }
}

pub(crate) fn unknown_account(id: &str) -> ToolError {
    ToolError::InvalidArgs(format!("unknown account {id:?} - the accounts tool lists them"))
}

/// An account a tool may use: one being removed is unknown to the tools.
pub(crate) fn find(accounts: &Accounts, id: &str) -> Result<Account, ToolError> {
    accounts.get(id).map_err(failed)?.filter(|account| account.state != REMOVING).ok_or_else(|| unknown_account(id))
}

/// An account a read tool may use with `args`: the conversation they name, or anything of the
/// account when they name none (spec 8.6).
fn readable(accounts: &Accounts, id: &str, args: &Value) -> Result<Account, ToolError> {
    let account = find(accounts, id)?;
    let store = accounts.store();
    let (allowed, what) = match args.get("conversation").and_then(Value::as_str) {
        Some(conversation) => (permissions::effective(&store, &account.id, Some(conversation)).map_err(failed)?.read, "conversation"),
        None => (permissions::readable_anywhere(&store, &account.id).map_err(failed)?, "account"),
    };
    if allowed { Ok(account) } else { Err(ToolError::NotAllowed(format!("reading this {what} is not allowed"))) }
}

/// Drops the items of conversations the agent may not read.
fn drop_unreadable(accounts: &Accounts, account: &str, tool: &str, result: &mut Value) -> Result<(), ToolError> {
    let Some(items) = result.get_mut("items").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let key = if tool == "list_conversations" { "id" } else { "conversation" };
    let store = accounts.store();
    let mut kept = Vec::new();
    for item in std::mem::take(items) {
        if permissions::effective(&store, account, item.get(key).and_then(Value::as_str)).map_err(failed)?.read {
            kept.push(item);
        }
    }
    *items = kept;
    Ok(())
}

fn list_accounts(accounts: &Accounts) -> Result<Value, ToolError> {
    let modules = accounts.modules().list();
    let platform = |module: &str| {
        modules.iter().find(|info| info.name == module && info.state != ModuleState::PendingReplacement).map(|info| info.platform.clone())
    };
    let items: Vec<Value> = accounts
        .list()
        .map_err(failed)?
        .iter()
        .map(|account| {
            json!({
                "id": account.id,
                "module": account.module,
                "platform": platform(&account.module),
                "display_name": account.display_name,
                "state": accounts.effective_state(account),
            })
        })
        .collect();
    Ok(json!({"items": items}))
}

/// The account's running module, or an error naming the module's state.
pub(crate) fn module(accounts: &Accounts, account: &Account) -> Result<ModuleHandle, ToolError> {
    let modules = accounts.modules();
    modules.handle(&account.module).ok_or_else(|| {
        ToolError::Module(match modules.state(&account.module) {
            Some(state) => format!("module {} is {state}", account.module),
            None => format!("module {} is not installed", account.module),
        })
    })
}

/// A module call under the call limit, counted from now: a call waiting behind others on the same
/// module counts its wait too.
pub(crate) async fn within<T>(module: &str, call: impl Future<Output = Result<T, HostError>>) -> Result<T, ToolError> {
    match tokio::time::timeout(CALL_TIMEOUT, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(ToolError::Module(format!("module {module}: {err}"))),
        Err(_) => Err(ToolError::Module(format!("module {module}: {}", HostError::Timeout))),
    }
}

/// Runs a shared tool on one account's module: the result parsed, without what the agent may not
/// read, cut to the limit and labelled with the account, without the untrusted notice.
pub(crate) async fn call_account(accounts: &Accounts, account: &Account, tool: &str, args: &Value) -> Result<Value, ToolError> {
    let handle = module(accounts, account)?;
    let json = within(&account.module, handle.call(&account.module_account, tool, &args.to_string())).await?;
    let mut result = shared::parse(tool, &json)?;
    drop_unreadable(accounts, &account.id, tool, &mut result)?;
    if let Some(limit) = args.get("limit").and_then(Value::as_u64).and_then(|limit| usize::try_from(limit).ok()) {
        for key in ["items", "examples"] {
            if let Some(list) = result.get_mut(key).and_then(Value::as_array_mut) {
                list.truncate(limit);
            }
        }
    }
    for item in result.get_mut("items").and_then(Value::as_array_mut).into_iter().flatten().filter_map(Value::as_object_mut) {
        item.insert("account".into(), account.id.clone().into());
    }
    Ok(result)
}

/// Asks every account at once, each under the call limit; an account that fails or is not online
/// becomes an entry in `errors` and the others still answer. An account the agent may not read is
/// left out without an entry.
async fn fan_out(accounts: &Arc<Accounts>, tool: &str, args: &Value) -> Result<Value, ToolError> {
    let mut calls = JoinSet::new();
    let mut errors = Vec::new();
    let list = accounts.list().map_err(failed)?.into_iter().filter(|account| account.state != REMOVING);
    for (index, account) in list.enumerate() {
        if !permissions::readable_anywhere(&accounts.store(), &account.id).map_err(failed)? {
            continue;
        }
        if let Err(err) = module(accounts, &account) {
            errors.push((index, account.id, err.to_string()));
            continue;
        }
        if account.state != "online" {
            errors.push((index, account.id, format!("account is {}", account.state)));
            continue;
        }
        let (accounts, tool, args) = (Arc::clone(accounts), tool.to_string(), args.clone());
        calls.spawn(async move {
            let result = call_account(&accounts, &account, &tool, &args).await;
            (index, account.id, result)
        });
    }
    let mut answers = Vec::new();
    // A call task ends early only by panicking, which the lints rule out.
    while let Some(Ok((index, id, result))) = calls.join_next().await {
        match result {
            Ok(result) => answers.push((index, result)),
            Err(err) => errors.push((index, id, err.to_string())),
        }
    }
    answers.sort_by_key(|(index, _)| *index);
    errors.sort_by_key(|(index, ..)| *index);
    let items: Vec<Value> = answers
        .into_iter()
        .flat_map(|(_, mut result)| match result.get_mut("items").map(Value::take) {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        })
        .collect();
    let mut result = serde_json::Map::new();
    result.insert("items".into(), items.into());
    if !errors.is_empty() {
        result.insert("errors".into(), errors.into_iter().map(|(_, account, error)| json!({"account": account, "error": error})).collect());
    }
    Ok(shared::with_notice(result.into()))
}

/// The module fetches the bytes; the core writes the file (spec 8.5).
async fn get_attachment(accounts: &Arc<Accounts>, id: &str, args: &Value) -> Result<Value, ToolError> {
    let account = readable(accounts, id, args)?;
    let handle = module(accounts, &account)?;
    let arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    let attachment =
        within(&account.module, handle.get_attachment(&account.module_account, arg("conversation"), arg("message"), arg("attachment"))).await?;
    let accounts = Arc::clone(accounts);
    tokio::task::spawn_blocking(move || accounts.save_attachment(&account.id, attachment)).await.map_err(failed)?
}
