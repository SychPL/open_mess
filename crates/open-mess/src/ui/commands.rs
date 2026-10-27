//! What the configuration window can do, as plain functions over the daemon's state (the Tauri
//! commands in window.rs only wrap them). None of this is a tool: setting permissions, deciding
//! waiting actions, approving modules, removing accounts and showing the local API token happen
//! only here and in the toasts, where the user is (spec 8.6, 8.8, 15).
//! Errors are messages for the window.

use std::collections::BTreeSet;
use std::sync::Arc;

use open_mess_host::{Manifest, SignInReply, SignInRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::UiState;
use crate::accounts::Account;
use crate::modules::{self, ApprovalReason, ModuleInfo, ModuleState};
use crate::{approval, cli, permissions};

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
    Ok(state.accounts.list().map_err(text)?.into_iter().map(|account| account_dto(state, &modules, account)).collect())
}

fn account_dto(state: &UiState, modules: &[ModuleInfo], account: Account) -> AccountDto {
    let platform = modules.iter().find(|info| info.name == account.module && info.state != ModuleState::PendingReplacement);
    AccountDto {
        platform: platform.map(|info| info.platform.clone()),
        state: state.accounts.effective_state(&account),
        id: account.id,
        module: account.module,
        display_name: account.display_name,
    }
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

pub fn modules(state: &UiState) -> Result<Vec<ModuleDto>, String> {
    let list = state.accounts.modules().list();
    let mut dtos = Vec::new();
    for info in &list {
        // What this file would replace: the version that runs, else the one approved last.
        let earlier =
            match list.iter().find(|other| other.name == info.name && other.state == ModuleState::Active).and_then(|other| other.manifest.as_ref()) {
                _ if info.state == ModuleState::Active || info.manifest.is_none() => None,
                Some(running) => Some(modules::canonical(running)),
                None => modules::approved_manifest(&state.accounts.store(), &info.name).map_err(text)?,
            };
        let changes = match (&info.manifest, earlier) {
            (Some(new), Some(old)) => changes(&modules::canonical(new), &old),
            _ => Vec::new(),
        };
        let takes_over = matches!(
            info.state,
            ModuleState::PendingReplacement | ModuleState::AwaitingApproval { reason: ApprovalReason::ManifestChanged | ApprovalReason::FileChanged }
        );
        let Manifest { hosts, sign_in_hosts, wam, extra_tools, .. } = info.manifest.clone().unwrap_or_else(|| empty_manifest(info));
        dtos.push(ModuleDto {
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
        });
    }
    Ok(dtos)
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

/// How manifest `new` differs from `old`, line by line; both as [`modules::canonical`] gives them.
fn changes(new: &Value, old: &Value) -> Vec<String> {
    let mut changes = Vec::new();
    for (key, what) in [("version", "wersja"), ("platform", "platforma")] {
        if field(new, key) != field(old, key) {
            changes.push(format!("{what}: {} -> {}", text_of(field(old, key)), text_of(field(new, key))));
        }
    }
    list_changes(&mut changes, "host", field(new, "hosts"), field(old, "hosts"));
    list_changes(&mut changes, "host logowania", field(new, "sign_in_hosts"), field(old, "sign_in_hosts"));
    if field(new, "wam") != field(old, "wam") {
        let wam = |manifest: &Value| match field(manifest, "wam") {
            wam @ Value::Object(_) => format!("{} ({})", text_of(field(wam, "client_id")), strings(field(wam, "scopes")).join(", ")),
            _ => "brak".to_string(),
        };
        changes.push(format!("tożsamość WAM: {} -> {}", wam(old), wam(new)));
    }
    let tools = |manifest: &Value| field(manifest, "extra_tools").as_array().cloned().unwrap_or_default();
    let (new_tools, old_tools) = (tools(new), tools(old));
    let name = |tool: &Value| text_of(field(tool, "name"));
    let kind = |tool: &Value| if field(tool, "writes") == &Value::Bool(true) { "pisze" } else { "czyta" };
    for tool in &new_tools {
        match old_tools.iter().find(|old| name(old) == name(tool)) {
            None => changes.push(format!("nowe narzędzie: {} ({})", name(tool), kind(tool))),
            Some(old) if old != tool => changes.push(format!("zmienione narzędzie: {} ({})", name(tool), kind(tool))),
            Some(_) => {}
        }
    }
    for tool in old_tools.iter().filter(|old| !new_tools.iter().any(|new| name(new) == name(old))) {
        changes.push(format!("usunięte narzędzie: {}", name(tool)));
    }
    if changes.is_empty() {
        changes.push("manifest bez zmian, inny plik".into());
    }
    changes
}

/// A field of a JSON object; null when it is missing.
fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}

fn text_of(value: &Value) -> String {
    value.as_str().map_or_else(|| value.to_string(), str::to_owned)
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().map(|items| items.iter().map(text_of).collect()).unwrap_or_default()
}

fn list_changes(changes: &mut Vec<String>, what: &str, new: &Value, old: &Value) {
    let (new, old): (BTreeSet<String>, BTreeSet<String>) = (strings(new).into_iter().collect(), strings(old).into_iter().collect());
    changes.extend(new.difference(&old).map(|item| format!("nowy {what}: {item}")));
    changes.extend(old.difference(&new).map(|item| format!("usunięty {what}: {item}")));
}

/// Approves the module file the window showed, by the hashes it showed, and loads it (a module
/// stopped after repeated crashes starts again). Refused with [`MODULE_CHANGED`] when the file is
/// no longer that one.
pub async fn approve_module(state: &UiState, name: &str, file_sha256: &str, manifest_sha256: &str) -> Result<(), String> {
    let shown = state
        .accounts
        .modules()
        .list()
        .into_iter()
        .find(|info| info.name == name && info.file_sha256 == file_sha256 && info.manifest_sha256 == manifest_sha256);
    let Some((file, Some(manifest))) = shown.map(|info| (info.file, info.manifest)) else {
        return Err(MODULE_CHANGED.into());
    };
    let accounts = Arc::clone(&state.accounts);
    let on_disk = tokio::task::spawn_blocking(move || accounts.modules().file_sha256(&file)).await.map_err(text)?;
    if on_disk.as_deref() != Some(file_sha256) {
        return Err(MODULE_CHANGED.into());
    }
    {
        let store = state.accounts.store();
        modules::approve(&store, name, file_sha256, manifest_sha256).map_err(text)?;
        modules::remember_approved(&store, &manifest).map_err(text)?;
    }
    rescan(state, Some(name)).await
}

/// Turns a module on or off, at once; turning it on also restarts it after repeated crashes.
pub async fn set_module_enabled(state: &UiState, name: &str, enabled: bool) -> Result<(), String> {
    if !state.accounts.modules().list().iter().any(|info| info.name == name && info.manifest.is_some()) {
        return Err(format!("nie ma modułu {name}"));
    }
    modules::set_enabled(&state.accounts.store(), name, enabled).map_err(text)?;
    rescan(state, enabled.then_some(name)).await
}

/// Reads the modules folder again (spec 8.3).
pub async fn rescan_modules(state: &UiState) -> Result<(), String> {
    rescan(state, None).await
}

async fn rescan(state: &UiState, restart: Option<&str>) -> Result<(), String> {
    state.accounts.rescan_modules(restart).await.map_err(text)?;
    // The rescan expired the actions of modules that changed; this closes their toasts.
    state.pending.expire_due().map_err(text)
}

#[derive(Clone, Debug, Serialize)]
pub struct OverrideDto {
    pub conversation: String,
    pub read: bool,
    /// `never`, `confirm` or `autonomous`.
    pub write: String,
}

/// An account's agent permissions and the conversations with their own (spec 8.6).
#[derive(Clone, Debug, Serialize)]
pub struct PermissionsDto {
    pub account: String,
    pub read: bool,
    pub write: String,
    pub overrides: Vec<OverrideDto>,
}

/// The longest conversation id taken, as the tools take it.
const MAX_CONVERSATION: usize = 1024;

fn known_account(state: &UiState, account: &str) -> Result<(), String> {
    match state.accounts.get(account).map_err(text)? {
        Some(found) if found.state != crate::accounts::REMOVING => Ok(()),
        _ => Err(format!("nie ma konta {account}")),
    }
}

pub fn permissions(state: &UiState, account: &str) -> Result<PermissionsDto, String> {
    known_account(state, account)?;
    let store = state.accounts.store();
    let own = permissions::effective(&store, account, None).map_err(text)?;
    let overrides = permissions::overrides(&store, account).map_err(text)?;
    Ok(PermissionsDto {
        account: account.into(),
        read: own.read,
        write: own.write.name().into(),
        overrides: overrides
            .into_iter()
            .map(|(conversation, set)| OverrideDto { conversation, read: set.read, write: set.write.name().into() })
            .collect(),
    })
}

/// Sets the permissions of an account (`conversation` None) or of one of its conversations.
pub fn set_permissions(state: &UiState, account: &str, conversation: Option<String>, read: bool, write: &str) -> Result<(), String> {
    known_account(state, account)?;
    let write = permissions::Write::parse(write).map_err(text)?;
    let conversation = conversation.map(|id| id.trim().to_owned());
    if let Some(id) = &conversation
        && (id.is_empty() || id.len() > MAX_CONVERSATION || id.chars().any(char::is_control))
    {
        return Err("podaj identyfikator rozmowy".into());
    }
    permissions::set(&state.accounts.store(), account, conversation.as_deref(), read, write).map_err(text)
}

/// Removes a conversation's own permissions; the account's apply to it again.
pub fn clear_permission_override(state: &UiState, account: &str, conversation: &str) -> Result<(), String> {
    permissions::clear_override(&state.accounts.store(), account, conversation).map_err(text)
}

/// A waiting action with everything its approval shows (spec 8.6), cleaned as the toasts clean it.
#[derive(Clone, Debug, Serialize)]
pub struct PreviewDto {
    pub action: String,
    /// `send`, `edit` or `react`.
    pub kind: &'static str,
    /// The account's name and id.
    pub account: String,
    pub conversation: String,
    /// Every participant; none for a channel.
    pub participants: Vec<String>,
    /// The text to send, or an edit's new text.
    pub text: Option<String>,
    pub reaction: Option<String>,
    /// The message replied to, edited or reacted to, as it is now.
    pub target_text: Option<String>,
    /// That message is longer than shown: the action can only be rejected.
    pub target_cut: bool,
}

/// Why an action whose target message could not be shown in full is not approved.
pub const TARGET_CUT: &str = "wiadomości, której dotyczy akcja, nie da się pokazać w całości - można ją tylko odrzucić";

/// The actions waiting for approval, oldest first, in full: the window is where long content can
/// be approved.
pub fn waiting(state: &UiState) -> Vec<PreviewDto> {
    let clean = |text: &Option<String>| text.as_deref().map(approval::clean);
    state
        .pending
        .waiting()
        .iter()
        .map(|preview| PreviewDto {
            action: preview.action.clone(),
            kind: preview.kind.name(),
            account: approval::clean(&preview.account),
            conversation: approval::clean(&preview.conversation_title),
            participants: preview.participants.iter().map(|name| approval::clean(name)).collect(),
            text: clean(&preview.text),
            reaction: clean(&preview.reaction),
            target_text: clean(&preview.target_text),
            target_cut: preview.target_cut,
        })
        .collect()
}

/// The user's decision on one waiting action; an approved one runs before this returns. Returns
/// the action's state: `done`, `rejected`, `expired`, `voided`, `uncertain` or `failed`. Nothing
/// is approved that the window could not show in full.
pub async fn decide(state: &UiState, action: &str, approve: bool) -> Result<String, String> {
    if approve && state.pending.waiting().iter().any(|preview| preview.action == action && preview.target_cut) {
        return Err(TARGET_CUT.into());
    }
    Ok(state.pending.decide(action, approve).await.map_err(text)?.name().into())
}

#[derive(Clone, Debug, Serialize)]
pub struct McpCommandsDto {
    /// `claude mcp add ...`, as `om mcp-command` prints it.
    pub claude: String,
    /// `codex mcp add ...`, which reads the token from `OPEN_MESS_TOKEN`, and the `setx` that sets
    /// it for the user.
    pub codex: String,
}

/// The commands that connect Claude Code and Codex, with the local API token in them.
pub fn mcp_command(state: &UiState) -> McpCommandsDto {
    McpCommandsDto {
        claude: cli::mcp_add_command(state.port, &state.token),
        codex: format!(
            "setx OPEN_MESS_TOKEN \"{}\"\ncodex mcp add open_mess --url http://127.0.0.1:{}/mcp --bearer-token-env-var OPEN_MESS_TOKEN",
            state.token, state.port
        ),
    }
}

/// A module's sign-in prompt, as the window shows it.
#[derive(Clone, Debug, Serialize)]
pub struct SignInPromptDto {
    /// What [`sign_in_answer`] names.
    pub id: u64,
    /// `device-code`, `qr` or `page`.
    pub kind: &'static str,
    pub module: String,
    pub code: Option<String>,
    /// Where the device code is entered; the host of a sign-in page.
    pub url: Option<String>,
    /// The QR code, as an SVG image.
    pub qr_svg: Option<String>,
}

/// The window's answer to a sign-in prompt: `{"done": value}` or `"cancelled"`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerDto {
    Done(Option<String>),
    Cancelled,
}

/// Signs in an account of `module`, from the window only (spec 8.8); the module's prompts reach
/// the window meanwhile. Returns the account added.
pub async fn sign_in_start(state: &UiState, module: &str) -> Result<AccountDto, String> {
    let handle = state.accounts.modules().handle(module).ok_or_else(|| format!("moduł {module} nie działa"))?;
    let _active = state.sign_ins.begin(module)?;
    let info = handle.sign_in().await.map_err(|err| format!("logowanie nie powiodło się: {err}"))?;
    let account = state.accounts.add(module, info).map_err(text)?;
    Ok(account_dto(state, &state.accounts.modules().list(), account))
}

/// The sign-in prompt waiting for the user, if any.
pub fn sign_in_request(state: &UiState) -> Option<SignInPromptDto> {
    let (id, request) = state.sign_ins.shown()?;
    let module = request.module().to_owned();
    let dto = |kind| SignInPromptDto { id, kind, module, code: None, url: None, qr_svg: None };
    Some(match request {
        SignInRequest::DeviceCode { code, url, .. } => SignInPromptDto { code: Some(code), url: Some(url), ..dto("device-code") },
        SignInRequest::Qr { data, .. } => SignInPromptDto { qr_svg: qr_svg(&data), ..dto("qr") },
        SignInRequest::OpenPage { url, .. } => SignInPromptDto { url: Some(host_of(&url)), ..dto("page") },
    })
}

/// The QR code of `data` as an SVG image; None when it does not fit in one.
fn qr_svg(data: &str) -> Option<String> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    Some(code.render::<qrcode::render::svg::Color<'_>>().min_dimensions(240, 240).build())
}

/// The host of a URL, for the window to name; the URL itself when it has none.
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest).to_owned()
}

/// The user's answer to sign-in prompt `id`. A sign-in page is answered only by the page itself
/// (its redirect); the window can only cancel it.
pub fn sign_in_answer(state: &UiState, id: u64, answer: AnswerDto) -> Result<(), String> {
    let reply = match answer {
        AnswerDto::Cancelled => SignInReply::Cancelled,
        AnswerDto::Done(_) if matches!(state.sign_ins.shown(), Some((shown, SignInRequest::OpenPage { .. })) if shown == id) => {
            return Err("stronę logowania można tylko anulować".into());
        }
        AnswerDto::Done(value) => SignInReply::Done(value),
    };
    state.sign_ins.answer(id, reply)
}

/// Opens the verification page of device-code prompt `id` in the default browser. Only that
/// page: the window opens nothing else.
pub fn sign_in_open_link(state: &UiState, id: u64) -> Result<(), String> {
    let url = match state.sign_ins.shown() {
        Some((shown, SignInRequest::DeviceCode { url, .. })) if shown == id => url,
        _ => return Err(super::sign_in::GONE.into()),
    };
    if !url.starts_with("https://") || url.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"') {
        return Err("moduł podał nieprawidłowy adres strony logowania".into());
    }
    open_in_browser(&url)
}

#[cfg(windows)]
fn open_in_browser(url: &str) -> Result<(), String> {
    // Explorer hands an https URL to the default browser; no shell parses it.
    std::process::Command::new("explorer.exe").arg(url).spawn().map(drop).map_err(text)
}

#[cfg(not(windows))]
fn open_in_browser(_url: &str) -> Result<(), String> {
    Err(crate::Error::NoWindow.to_string())
}
