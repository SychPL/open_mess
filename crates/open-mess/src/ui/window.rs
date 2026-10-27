//! The tray icon and the window, on Tauri (WebView2). Closing the window destroys it and its
//! webview; the tray icon and the daemon stay until `Zakończ`. The window loads only the embedded
//! `ui/` folder and runs only the commands `capabilities/main.json` grants it.

use std::future::Future;
use std::path::{Path, PathBuf};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tokio::runtime::Runtime;

use open_mess_host::{SignInReply, SignInRequest};

use super::{Opener, UiState, sign_in_page};
use crate::Error;

/// The window's label; the capability file grants its commands to this label only.
const LABEL: &str = "main";

pub(super) fn run(runtime: &Runtime, state: UiState, opener: Opener, quit: impl Future<Output = ()> + Send + 'static) -> Result<(), Error> {
    // Commands and Tauri's own tasks run on the daemon's runtime.
    tauri::async_runtime::set(runtime.handle().clone());
    // The webview's profile stays in the data folder (spec 8.4), not in a folder of its own.
    let profile = state.accounts.data_dir().join("webview");
    let pending = std::sync::Arc::clone(&state.pending);
    let sign_ins = state.sign_ins.clone();
    let app = tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            ipc::status,
            ipc::accounts,
            ipc::remove_account,
            ipc::modules,
            ipc::approve_module,
            ipc::set_module_enabled,
            ipc::rescan_modules,
            ipc::mcp_command,
            ipc::permissions,
            ipc::set_permissions,
            ipc::clear_permission_override,
            ipc::waiting,
            ipc::decide,
            ipc::sign_in_start,
            ipc::sign_in_request,
            ipc::sign_in_answer,
            ipc::sign_in_open_link,
            ipc::rules,
            ipc::put_rule,
            ipc::delete_rule,
            ipc::pick_program,
            ipc::channel_setup
        ])
        .build(tauri::generate_context!())
        .map_err(failed)?;
    tray(&app, profile.clone()).map_err(failed)?;
    let page_profile = profile.clone();

    let handle = app.handle().clone();
    runtime.spawn(async move {
        while let Some(request) = opener.next().await {
            let (app, profile, waiting) = (handle.clone(), profile.clone(), opener.take_waiting());
            // A request the event loop drops tells its caller that the window is not available.
            let _ = handle.run_on_main_thread(move || {
                let _ = request.send(open_window(&app, &profile, waiting));
            });
        }
    });
    // The page's "pending-changed" and "sign-in-request" events, sent by evaluating a line in the
    // page, so the window needs no event permissions.
    let (handle, mut changed) = (app.handle().clone(), pending.subscribe());
    runtime.spawn(async move {
        while changed.changed().await.is_ok() {
            dispatch(&handle, "pending-changed");
        }
    });
    let (handle, mut prompts, watched) = (app.handle().clone(), sign_ins.subscribe(), sign_ins.clone());
    runtime.spawn(async move {
        // The prompt whose sign-in page has a window.
        let mut page_open = None;
        while prompts.changed().await.is_ok() {
            dispatch(&handle, "sign-in-request");
            let page = match watched.shown() {
                Some((id, SignInRequest::OpenPage { module, url, redirect_prefix })) => Some((id, module, url, redirect_prefix)),
                _ => None,
            };
            if page.as_ref().map(|(id, ..)| *id) == page_open {
                continue;
            }
            // The prompt changed: its page's window goes, the new one's opens.
            if let Some(old) = page_open.take() {
                let app = handle.clone();
                let _ = handle.run_on_main_thread(move || sign_in_page::close(&app, old));
            }
            if let Some((id, module, url, prefix)) = page {
                page_open = Some(id);
                let (app, profile, sign_ins) = (handle.clone(), page_profile.clone(), watched.clone());
                let _ = handle.run_on_main_thread(move || {
                    if let Err(why) = sign_in_page::open(&app, &profile, id, &module, &url, &prefix) {
                        let _ = sign_ins.answer(id, SignInReply::Refused(why));
                    }
                });
            }
        }
    });
    let handle = app.handle().clone();
    runtime.spawn(async move {
        quit.await;
        handle.exit(0);
    });

    app.run_return(move |_, event| match event {
        // The last window closing asks to exit; only `Zakończ` and `quit` end the loop.
        RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        // Nobody can answer what the closed window showed.
        RunEvent::WindowEvent { label, event: WindowEvent::Destroyed, .. } if label == LABEL => sign_ins.window_closed(),
        // The user closed a sign-in page: that sign-in is cancelled, if it still waits.
        RunEvent::WindowEvent { label, event: WindowEvent::Destroyed, .. } => {
            if let Some(page) = sign_in_page::page_of(&label) {
                sign_ins.page_closed(page);
            }
        }
        _ => {}
    });
    Ok(())
}

/// Sends an event to the page, if the window is open.
fn dispatch<R: tauri::Runtime>(app: &AppHandle<R>, event: &str) {
    if let Some(window) = app.get_webview_window(LABEL) {
        // ponytail: a page that misses one refreshes on the next change or when shown.
        let _ = window.eval(format!("window.dispatchEvent(new Event('{event}'))"));
    }
}

fn failed(err: tauri::Error) -> Error {
    Error::Ui(err.to_string())
}

/// The commands: commands.rs behind Tauri's IPC, with the arguments named as in Rust. Each one is
/// listed in build.rs and granted in capabilities/main.json.
mod ipc {
    // The glue `tauri::command` generates uses unreachable!; nothing written here does.
    #![allow(clippy::unreachable)]

    use tauri::State;

    use crate::ui::UiState;
    use crate::ui::commands::{
        self, AccountDto, AnswerDto, ChannelSetupDto, McpCommandsDto, ModuleDto, PermissionsDto, PreviewDto, RuleDto, RuleViewDto, SignInPromptDto,
        StatusDto,
    };

    #[tauri::command(rename_all = "snake_case")]
    pub async fn status(state: State<'_, UiState>) -> Result<StatusDto, String> {
        commands::status(&state)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn accounts(state: State<'_, UiState>) -> Result<Vec<AccountDto>, String> {
        commands::accounts(&state)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn remove_account(state: State<'_, UiState>, id: String) -> Result<(), String> {
        commands::remove_account(&state, &id).await
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn modules(state: State<'_, UiState>) -> Result<Vec<ModuleDto>, String> {
        commands::modules(&state)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn approve_module(state: State<'_, UiState>, name: String, file_sha256: String, manifest_sha256: String) -> Result<(), String> {
        commands::approve_module(&state, &name, &file_sha256, &manifest_sha256).await
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn set_module_enabled(state: State<'_, UiState>, name: String, enabled: bool) -> Result<(), String> {
        commands::set_module_enabled(&state, &name, enabled).await
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn rescan_modules(state: State<'_, UiState>) -> Result<(), String> {
        commands::rescan_modules(&state).await
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn mcp_command(state: State<'_, UiState>) -> Result<McpCommandsDto, String> {
        Ok(commands::mcp_command(&state))
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn permissions(state: State<'_, UiState>, account: String) -> Result<PermissionsDto, String> {
        commands::permissions(&state, &account)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn set_permissions(
        state: State<'_, UiState>,
        account: String,
        conversation: Option<String>,
        read: bool,
        write: String,
    ) -> Result<(), String> {
        commands::set_permissions(&state, &account, conversation, read, &write)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn clear_permission_override(state: State<'_, UiState>, account: String, conversation: String) -> Result<(), String> {
        commands::clear_permission_override(&state, &account, &conversation)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn waiting(state: State<'_, UiState>) -> Result<Vec<PreviewDto>, String> {
        Ok(commands::waiting(&state))
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn decide(state: State<'_, UiState>, action: String, approve: bool) -> Result<String, String> {
        commands::decide(&state, &action, approve).await
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn sign_in_start(state: State<'_, UiState>, module: String, account: Option<String>) -> Result<AccountDto, String> {
        commands::sign_in_start(&state, &module, account.as_deref()).await
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn sign_in_request(state: State<'_, UiState>) -> Result<Option<SignInPromptDto>, String> {
        Ok(commands::sign_in_request(&state))
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn sign_in_answer(state: State<'_, UiState>, request_id: u64, reply: AnswerDto) -> Result<(), String> {
        commands::sign_in_answer(&state, request_id, reply)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn sign_in_open_link(state: State<'_, UiState>, request_id: u64) -> Result<(), String> {
        commands::sign_in_open_link(&state, request_id)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn rules(state: State<'_, UiState>) -> Result<Vec<RuleViewDto>, String> {
        commands::rules(&state)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn put_rule(state: State<'_, UiState>, rule: RuleDto) -> Result<Option<String>, String> {
        commands::put_rule(&state, rule)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn delete_rule(state: State<'_, UiState>, id: String) -> Result<(), String> {
        commands::delete_rule(&state, &id)
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn pick_program() -> Result<Option<String>, String> {
        commands::pick_program().await
    }

    #[tauri::command(rename_all = "snake_case")]
    pub async fn channel_setup(state: State<'_, UiState>) -> Result<ChannelSetupDto, String> {
        commands::channel_setup(&state)
    }
}

fn tray(app: &tauri::App, profile: PathBuf) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Otwórz ustawienia", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Zakończ", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let on_click = profile.clone();
    let mut tray = TrayIconBuilder::with_id("open_mess")
        .tooltip("open_mess")
        .menu(&menu)
        .show_menu_on_left_click(false)
        // ponytail: a window the tray fails to open is reported once a log sink exists; `om config`
        // reports it now.
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "open" => drop(open_window(app, &profile, false)),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                drop(open_window(tray.app_handle(), &on_click, false));
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// Opens the window, or brings the open one to the front. On the main thread, so two requests
/// never make two windows.
/// Opens the window, or raises the open one; `waiting`: on the list of actions waiting for
/// approval. (A new window starts on that list anyway while any wait.)
fn open_window<R: tauri::Runtime>(app: &AppHandle<R>, profile: &Path, waiting: bool) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(LABEL) {
        // A window that cannot be raised is still open.
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        if waiting {
            dispatch(app, "show-waiting");
        }
        return Ok(());
    }
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("open_mess - ustawienia")
        .inner_size(960.0, 680.0)
        .min_inner_size(640.0, 420.0)
        .data_directory(profile.to_path_buf())
        .on_navigation(own_page)
        .build()
        .map_err(|err| format!("the window did not open: {err}"))?;
    // The WAM dialog of a sign-in opens over this window.
    if let (Some(state), Ok(hwnd)) = (app.try_state::<UiState>(), window.hwnd()) {
        state.sign_ins.window_opened(hwnd.0 as isize);
    }
    Ok(())
}

/// The embedded pages only: the window never goes to a remote address.
fn own_page(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => true,
        "http" | "https" => url.host_str() == Some("tauri.localhost"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_open_focuses() {
        let app = tauri::test::mock_app();
        let profile = tempfile::tempdir().unwrap();
        open_window(app.handle(), profile.path(), false).unwrap();
        // Raised on the list of waiting actions, as the notice that several wait asks.
        open_window(app.handle(), profile.path(), true).unwrap();
        assert_eq!(app.webview_windows().len(), 1);
        assert!(app.get_webview_window(LABEL).is_some());
    }

    #[test]
    fn window_stays_on_its_own_pages() {
        let page = |url: &str| own_page(&Url::parse(url).unwrap());
        assert!(page("http://tauri.localhost/index.html"));
        assert!(page("https://tauri.localhost/app.js"));
        assert!(page("tauri://localhost/index.html"));
        assert!(!page("https://login.microsoftonline.com/"));
        assert!(!page("http://tauri.localhost.example.com/"));
        assert!(!page("file:///C:/Windows/win.ini"));
    }
}
