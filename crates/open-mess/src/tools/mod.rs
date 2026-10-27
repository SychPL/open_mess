//! The tool registry shared by MCP and the CLI.
//!
//! Every capability is a tool here, with one name, one parameter schema and one result shape for
//! both front ends (AGENTS.md, "One tool registry"). The shared tools (`shared.rs`) run on the
//! module of the account they name; without an account, `list_conversations`, `unread` and
//! `mentions` ask every account at once. Read tools see only what the agent permissions allow
//! (spec 8.6): an account or conversation the agent may not read is refused when named and left
//! out of lists. The writes never reach a module from here: they become pending actions
//! (pending.rs), which run under the write permission.

pub mod shared;

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use open_mess_host::limits::CALL_TIMEOUT;
use open_mess_host::{HostError, ModuleHandle};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinSet;

use crate::accounts::{Account, Accounts, REMOVING};
use crate::modules::ModuleState;
use crate::pending::{Kind, Pending, State};
use crate::permissions;

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

/// The tools of this daemon.
pub struct Registry {
    started: Instant,
    /// None when the daemon runs no modules and has no accounts.
    accounts: Option<Arc<Accounts>>,
    /// The accounts' writes; None with the accounts.
    pending: Option<Arc<Pending>>,
}

impl Registry {
    /// The tools with no modules and no accounts; `started` is when the daemon started, for
    /// `status`.
    pub fn with_builtin(started: Instant) -> Registry {
        Registry { started, accounts: None, pending: None }
    }

    pub fn new(started: Instant, accounts: Arc<Accounts>) -> Registry {
        let pending = Arc::new(Pending::new(Arc::clone(&accounts)));
        Registry::with_pending(started, accounts, pending)
    }

    /// [`Registry::new`] with the accounts' pending actions made by the caller, who shares them
    /// with the approval toasts and the configuration window.
    pub fn with_pending(started: Instant, accounts: Arc<Accounts>, pending: Arc<Pending>) -> Registry {
        Registry { started, accounts: Some(accounts), pending: Some(pending) }
    }

    /// The pending actions, for the approval UI and the configuration window.
    pub fn pending(&self) -> Option<&Arc<Pending>> {
        self.pending.as_ref()
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

    fn status(&self) -> Result<Value, ToolError> {
        let (modules, accounts) = match &self.accounts {
            None => (Vec::new(), Vec::new()),
            Some(accounts) => {
                let list = accounts.list().map_err(failed)?;
                let list = list.iter().map(|account| json!({"id": account.id, "module": account.module, "state": accounts.effective_state(account)}));
                (accounts.modules().list(), list.collect())
            }
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
                })
            })
            .collect();
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": self.started.elapsed().as_secs(),
            "modules": modules,
            "accounts": accounts,
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
    modules.handle(&account.module).cloned().ok_or_else(|| {
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
async fn call_account(accounts: &Accounts, account: &Account, tool: &str, args: &Value) -> Result<Value, ToolError> {
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
