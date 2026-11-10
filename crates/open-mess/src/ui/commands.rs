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
use super::sign_in::Active;
use crate::accounts::Account;
use crate::i18n::{self, Lang};
use crate::modules::{self, ApprovalReason, ModuleInfo, ModuleState};
use crate::recording::archive::{self, Gap};
use crate::recording::captions::{self, Archiver};
use crate::recording::recorder::{By, Recorder};
use crate::recording::{Entry, Meta, Reason, Source, Speaker, State, valid_id};
use crate::triggers::RuleCounters;
use crate::triggers::rules::{self, Rule};
use crate::update::release::Version;
use crate::update::service::{UpdateService, UpdateView};
use crate::{approval, cli, permissions};

/// Why an approval is refused when the file on disk is not the one the window showed.
pub fn module_changed() -> &'static str {
    i18n::pick("plik modułu się zmienił - odśwież", "the module file changed - refresh")
}

/// Why a save from the window is refused while an update's new version is checked (spec
/// 2026-10-27-self-update, 7.2): a rollback would undo it without a word.
pub fn update_in_progress() -> &'static str {
    i18n::pick("aktualizacja w toku", "update in progress")
}

fn text(err: impl std::fmt::Display) -> String {
    err.to_string()
}

/// Refuses a change while an update's new version is checked.
fn changes_allowed(state: &UiState) -> Result<(), String> {
    if state.probation.get() { Err(update_in_progress().into()) } else { Ok(()) }
}

/// Refuses a change to the modules also while an installation stops the daemon: a module loaded
/// then is not among those phase A paused (spec 2026-10-27-self-update, 7.1).
fn module_changes_allowed(state: &UiState) -> Result<(), String> {
    changes_allowed(state)?;
    if state.update.as_ref().is_some_and(UpdateService::installing) { Err(update_in_progress().into()) } else { Ok(()) }
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
        .map(|info| format!("{} ({}): {}", info.name, info.file, describe(&info.state, i18n::current())))
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
    changes_allowed(state)?;
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
    /// The address ranges of the module's media connections; empty without media access.
    pub media: Vec<String>,
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
            (Some(new), Some(old)) => changes(&modules::canonical(new), &old, i18n::current()),
            _ => Vec::new(),
        };
        let takes_over = matches!(
            info.state,
            ModuleState::PendingReplacement | ModuleState::AwaitingApproval { reason: ApprovalReason::ManifestChanged | ApprovalReason::FileChanged }
        );
        let Manifest { hosts, sign_in_hosts, wam, extra_tools, media, .. } = info.manifest.clone().unwrap_or_else(|| empty_manifest(info));
        dtos.push(ModuleDto {
            name: info.name.clone(),
            file: info.file.clone(),
            version: info.version.clone(),
            platform: info.platform.clone(),
            status: status_code(&info.state),
            detail: describe(&info.state, i18n::current()),
            hosts,
            sign_in_hosts,
            media: media.map(|media| media.peers).unwrap_or_default(),
            wam: wam
                .map(|wam| WamDto { client_id: wam.client_id, scopes: wam.scopes.iter().map(|scope| scope_text(scope, i18n::current())).collect() }),
            tools: extra_tools.into_iter().map(|tool| ToolDto { name: tool.name, writes: tool.writes }).collect(),
            file_sha256: info.file_sha256.clone(),
            manifest_sha256: info.manifest_sha256.clone(),
            changes,
            takes_over,
        });
    }
    Ok(dtos)
}

/// A WAM scope as the approval shows it: a pattern says what it hands over (spec
/// 2026-10-28-meetings-transcripts, 5.2; spec 15 asks to name the trust given).
fn scope_text(scope: &str, lang: Lang) -> String {
    let what = match scope {
        "https://*.sharepoint.com/.default" => lang.pick(
            "token do dowolnej witryny SharePoint/OneDrive Twojej organizacji, z pełnymi uprawnieniami (także zapis i usuwanie plików, Sites.FullControl.All)",
            "a token for any SharePoint/OneDrive site of your organization, with full permissions (writing and deleting files too, Sites.FullControl.All)",
        ),
        _ if scope.contains('*') => lang.pick("token do dowolnej usługi pasującej do wzorca", "a token for any service that matches the pattern"),
        _ => return scope.to_string(),
    };
    format!("{scope} - {what}")
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
        media: None,
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

/// The state of a module in words, as its card shows it.
fn describe(state: &ModuleState, lang: Lang) -> String {
    match state {
        ModuleState::Active => lang.pick("działa", "running").into(),
        ModuleState::AwaitingApproval { reason: ApprovalReason::New } => {
            lang.pick("czeka na zatwierdzenie: nowy moduł", "awaiting approval: new module").into()
        }
        ModuleState::AwaitingApproval { reason: ApprovalReason::ManifestChanged } => {
            lang.pick("czeka na zatwierdzenie: zmieniony manifest", "awaiting approval: changed manifest").into()
        }
        ModuleState::AwaitingApproval { reason: ApprovalReason::FileChanged } => {
            lang.pick("czeka na zatwierdzenie: zmieniony plik, ten sam manifest", "awaiting approval: changed file, same manifest").into()
        }
        ModuleState::PendingReplacement => lang.pick("czeka, by zastąpić działającą wersję", "waiting to replace the running version").into(),
        ModuleState::Disabled => lang.pick("wyłączony", "turned off").into(),
        ModuleState::Crashed => lang.pick("zatrzymany po kilku awariach", "stopped after repeated crashes").into(),
        ModuleState::Invalid(why) => match lang {
            Lang::Pl => format!("to nie jest prawidłowy moduł: {why}"),
            Lang::En => format!("not a valid module: {why}"),
        },
    }
}

/// How manifest `new` differs from `old`, line by line; both as [`modules::canonical`] gives them.
/// Each language has templates of its own: Polish adjectives follow the noun's gender.
fn changes(new: &Value, old: &Value, lang: Lang) -> Vec<String> {
    let mut changes = Vec::new();
    for (key, what) in [("version", lang.pick("wersja", "version")), ("platform", lang.pick("platforma", "platform"))] {
        if field(new, key) != field(old, key) {
            changes.push(format!("{what}: {} -> {}", text_of(field(old, key)), text_of(field(new, key))));
        }
    }
    let (new_host, removed_host) = (lang.pick("nowy host", "new host"), lang.pick("usunięty host", "removed host"));
    list_changes(&mut changes, new_host, removed_host, field(new, "hosts"), field(old, "hosts"));
    let (new_host, removed_host) =
        (lang.pick("nowy host logowania", "new sign-in host"), lang.pick("usunięty host logowania", "removed sign-in host"));
    list_changes(&mut changes, new_host, removed_host, field(new, "sign_in_hosts"), field(old, "sign_in_hosts"));
    let (new_range, removed_range) = (lang.pick("nowy zakres mediów", "new media range"), lang.pick("usunięty zakres mediów", "removed media range"));
    list_changes(&mut changes, new_range, removed_range, field(field(new, "media"), "peers"), field(field(old, "media"), "peers"));
    if field(new, "wam") != field(old, "wam") {
        let wam = |manifest: &Value| match field(manifest, "wam") {
            wam @ Value::Object(_) => format!("{} ({})", text_of(field(wam, "client_id")), strings(field(wam, "scopes")).join(", ")),
            _ => lang.pick("brak", "none").to_owned(),
        };
        changes.push(format!("{}: {} -> {}", lang.pick("tożsamość WAM", "WAM identity"), wam(old), wam(new)));
    }
    let tools = |manifest: &Value| field(manifest, "extra_tools").as_array().cloned().unwrap_or_default();
    let (new_tools, old_tools) = (tools(new), tools(old));
    let name = |tool: &Value| text_of(field(tool, "name"));
    let kind = |tool: &Value| if field(tool, "writes") == &Value::Bool(true) { lang.pick("pisze", "writes") } else { lang.pick("czyta", "reads") };
    let (new_tool, changed_tool) = (lang.pick("nowe narzędzie", "new tool"), lang.pick("zmienione narzędzie", "changed tool"));
    for tool in &new_tools {
        match old_tools.iter().find(|old| name(old) == name(tool)) {
            None => changes.push(format!("{new_tool}: {} ({})", name(tool), kind(tool))),
            Some(old) if old != tool => changes.push(format!("{changed_tool}: {} ({})", name(tool), kind(tool))),
            Some(_) => {}
        }
    }
    let removed_tool = lang.pick("usunięte narzędzie", "removed tool");
    for tool in old_tools.iter().filter(|old| !new_tools.iter().any(|new| name(new) == name(old))) {
        changes.push(format!("{removed_tool}: {}", name(tool)));
    }
    if changes.is_empty() {
        changes.push(lang.pick("manifest bez zmian, inny plik", "same manifest, another file").into());
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

/// The items of list `new` that `old` lacks, as `added: item`, and the other way round.
fn list_changes(changes: &mut Vec<String>, added: &str, removed: &str, new: &Value, old: &Value) {
    let (new, old): (BTreeSet<String>, BTreeSet<String>) = (strings(new).into_iter().collect(), strings(old).into_iter().collect());
    changes.extend(new.difference(&old).map(|item| format!("{added}: {item}")));
    changes.extend(old.difference(&new).map(|item| format!("{removed}: {item}")));
}

/// Approves the module file the window showed, by the hashes it showed, and loads it (a module
/// stopped after repeated crashes starts again). Refused with [`module_changed`] when the file is
/// no longer that one. Once a file an update placed beside the module is the module, the approved
/// file it replaced is deleted, so that the module has one file (spec 2026-10-27-self-update, 7.4);
/// a file added by hand deletes nothing.
pub async fn approve_module(state: &UiState, name: &str, file_sha256: &str, manifest_sha256: &str) -> Result<(), String> {
    module_changes_allowed(state)?;
    let list = state.accounts.modules().list();
    let shown = list.iter().find(|info| info.name == name && info.file_sha256 == file_sha256 && info.manifest_sha256 == manifest_sha256);
    let Some((file, Some(manifest))) = shown.map(|info| (info.file.clone(), info.manifest.clone())) else {
        return Err(module_changed().into());
    };
    // The module's approved file now, when the one approved is an update's file beside it.
    let approved = |state: &ModuleState| matches!(state, ModuleState::Active | ModuleState::Disabled | ModuleState::Crashed);
    let replaced = list
        .iter()
        .filter(|_| placed_beside(&file))
        .find(|info| info.name == name && info.file != file && approved(&info.state))
        .map(|info| (info.file.clone(), info.file_sha256.clone()));
    let accounts = Arc::clone(&state.accounts);
    let shown_file = file.clone();
    let on_disk = tokio::task::spawn_blocking(move || accounts.modules().file_sha256(&shown_file)).await.map_err(text)?;
    if on_disk.as_deref() != Some(file_sha256) {
        return Err(module_changed().into());
    }
    {
        let store = state.accounts.store();
        modules::approve(store.conn(), name, file_sha256, manifest_sha256).map_err(text)?;
        modules::remember_approved(store.conn(), &manifest).map_err(text)?;
    }
    rescan(state, Some(name)).await?;
    let now = state.accounts.modules().list();
    let is_module = now.iter().any(|info| info.name == name && info.file == file && info.state != ModuleState::PendingReplacement);
    // Still the same file of the same module: nothing put in its place meanwhile is deleted.
    let unchanged = |(old, sha): &(String, String)| now.iter().any(|info| info.name == name && info.file == *old && info.file_sha256 == *sha);
    if let Some((old, _)) = replaced.filter(|replaced| is_module && unchanged(replaced))
        && std::fs::remove_file(state.accounts.data_dir().join("modules").join(old)).is_ok()
    {
        // ponytail: reads the folder once more so the window stops listing the deleted file; a
        // Modules method that drops one entry would save loading every module again.
        rescan(state, None).await?;
    }
    Ok(())
}

/// Whether `file` is named as an update places a module beside the one it replaces:
/// `<stem>-<version>.wasm` or `<stem>-<version>-<n>.wasm` (update/apply.rs).
fn placed_beside(file: &str) -> bool {
    let Some(stem) = file.strip_suffix(".wasm") else {
        return false;
    };
    let versioned = |stem: &str| stem.rsplit_once('-').is_some_and(|(name, version)| !name.is_empty() && Version::parse(version).is_ok());
    let numbered = stem.rsplit_once('-').filter(|(_, n)| !n.is_empty() && n.bytes().all(|byte| byte.is_ascii_digit()));
    versioned(stem) || numbered.is_some_and(|(stem, _)| versioned(stem))
}

/// Turns a module on or off, at once; turning it on also restarts it after repeated crashes.
pub async fn set_module_enabled(state: &UiState, name: &str, enabled: bool) -> Result<(), String> {
    module_changes_allowed(state)?;
    if !state.accounts.modules().list().iter().any(|info| info.name == name && info.manifest.is_some()) {
        return Err(match i18n::current() {
            Lang::Pl => format!("nie ma modułu {name}"),
            Lang::En => format!("no module {name}"),
        });
    }
    modules::set_enabled(&state.accounts.store(), name, enabled).map_err(text)?;
    rescan(state, enabled.then_some(name)).await
}

/// Reads the modules folder again (spec 8.3).
pub async fn rescan_modules(state: &UiState) -> Result<(), String> {
    module_changes_allowed(state)?;
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
        _ => Err(match i18n::current() {
            Lang::Pl => format!("nie ma konta {account}"),
            Lang::En => format!("no account {account}"),
        }),
    }
}

pub fn permissions(state: &UiState, account: &str) -> Result<PermissionsDto, String> {
    if account != permissions::LOCAL {
        known_account(state, account)?;
    }
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
    changes_allowed(state)?;
    let write = permissions::Write::parse(write).map_err(text)?;
    if account == permissions::LOCAL {
        // The local recordings are only read: no writing, no conversations of their own.
        if conversation.is_some() {
            return Err(i18n::pick("nagrania lokalne nie mają rozmów", "local recordings have no conversations").into());
        }
        return permissions::set(&state.accounts.store(), account, None, read, permissions::Write::Never).map_err(text);
    }
    known_account(state, account)?;
    let conversation = conversation.map(|id| id.trim().to_owned());
    if let Some(id) = &conversation
        && (id.is_empty() || id.len() > MAX_CONVERSATION || id.chars().any(char::is_control))
    {
        return Err(i18n::pick("podaj identyfikator rozmowy", "enter a conversation id").into());
    }
    permissions::set(&state.accounts.store(), account, conversation.as_deref(), read, write).map_err(text)
}

/// Removes a conversation's own permissions; the account's apply to it again.
pub fn clear_permission_override(state: &UiState, account: &str, conversation: &str) -> Result<(), String> {
    changes_allowed(state)?;
    permissions::clear_override(&state.accounts.store(), account, conversation).map_err(text)
}

/// A waiting action with everything its approval shows (spec 8.6), cleaned as the toasts clean it.
#[derive(Clone, Debug, Serialize)]
pub struct PreviewDto {
    pub action: String,
    /// `send`, `edit` or `react`, or a module write tool's full name.
    pub kind: String,
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
    /// A module write tool's description, from its approved manifest.
    pub description: Option<String>,
    /// A module write tool's arguments but `conversation`, each as `key: value`.
    pub arguments: Vec<String>,
}

/// Why an action whose target message could not be shown in full is not approved.
pub fn target_cut() -> &'static str {
    i18n::pick(
        "wiadomości, której dotyczy akcja, nie da się pokazać w całości - można ją tylko odrzucić",
        "the message this action is about can't be shown in full - it can only be rejected",
    )
}

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
            kind: preview.kind.name().to_string(),
            account: approval::clean(&preview.account),
            conversation: approval::clean(&preview.conversation_title),
            participants: preview.participants.iter().map(|name| approval::clean(name)).collect(),
            text: clean(&preview.text),
            reaction: clean(&preview.reaction),
            target_text: clean(&preview.target_text),
            target_cut: preview.target_cut,
            description: clean(&preview.description),
            arguments: preview.arguments.iter().map(|argument| approval::clean(argument)).collect(),
        })
        .collect()
}

/// The user's decision on one waiting action; an approved one runs before this returns. Returns
/// the action's state: `done`, `rejected`, `expired`, `voided`, `uncertain` or `failed`. Nothing
/// is approved that the window could not show in full.
pub async fn decide(state: &UiState, action: &str, approve: bool) -> Result<String, String> {
    changes_allowed(state)?;
    if approve && state.pending.waiting().iter().any(|preview| preview.action == action && preview.target_cut) {
        return Err(target_cut().into());
    }
    Ok(state.pending.decide(action, approve).await.map_err(text)?.name().into())
}

#[derive(Clone, Debug, Serialize)]
pub struct McpCommandsDto {
    /// `claude mcp add ...`, as `om mcp-command` prints it.
    pub claude: String,
    /// The `[mcp_servers.open_mess]` table for Codex's `config.toml`, the token in a fixed header,
    /// kept in Codex's own configuration as Claude Code keeps it in its own. `codex mcp add` takes a
    /// token only from an environment variable, which Codex hands on to the commands it runs (and
    /// setx to every program); a line typed into a shell also lands in its history and logs.
    pub codex: String,
}

/// The commands that connect Claude Code and Codex, with the local API token in them.
pub fn mcp_command(state: &UiState) -> McpCommandsDto {
    McpCommandsDto {
        claude: cli::mcp_add_command(state.port, &state.token),
        codex: format!(
            "[mcp_servers.open_mess]\nurl = \"http://127.0.0.1:{}/mcp\"\nhttp_headers = {{ Authorization = \"Bearer {}\" }}",
            state.port, state.token
        ),
    }
}

/// A module's sign-in prompt, as the window shows it.
#[derive(Clone, Debug, Serialize)]
pub struct SignInPromptDto {
    /// What [`sign_in_answer`] names.
    pub id: u64,
    /// `device-code`, `qr`, `page`, `wam-pick` or `ask-host` (the sign-in page went on to the host
    /// in `url`: may it?).
    pub kind: &'static str,
    pub module: String,
    pub code: Option<String>,
    /// Where the device code is entered; the host of a sign-in page or of the question.
    pub url: Option<String>,
    /// The QR code, as an SVG image.
    pub qr_svg: Option<String>,
    /// The Windows accounts to pick from.
    pub accounts: Vec<WamAccountDto>,
    /// The whole address of a sign-in page, for a front end without a webview to open in a
    /// browser: the open_mess plugin in Paperclip (spec 2026-11-03-paperclip-plugin, 5.4).
    pub open_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WamAccountDto {
    /// What the answer names.
    pub id: String,
    pub user_name: String,
    /// The organization's id, to tell apart one user name in several organizations.
    pub tenant: String,
}

/// The window's answer to a sign-in prompt: `{"done": value}` or `"cancelled"`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerDto {
    Done(Option<String>),
    Cancelled,
}

/// Signs in an account of `module` (`Dodaj konto`), or `account` again (`Zaloguj ponownie`), from
/// the window only (spec 8.8); the module's prompts reach the window meanwhile. Returns the account.
pub async fn sign_in_start(state: &UiState, module: &str, account: Option<&str>) -> Result<AccountDto, String> {
    let _active = sign_in_begin(state, module)?;
    sign_in_run(state, module, account).await
}

/// The first step of a sign-in, refused at once while an update or another sign-in runs. The
/// sign-in lasts as long as the returned guard; the administrative API keeps it in a task of its
/// own (spec 2026-11-03-paperclip-plugin, 5.4).
pub fn sign_in_begin(state: &UiState, module: &str) -> Result<Active, String> {
    changes_allowed(state)?;
    state.sign_ins.begin(module)
}

/// The second step: the module signs in, its prompts reaching the front end meanwhile.
pub async fn sign_in_run(state: &UiState, module: &str, account: Option<&str>) -> Result<AccountDto, String> {
    let account = state.accounts.sign_in(module, account).await.map_err(text)?;
    Ok(account_dto(state, &state.accounts.modules().list(), account))
}

/// The sign-in prompt waiting for the user, if any.
pub fn sign_in_request(state: &UiState) -> Option<SignInPromptDto> {
    let (id, request) = state.sign_ins.shown()?;
    let module = request.module().to_owned();
    // The sign-in page went on to a host that is not approved: the window asks first.
    if let Some((id, host)) = state.sign_ins.question() {
        return Some(SignInPromptDto {
            id,
            kind: "ask-host",
            module,
            code: None,
            url: Some(host),
            qr_svg: None,
            accounts: Vec::new(),
            open_url: None,
        });
    }
    let dto = |kind| SignInPromptDto { id, kind, module, code: None, url: None, qr_svg: None, accounts: Vec::new(), open_url: None };
    Some(match request {
        SignInRequest::DeviceCode { code, url, .. } => SignInPromptDto { code: Some(code), url: Some(url), ..dto("device-code") },
        SignInRequest::Qr { data, .. } => SignInPromptDto { qr_svg: qr_svg(&data), ..dto("qr") },
        SignInRequest::OpenPage { url, .. } => SignInPromptDto { url: Some(host_of(&url)), open_url: Some(url), ..dto("page") },
        SignInRequest::WamPick { accounts, .. } => SignInPromptDto {
            accounts: accounts
                .into_iter()
                .map(|account| WamAccountDto { id: account.id, user_name: account.user_name, tenant: account.tenant })
                .collect(),
            ..dto("wam-pick")
        },
        // Answered by the daemon, never shown.
        SignInRequest::WamWindow { .. } => return None,
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

/// The user's answer to sign-in prompt `id`. On Windows a sign-in page is answered only by the page
/// itself (its redirect); the window can only cancel it. Elsewhere there is no window: the answer
/// is the address the user's browser ended on, pasted (spec 2026-11-03-paperclip-plugin, 5.4).
pub fn sign_in_answer(state: &UiState, id: u64, answer: AnswerDto) -> Result<(), String> {
    let page = match state.sign_ins.shown() {
        Some((shown, SignInRequest::OpenPage { redirect_prefix, .. })) if shown == id => Some(redirect_prefix),
        _ => None,
    };
    let reply = match (answer, page) {
        (AnswerDto::Cancelled, _) => SignInReply::Cancelled,
        (AnswerDto::Done(pasted), Some(redirect_prefix)) => SignInReply::Done(Some(pasted_page(pasted.as_deref(), &redirect_prefix)?)),
        (AnswerDto::Done(value), None) => SignInReply::Done(value),
    };
    state.sign_ins.answer(id, reply)
}

#[cfg(windows)]
fn pasted_page(_pasted: Option<&str>, _redirect_prefix: &str) -> Result<String, String> {
    Err(i18n::pick("stronę logowania można tylko anulować", "a sign-in page can only be cancelled").into())
}

#[cfg(not(windows))]
fn pasted_page(pasted: Option<&str>, redirect_prefix: &str) -> Result<String, String> {
    super::sign_in_page::check_pasted(pasted.unwrap_or_default(), redirect_prefix)
}

/// Opens the verification page of device-code prompt `id` in the default browser. Only that
/// page: the window opens nothing else.
pub fn sign_in_open_link(state: &UiState, id: u64) -> Result<(), String> {
    let url = match state.sign_ins.shown() {
        Some((shown, SignInRequest::DeviceCode { url, .. })) if shown == id => url,
        _ => return Err(super::sign_in::gone().into()),
    };
    if !url.starts_with("https://") || url.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"') {
        return Err(i18n::pick("moduł podał nieprawidłowy adres strony logowania", "the module gave an invalid sign-in page address").into());
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

/// A trigger rule as the window edits it (spec 8.7): the rule's own fields, flattened.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleDto {
    /// 1-64 characters, made by the window for a new rule.
    pub id: String,
    pub account: String,
    pub enabled: bool,
    #[serde(flatten)]
    pub rule: Rule,
}

/// A rule with what happened to its batches since the daemon started.
#[derive(Clone, Debug, Serialize)]
pub struct RuleViewDto {
    #[serde(flatten)]
    pub rule: RuleDto,
    pub counters: RuleCounters,
}

/// What the window shows when a rule wakes agents that may write on their own (spec 8.7). It quotes
/// the window's labels of the write permission and names its section, in either language.
pub fn autonomous_rule(lang: Lang) -> &'static str {
    lang.pick(
        "Agenci obudzeni przez tę regułę mogą wysyłać, poprawiać i reagować bez Twojej zgody, bo konto albo rozmowa, której dotyczy, ma pisanie \"samodzielnie\". Ustaw \"z potwierdzeniem\" w sekcji Uprawnienia agentów, jeśli mają pytać.",
        "Agents woken by this rule can send, edit and react without your approval, because the account or conversation it is about has writing set to \"autonomously\". Set \"with confirmation\" in the Agent permissions section if they should ask.",
    )
}

/// Every trigger rule, oldest first, with its counters.
pub fn rules(state: &UiState) -> Result<Vec<RuleViewDto>, String> {
    let rules = rules::list(&state.accounts.store()).map_err(text)?;
    Ok(rules
        .into_iter()
        .map(|rule| RuleViewDto {
            counters: state.counters.rule(&rule.id),
            rule: RuleDto { id: rule.id.clone(), account: rule.account.clone(), enabled: rule.enabled, rule },
        })
        .collect())
}

/// Saves a rule, new or changed, for an account open_mess has. Returns the warning about agents
/// that may write on their own; the rule is saved anyway.
pub fn put_rule(state: &UiState, dto: RuleDto) -> Result<Option<String>, String> {
    changes_allowed(state)?;
    known_account(state, &dto.account)?;
    let rule = Rule { id: dto.id, account: dto.account, enabled: dto.enabled, ..dto.rule };
    let warning = rules::put(&state.accounts.store(), &rule).map_err(text)?;
    Ok(warning.map(|_| autonomous_rule(i18n::current()).to_owned()))
}

pub fn delete_rule(state: &UiState, id: &str) -> Result<(), String> {
    changes_allowed(state)?;
    match rules::delete(&state.accounts.store(), id).map_err(text)? {
        true => Ok(()),
        false => Err(match i18n::current() {
            Lang::Pl => format!("nie ma reguły {id}"),
            Lang::En => format!("no rule {id}"),
        }),
    }
}

/// The program a spawn action starts, chosen in a file dialog: its full path, or None.
pub async fn pick_program() -> Result<Option<String>, String> {
    pick_file().await
}

#[cfg(windows)]
async fn pick_file() -> Result<Option<String>, String> {
    let lang = i18n::current();
    let picked = tokio::task::spawn_blocking(move || {
        rfd::FileDialog::new()
            .set_title(lang.pick("Program, który uruchomi reguła", "The program the rule starts"))
            .add_filter(lang.pick("Programy", "Programs"), &["exe", "cmd", "bat"])
            .add_filter(lang.pick("Wszystkie pliki", "All files"), &["*"])
            .pick_file()
    });
    Ok(picked.await.map_err(text)?.map(|path| path.display().to_string()))
}

#[cfg(not(windows))]
async fn pick_file() -> Result<Option<String>, String> {
    Err(crate::Error::NoWindow.to_string())
}

/// How to start a Claude Code session that takes trigger notifications (spec 8.7).
#[derive(Clone, Debug, Serialize)]
pub struct ChannelSetupDto {
    /// Where the MCP configuration below is saved.
    pub file: String,
    /// The MCP configuration with `om mcp-stdio`.
    pub json: String,
    /// The command that starts Claude Code with it.
    pub command: String,
    /// MCP sessions that take notifications now.
    pub sessions: usize,
}

/// The channel setup, with its MCP configuration saved in the data folder.
pub fn channel_setup(state: &UiState) -> Result<ChannelSetupDto, String> {
    let om = std::env::current_exe().map_err(text)?;
    // The bridge finds the daemon (its port, as stored) through the data folder: this daemon's.
    let data_dir = state.accounts.data_dir();
    let config = serde_json::json!({"mcpServers": {"open_mess_channel": {
        "command": om, "args": ["mcp-stdio"], "env": {"OPEN_MESS_DATA_DIR": data_dir},
    }}});
    let json = serde_json::to_string_pretty(&config).map_err(text)?;
    let file = data_dir.join(CHANNEL_CONFIG);
    if std::fs::read_to_string(&file).ok().as_deref() != Some(json.as_str()) {
        std::fs::write(&file, &json).map_err(text)?;
    }
    let command = format!("claude --mcp-config \"{}\" --dangerously-load-development-channels server:open_mess_channel", file.display());
    Ok(ChannelSetupDto { file: file.display().to_string(), json, command, sessions: state.sessions.len() })
}

/// The channel setup's MCP configuration, in the data folder.
pub const CHANNEL_CONFIG: &str = "channel-mcp.json";

/// The updates, where the daemon has them (spec 2026-10-27-self-update, 6). Consent and the option
/// to install on its own are set only here and in the toast: no tool or CLI command reaches them.
fn updates(state: &UiState) -> Result<&UpdateService, String> {
    state.update.as_ref().ok_or_else(|| i18n::pick("ten demon nie aktualizuje się sam", "this daemon does not update itself").to_owned())
}

pub fn update_status(state: &UiState) -> Result<UpdateView, String> {
    Ok(updates(state)?.view())
}

/// "Sprawdź teraz"; the window looks at [`update_status`] for the outcome.
pub fn check_update(state: &UiState) -> Result<(), String> {
    updates(state)?.check_now();
    Ok(())
}

/// "Zainstaluj": the user's consent; the installation goes on in the background.
pub fn install_update(state: &UiState) -> Result<(), String> {
    updates(state)?.install()
}

/// "Anuluj", during phase A or B.
pub fn cancel_update(state: &UiState) -> Result<(), String> {
    updates(state)?.cancel();
    Ok(())
}

/// The Whisper model, where the daemon records (spec 2026-10-28-local-transcription, 4.6). Its
/// download and removal are only here: no tool or CLI command reaches them.
fn models(state: &UiState) -> Result<&crate::recording::model::Models, String> {
    state.models.as_deref().ok_or_else(|| i18n::pick("nagrywanie rozmów działa tylko w Windows", "call recording works on Windows only").to_owned())
}

pub fn model_status(state: &UiState) -> Result<crate::recording::model::ModelView, String> {
    Ok(models(state)?.view())
}

/// "Pobierz model"; the window follows it in [`model_status`]. Only files under
/// `%LOCALAPPDATA%`, which an update's rollback does not touch, so allowed while one is checked.
pub fn download_model(state: &UiState) -> Result<(), String> {
    let source = crate::recording::model::source().map_err(text)?;
    models(state)?.start_download(source)
}

/// "Usuń model"; refused while a call is recorded or transcribed (4.6).
pub fn delete_model(state: &UiState) -> Result<(), String> {
    let delete = || models(state)?.delete();
    match &state.recorder {
        // Checked and deleted while no recording can start (Codex on db70d3c).
        Some(recorder) => recorder.without_work(delete).map_err(str::to_owned)?,
        None => delete(),
    }
}

/// What records the calls (spec 2026-10-28-local-transcription, 4.6).
fn recorder(state: &UiState) -> Result<&Arc<Recorder>, String> {
    state.recorder.as_ref().ok_or_else(|| i18n::pick("nagrywanie rozmów działa tylko w Windows", "call recording works on Windows only").to_owned())
}

fn unknown() -> String {
    i18n::pick("nieznane nagranie", "unknown recording").into()
}

/// One recording on the window's list.
#[derive(Clone, Debug, Serialize)]
pub struct RecordingDto {
    pub id: String,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub state: &'static str,
    pub progress: Option<u8>,
    pub reason: Option<Reason>,
    /// The audio is still there: a failed one can be tried again.
    pub audio: bool,
    /// A call's audio or its live captions; a captions recording's coverage in percent and gaps.
    pub source: Source,
    pub coverage: Option<u8>,
    pub gaps: Option<u32>,
}

/// The Nagrywanie rozmów section: its settings, the recording going on, `status`'s recording
/// object and the recordings, newest first; and the captions archive's switch, language and
/// `status` object (spec 2026-11-04-captions-archive, 4.1, 4.7), null without it.
#[derive(Clone, Debug, Serialize)]
pub struct RecordingsDto {
    pub auto: bool,
    pub microphone: bool,
    pub language: String,
    pub active: Option<String>,
    pub status: Value,
    pub recordings: Vec<RecordingDto>,
    pub captions_auto: bool,
    pub captions_language: String,
    pub captions: Value,
}

pub fn recordings(state: &UiState) -> Result<RecordingsDto, String> {
    let recorder = recorder(state)?;
    let setting = |key: &str| state.accounts.store().get_config(key).ok().flatten();
    let recordings = recorder
        .recordings()
        .list()
        .into_iter()
        .map(|Entry { id, meta }| RecordingDto {
            progress: recorder.queue().progress(&id),
            audio: recorder.recordings().has_audio(&id),
            start_ms: meta.start_ms,
            end_ms: meta.end_ms,
            state: meta.state.name(),
            reason: meta.reason,
            source: meta.source,
            coverage: meta.coverage,
            gaps: meta.gaps,
            id,
        })
        .collect();
    let archiver = state.captions.as_ref();
    Ok(RecordingsDto {
        auto: setting("recording.auto").as_deref() == Some("true"),
        microphone: setting("recording.mic").as_deref() != Some("false"),
        language: setting("recording.language").unwrap_or_else(|| "auto".into()),
        active: recorder.active(),
        status: recorder.status(),
        recordings,
        captions_auto: archiver.is_some_and(|archiver| archiver.auto()),
        captions_language: archiver.map_or_else(|| captions::DEFAULT_LANGUAGE.into(), |archiver| archiver.language()),
        captions: archiver.map_or(Value::Null, |archiver| archiver.status()),
    })
}

fn archiver(state: &UiState) -> Result<&Arc<Archiver>, String> {
    state.captions.as_ref().ok_or_else(|| i18n::pick("zapis napisów działa tylko w Windows", "keeping captions works on Windows only").to_owned())
}

/// "Zapisuj napisy rozmów Teams automatycznie" and "Język napisów, gdy rozmowa nie ma żadnego"
/// (spec 2026-11-04-captions-archive, 4.1): the owner's standing consent. Turning it off ends
/// every call kept, after a join running now.
pub async fn set_captions_settings(state: &UiState, auto: bool, language: &str) -> Result<(), String> {
    changes_allowed(state)?;
    let archiver = archiver(state)?;
    if !captions::valid_language(language) {
        return Err(i18n::pick("nieznany język", "unknown language").into());
    }
    archiver.set(auto, language, captions::now_ms()).await
}

/// One cue of a captions recording, as "Pokaż" shows it.
#[derive(Clone, Debug, Serialize)]
pub struct CaptionDto {
    pub at_ms: u64,
    pub speaker: String,
    pub own: bool,
    pub text: String,
}

/// A captions recording as "Pokaż" shows it: its cues, its gaps, the lines that could not be read,
/// and whether more cues are there than shown.
#[derive(Clone, Debug, Serialize)]
pub struct CaptionsDto {
    pub cues: Vec<CaptionDto>,
    pub gaps: Vec<Gap>,
    pub damaged: usize,
    pub more: bool,
}

/// The most cues "Pokaż" shows.
const SHOWN_CUES: usize = 20_000;

/// "Pokaż" of a captions recording, in any state, for the owner whatever agents may read (4.7). It
/// is other people's speech: the window puts it in as text, never as HTML.
pub fn show_captions(state: &UiState, id: &str) -> Result<CaptionsDto, String> {
    let recordings = recorder(state)?.recordings();
    let meta = if valid_id(id) { recordings.meta(id).map_err(|_| unknown())? } else { return Err(unknown()) };
    let dir = recordings.path(id).filter(|_| meta.source == Source::Captions).ok_or_else(unknown)?;
    let written = meta.state == State::Recording;
    let damaged_text = || i18n::pick("zapis jest uszkodzony", "the captions are damaged").to_owned();
    let (mut cues, mut damaged, mut more) = (Vec::new(), 0, false);
    archive::read_lines(&dir.join(archive::CAPTIONS), written, |cue: Option<archive::Cue>| {
        match cue {
            Some(_) if cues.len() == SHOWN_CUES => more = true,
            Some(cue) => cues.push(CaptionDto { at_ms: cue.at, speaker: cue.speaker, own: cue.own, text: cue.text }),
            None => damaged += 1,
        }
        true
    })
    .map_err(|_| damaged_text())?;
    let (gaps, gap_lines) = archive::Gaps::load(&dir, written).map_err(|_| damaged_text())?;
    Ok(CaptionsDto { cues, gaps: gaps.0, damaged: damaged + gap_lines, more })
}

/// "Zatrzymaj" of a captions recording in the window, or of every one: as the toast's (4.7).
pub async fn stop_captions(state: &UiState, id: Option<&str>) -> Result<(), String> {
    archiver(state)?.stop(|recording, _| id.is_none_or(|id| id == recording)).await;
    Ok(())
}

/// "Nagrywaj rozmowy Teams automatycznie", "Nagrywaj mój mikrofon" and the language (4.6). The
/// language is `auto` or a Whisper code; one Whisper does not know fails the transcription, not
/// the daemon. Turning the mode off ends a recording going on.
pub fn set_recording_settings(state: &UiState, auto: bool, microphone: bool, language: &str) -> Result<(), String> {
    changes_allowed(state)?;
    let recorder = recorder(state)?;
    if language != "auto" && !((2..=8).contains(&language.len()) && language.bytes().all(|b| b.is_ascii_lowercase())) {
        return Err(i18n::pick("nieznany język", "unknown language").into());
    }
    {
        let store = state.accounts.store();
        let flag = |on: bool| if on { "true" } else { "false" };
        store.set_config("recording.auto", flag(auto)).map_err(text)?;
        store.set_config("recording.mic", flag(microphone)).map_err(text)?;
        store.set_config("recording.language", language).map_err(text)?;
    }
    recorder.settings_changed();
    Ok(())
}

/// One piece of a transcript, as "Pokaż" shows it.
#[derive(Clone, Debug, Serialize)]
pub struct CueDto {
    pub start_ms: u64,
    pub speaker: Speaker,
    pub text: String,
}

/// "Pokaż": a done recording's transcript, for the owner, whatever agents may read (4.6). It is
/// other people's speech: the window puts it in as text, never as HTML.
pub fn show_transcript(state: &UiState, id: &str) -> Result<Vec<CueDto>, String> {
    let recorder = recorder(state)?;
    if !valid_id(id) {
        return Err(unknown());
    }
    let meta = recorder.recordings().meta(id).map_err(|_| unknown())?;
    if meta.state != State::Done {
        return Err(i18n::pick("transkrypcja nie jest gotowa", "the transcript is not ready").into());
    }
    let transcript =
        recorder.recordings().transcript(id).map_err(|_| i18n::pick("transkrypcja jest uszkodzona", "the transcript is damaged").to_owned())?;
    Ok(transcript.cues.into_iter().map(|cue| CueDto { start_ms: cue.start_ms, speaker: cue.speaker, text: cue.text }).collect())
}

/// "Usuń": the recording going on stops first, a transcription running stops first. A captions
/// recording going on stops as with "Zatrzymaj", and its call is not kept again (spec
/// 2026-11-04-captions-archive, 4.7).
pub async fn delete_recording(state: &UiState, id: &str) -> Result<(), String> {
    if !valid_id(id) {
        return Err(unknown());
    }
    delete_one(state, recorder(state)?, id).await.map_err(text)
}

async fn delete_one(state: &UiState, recorder: &Recorder, id: &str) -> std::io::Result<()> {
    match &state.captions {
        Some(archiver) if recorder.recordings().meta(id).is_ok_and(|meta| meta.source == Source::Captions) => archiver.delete(id).await,
        _ => recorder.delete(id),
    }
}

/// "Usuń wszystkie": a folder that cannot go (a file another program holds) stays, the rest go.
pub async fn delete_all_recordings(state: &UiState) -> Result<(), String> {
    let recorder = recorder(state)?;
    let mut failed = 0;
    for entry in recorder.recordings().list() {
        if delete_one(state, recorder, &entry.id).await.is_err() {
            failed += 1;
        }
    }
    if failed == 0 {
        return Ok(());
    }
    Err(match i18n::current() {
        Lang::Pl => format!("nie udało się usunąć nagrań: {failed} (plik trzyma inny program)"),
        Lang::En => format!("recordings not deleted: {failed} (another program holds a file)"),
    })
}

/// "Ponów": a failed recording whose audio is still there waits for its transcription again. An
/// interrupted one stays marked so.
pub fn retry_recording(state: &UiState, id: &str) -> Result<(), String> {
    let recorder = recorder(state)?;
    let recordings = recorder.recordings();
    let meta = if valid_id(id) { recordings.meta(id).map_err(|_| unknown())? } else { return Err(unknown()) };
    if meta.state != State::Failed || !recordings.has_audio(id) {
        return Err(i18n::pick("tego nagrania nie da się ponowić", "this recording cannot be retried").into());
    }
    let reason = meta.reason.filter(|reason| *reason == Reason::Interrupted);
    recordings.set_meta(id, &Meta { state: State::Waiting, reason, ..meta }).map_err(text)?;
    recorder.queue().wake();
    Ok(())
}

/// "Zatrzymaj" in the window: as the tray's and the toast's (4.2).
pub fn stop_recording(state: &UiState) -> Result<(), String> {
    recorder(state)?.stop(By::Owner, false, None);
    Ok(())
}

/// "Instaluj aktualizacje automatycznie".
pub fn set_auto_install(state: &UiState, on: bool) -> Result<(), String> {
    changes_allowed(state)?;
    updates(state)?.set_auto_install(on)
}

/// The window's language setting and the language the window shows now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LanguageDto {
    /// `auto`, `pl` or `en`.
    pub setting: &'static str,
    /// `pl` or `en`.
    pub lang: &'static str,
}

pub fn language(state: &UiState) -> LanguageDto {
    LanguageDto { setting: i18n::setting(&state.accounts.store()), lang: i18n::current().code() }
}

/// Saves the window's language (`auto`, `pl` or `en`) and settles it (spec 2026-10-30-ui-languages,
/// 4.4); returns it, and whether it changed. A window setting, not a tool: agents see no change.
/// Refused while a new version is checked, like every save: a rollback would undo it.
pub fn set_language(state: &UiState, setting: &str) -> Result<(Lang, bool), String> {
    if !matches!(setting, "auto" | "pl" | "en") {
        return Err(i18n::pick("nieznany język", "unknown language").into());
    }
    changes_allowed(state)?;
    let store = state.accounts.store();
    store.set_config(i18n::SETTING, setting).map_err(text)?;
    Ok(i18n::apply_language(&store))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_states_in_both_languages() {
        let states = [
            (ModuleState::Active, "działa", "running"),
            (ModuleState::AwaitingApproval { reason: ApprovalReason::New }, "czeka na zatwierdzenie: nowy moduł", "awaiting approval: new module"),
            (
                ModuleState::AwaitingApproval { reason: ApprovalReason::ManifestChanged },
                "czeka na zatwierdzenie: zmieniony manifest",
                "awaiting approval: changed manifest",
            ),
            (
                ModuleState::AwaitingApproval { reason: ApprovalReason::FileChanged },
                "czeka na zatwierdzenie: zmieniony plik, ten sam manifest",
                "awaiting approval: changed file, same manifest",
            ),
            (ModuleState::PendingReplacement, "czeka, by zastąpić działającą wersję", "waiting to replace the running version"),
            (ModuleState::Disabled, "wyłączony", "turned off"),
            (ModuleState::Crashed, "zatrzymany po kilku awariach", "stopped after repeated crashes"),
            (ModuleState::Invalid("no manifest".into()), "to nie jest prawidłowy moduł: no manifest", "not a valid module: no manifest"),
        ];
        for (state, pl, en) in states {
            assert_eq!((describe(&state, Lang::Pl).as_str(), describe(&state, Lang::En).as_str()), (pl, en));
        }
    }

    #[test]
    fn wam_scopes_in_both_languages() {
        let sharepoint = "https://*.sharepoint.com/.default";
        assert!(scope_text(sharepoint, Lang::Pl).starts_with(&format!("{sharepoint} - token do dowolnej witryny SharePoint/OneDrive")));
        assert!(scope_text(sharepoint, Lang::En).starts_with(&format!("{sharepoint} - a token for any SharePoint/OneDrive site")));
        assert!(scope_text(sharepoint, Lang::En).ends_with("Sites.FullControl.All)"));
        assert_eq!(
            scope_text("https://*.example.com/.default", Lang::En),
            "https://*.example.com/.default - a token for any service that matches the pattern"
        );
        assert_eq!(scope_text("https://graph.microsoft.com/.default", Lang::Pl), "https://graph.microsoft.com/.default");
    }

    #[test]
    fn manifest_changes_in_both_languages() {
        let old = serde_json::json!({
            "version": "1.0.0", "platform": "teams", "hosts": ["a.example", "b.example"], "sign_in_hosts": ["login.example"],
            "extra_tools": [{"name": "kept", "writes": false}, {"name": "changed", "writes": false}, {"name": "gone", "writes": true}],
        });
        let new = serde_json::json!({
            "version": "1.1.0", "platform": "teams", "hosts": ["a.example", "c.example"], "sign_in_hosts": [],
            "wam": {"client_id": "id", "scopes": ["s1", "s2"]},
            "extra_tools": [{"name": "kept", "writes": false}, {"name": "changed", "writes": true}, {"name": "added", "writes": false}],
        });
        assert_eq!(
            changes(&new, &old, Lang::Pl),
            [
                "wersja: 1.0.0 -> 1.1.0",
                "nowy host: c.example",
                "usunięty host: b.example",
                "usunięty host logowania: login.example",
                "tożsamość WAM: brak -> id (s1, s2)",
                "zmienione narzędzie: changed (pisze)",
                "nowe narzędzie: added (czyta)",
                "usunięte narzędzie: gone",
            ]
        );
        assert_eq!(
            changes(&new, &old, Lang::En),
            [
                "version: 1.0.0 -> 1.1.0",
                "new host: c.example",
                "removed host: b.example",
                "removed sign-in host: login.example",
                "WAM identity: none -> id (s1, s2)",
                "changed tool: changed (writes)",
                "new tool: added (reads)",
                "removed tool: gone",
            ]
        );
        let platform = serde_json::json!({"platform": "slack", "sign_in_hosts": ["login.example"]});
        let old_platform = serde_json::json!({"platform": "teams"});
        assert_eq!(changes(&platform, &old_platform, Lang::Pl), ["platforma: teams -> slack", "nowy host logowania: login.example"]);
        assert_eq!(changes(&platform, &old_platform, Lang::En), ["platform: teams -> slack", "new sign-in host: login.example"]);
        assert_eq!(changes(&old, &old, Lang::Pl), ["manifest bez zmian, inny plik"]);
        assert_eq!(changes(&old, &old, Lang::En), ["same manifest, another file"]);
    }

    #[test]
    fn autonomous_rule_quotes_the_window_in_its_language() {
        assert!(autonomous_rule(Lang::Pl).contains("\"samodzielnie\""));
        assert!(autonomous_rule(Lang::En).contains("\"autonomously\""));
    }

    #[test]
    fn a_media_range_is_a_change_to_approve() {
        let old = serde_json::json!({"hosts": []});
        let new = serde_json::json!({"hosts": [], "media": {"peers": ["52.112.0.0/14"]}});
        assert_eq!(changes(&new, &old, Lang::Pl), ["nowy zakres mediów: 52.112.0.0/14"]);
        assert_eq!(changes(&new, &old, Lang::En), ["new media range: 52.112.0.0/14"]);
        assert_eq!(changes(&old, &new, Lang::En), ["removed media range: 52.112.0.0/14"]);
    }

    #[test]
    fn only_an_update_s_names_are_placed_beside() {
        for file in ["teams-1.2.3.wasm", "teams-1.2.3-4.wasm", "open-mess-teams-10.0.1-100.wasm"] {
            assert!(placed_beside(file), "{file}");
        }
        for file in ["teams.wasm", "teams-dev.wasm", "-1.2.3.wasm", "teams-1.2.wasm", "teams-1.2.3-.wasm", "teams-1.2.3-x.wasm", "teams-1.2.3.exe"] {
            assert!(!placed_beside(file), "{file}");
        }
    }
}
