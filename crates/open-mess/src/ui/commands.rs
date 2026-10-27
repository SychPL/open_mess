//! What the configuration window can do, as plain functions over the daemon's state (the Tauri
//! commands in window.rs only wrap them). None of this is a tool: approving modules, removing
//! accounts and showing the local API token happen only here, where the user is (spec 8.8, 15).
//! Errors are messages for the window.

use std::collections::BTreeSet;
use std::sync::Arc;

use open_mess_host::Manifest;
use serde::Serialize;

use super::UiState;
use crate::cli;
use crate::modules::{self, ApprovalReason, ModuleInfo, ModuleState};

/// Why an approval is refused when the file on disk is not the one the window showed.
pub const MODULE_CHANGED: &str = "plik modułu się zmienił - odśwież";

fn text(err: impl std::fmt::Display) -> String {
    err.to_string()
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusDto {
    pub version: &'static str,
    pub uptime_seconds: u64,
    pub port: u16,
    pub data_dir: String,
    /// Where `om` is, for scripts.
    pub cli: String,
    pub accounts: usize,
    pub accounts_online: usize,
    pub modules: usize,
    pub modules_active: usize,
    pub waiting: usize,
    /// Modules that crashed or are not valid, in words.
    pub problems: Vec<String>,
}

pub fn status(state: &UiState) -> Result<StatusDto, String> {
    let accounts = accounts(state)?;
    let modules = state.accounts.modules().list();
    let problems = modules
        .iter()
        .filter(|info| matches!(info.state, ModuleState::Crashed | ModuleState::Invalid(_)))
        .map(|info| format!("{} ({}): {}", info.name, info.file, describe(&info.state)))
        .collect();
    Ok(StatusDto {
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: state.started.elapsed().as_secs(),
        port: state.port,
        data_dir: state.accounts.data_dir().display().to_string(),
        cli: std::env::current_exe().map(|exe| exe.display().to_string()).unwrap_or_default(),
        accounts_online: accounts.iter().filter(|account| account.state == "online").count(),
        accounts: accounts.len(),
        modules_active: modules.iter().filter(|info| info.state == ModuleState::Active).count(),
        modules: modules.len(),
        waiting: state.pending.waiting().len(),
        problems,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountDto {
    pub id: String,
    pub module: String,
    pub platform: Option<String>,
    pub display_name: String,
    /// As agents see it: `online`, `offline`, `auth-required` or `removing`.
    pub state: String,
}

pub fn accounts(state: &UiState) -> Result<Vec<AccountDto>, String> {
    let modules = state.accounts.modules().list();
    let platform = |module: &str| {
        modules.iter().find(|info| info.name == module && info.state != ModuleState::PendingReplacement).map(|info| info.platform.clone())
    };
    Ok(state
        .accounts
        .list()
        .map_err(text)?
        .into_iter()
        .map(|account| AccountDto {
            platform: platform(&account.module),
            state: state.accounts.effective_state(&account),
            id: account.id,
            module: account.module,
            display_name: account.display_name,
        })
        .collect())
}

/// Removes an account with its secrets, data and waiting actions (spec 8.8); their toasts close.
pub async fn remove_account(state: &UiState, id: &str) -> Result<(), String> {
    state.accounts.remove(id).await.map_err(text)?;
    // The removal deleted the actions; this forgets their previews and tells the toasts.
    state.pending.expire_due().map_err(text)
}

#[derive(Clone, Debug, Serialize)]
pub struct WamDto {
    pub client_id: String,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolDto {
    pub name: String,
    pub writes: bool,
}

/// A module file with everything its approval needs (spec 8.3).
#[derive(Clone, Debug, Serialize)]
pub struct ModuleDto {
    pub name: String,
    pub file: String,
    pub version: String,
    pub platform: String,
    /// `active`, `awaiting-approval`, `replacement`, `disabled`, `crashed` or `invalid`.
    pub status: &'static str,
    /// The state in words.
    pub detail: String,
    pub hosts: Vec<String>,
    pub sign_in_hosts: Vec<String>,
    pub wam: Option<WamDto>,
    /// The module's own tools.
    pub tools: Vec<ToolDto>,
    pub file_sha256: String,
    pub manifest_sha256: String,
    /// For a file waiting to replace the running version: how its manifest differs.
    pub changes: Vec<String>,
    /// Approving it hands an earlier version's accounts and secrets to this file.
    pub takes_over: bool,
}

pub fn modules(state: &UiState) -> Vec<ModuleDto> {
    let list = state.accounts.modules().list();
    let running =
        |name: &str| list.iter().find(|info| info.name == name && info.state == ModuleState::Active).and_then(|info| info.manifest.as_ref());
    list.iter()
        .map(|info| {
            let manifest = info.manifest.clone();
            let changes = match (&manifest, running(&info.name)) {
                (Some(new), Some(old)) if info.state != ModuleState::Active => changes(new, old),
                _ => Vec::new(),
            };
            let takes_over = matches!(
                info.state,
                ModuleState::PendingReplacement
                    | ModuleState::AwaitingApproval { reason: ApprovalReason::ManifestChanged | ApprovalReason::FileChanged }
            );
            let Manifest { hosts, sign_in_hosts, wam, extra_tools, .. } = manifest.unwrap_or_else(|| empty_manifest(info));
            ModuleDto {
                name: info.name.clone(),
                file: info.file.clone(),
                version: info.version.clone(),
                platform: info.platform.clone(),
                status: status_code(&info.state),
                detail: describe(&info.state),
                hosts,
                sign_in_hosts,
                wam: wam.map(|wam| WamDto { client_id: wam.client_id, scopes: wam.scopes }),
                tools: extra_tools.into_iter().map(|tool| ToolDto { name: tool.name, writes: tool.writes }).collect(),
                file_sha256: info.file_sha256.clone(),
                manifest_sha256: info.manifest_sha256.clone(),
                changes,
                takes_over,
            }
        })
        .collect()
}

fn empty_manifest(info: &ModuleInfo) -> Manifest {
    Manifest {
        name: info.name.clone(),
        version: String::new(),
        platform: String::new(),
        hosts: Vec::new(),
        sign_in_hosts: Vec::new(),
        wam: None,
        extra_tools: Vec::new(),
    }
}

fn status_code(state: &ModuleState) -> &'static str {
    match state {
        ModuleState::Active => "active",
        ModuleState::AwaitingApproval { .. } => "awaiting-approval",
        ModuleState::PendingReplacement => "replacement",
        ModuleState::Disabled => "disabled",
        ModuleState::Crashed => "crashed",
        ModuleState::Invalid(_) => "invalid",
    }
}

fn describe(state: &ModuleState) -> String {
    match state {
        ModuleState::Active => "działa".into(),
        ModuleState::AwaitingApproval { reason: ApprovalReason::New } => "czeka na zatwierdzenie: nowy moduł".into(),
        ModuleState::AwaitingApproval { reason: ApprovalReason::ManifestChanged } => "czeka na zatwierdzenie: zmieniony manifest".into(),
        ModuleState::AwaitingApproval { reason: ApprovalReason::FileChanged } => "czeka na zatwierdzenie: zmieniony plik, ten sam manifest".into(),
        ModuleState::PendingReplacement => "czeka, by zastąpić działającą wersję".into(),
        ModuleState::Disabled => "wyłączony".into(),
        ModuleState::Crashed => "zatrzymany po kilku awariach".into(),
        ModuleState::Invalid(why) => format!("to nie jest prawidłowy moduł: {why}"),
    }
}

/// How `new` differs from the running `old`, line by line.
fn changes(new: &Manifest, old: &Manifest) -> Vec<String> {
    let mut changes = Vec::new();
    if new.version != old.version {
        changes.push(format!("wersja: {} -> {}", old.version, new.version));
    }
    if new.platform != old.platform {
        changes.push(format!("platforma: {} -> {}", old.platform, new.platform));
    }
    list_changes(&mut changes, "host", &new.hosts, &old.hosts);
    list_changes(&mut changes, "host logowania", &new.sign_in_hosts, &old.sign_in_hosts);
    if new.wam != old.wam {
        let wam = |manifest: &Manifest| {
            manifest.wam.as_ref().map_or_else(|| "brak".to_string(), |wam| format!("{} ({})", wam.client_id, wam.scopes.join(", ")))
        };
        changes.push(format!("tożsamość WAM: {} -> {}", wam(old), wam(new)));
    }
    for tool in &new.extra_tools {
        match old.extra_tools.iter().find(|old| old.name == tool.name) {
            None => changes.push(format!("nowe narzędzie: {} ({})", tool.name, if tool.writes { "pisze" } else { "czyta" })),
            Some(old) if old != tool => changes.push(format!("zmienione narzędzie: {} ({})", tool.name, if tool.writes { "pisze" } else { "czyta" })),
            Some(_) => {}
        }
    }
    for tool in old.extra_tools.iter().filter(|old| !new.extra_tools.iter().any(|new| new.name == old.name)) {
        changes.push(format!("usunięte narzędzie: {}", tool.name));
    }
    if changes.is_empty() {
        changes.push("manifest bez zmian, inny plik".into());
    }
    changes
}

fn list_changes(changes: &mut Vec<String>, what: &str, new: &[String], old: &[String]) {
    let (new, old): (BTreeSet<&String>, BTreeSet<&String>) = (new.iter().collect(), old.iter().collect());
    changes.extend(new.difference(&old).map(|item| format!("nowy {what}: {item}")));
    changes.extend(old.difference(&new).map(|item| format!("usunięty {what}: {item}")));
}

/// Approves the module file the window showed, by the hashes it showed, and loads it. Refused
/// with [`MODULE_CHANGED`] when the file is no longer that one.
pub async fn approve_module(state: &UiState, name: &str, file_sha256: &str, manifest_sha256: &str) -> Result<(), String> {
    let shown = state
        .accounts
        .modules()
        .list()
        .into_iter()
        .find(|info| info.name == name && info.manifest.is_some() && info.file_sha256 == file_sha256 && info.manifest_sha256 == manifest_sha256);
    let Some(shown) = shown else {
        return Err(MODULE_CHANGED.into());
    };
    let accounts = Arc::clone(&state.accounts);
    let on_disk = tokio::task::spawn_blocking(move || accounts.modules().file_sha256(&shown.file)).await.map_err(text)?;
    if on_disk.as_deref() != Some(file_sha256) {
        return Err(MODULE_CHANGED.into());
    }
    modules::approve(&state.accounts.store(), name, file_sha256, manifest_sha256).map_err(text)?;
    rescan_modules(state).await
}

/// Turns a module on or off, at once.
pub async fn set_module_enabled(state: &UiState, name: &str, enabled: bool) -> Result<(), String> {
    if !state.accounts.modules().list().iter().any(|info| info.name == name && info.manifest.is_some()) {
        return Err(format!("nie ma modułu {name}"));
    }
    modules::set_enabled(&state.accounts.store(), name, enabled).map_err(text)?;
    rescan_modules(state).await
}

/// Reads the modules folder again; the actions waiting on a module that stopped or was replaced
/// expire (spec 8.3).
pub async fn rescan_modules(state: &UiState) -> Result<(), String> {
    for module in state.accounts.rescan_modules().await.map_err(text)? {
        state.pending.expire_module(&module).map_err(text)?;
    }
    Ok(())
}

/// The command that connects Claude Code, with the local API token in it.
pub fn mcp_command(state: &UiState) -> String {
    cli::mcp_add_command(state.port, &state.token)
}
