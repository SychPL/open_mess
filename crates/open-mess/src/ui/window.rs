//! The tray icon and the window, on Tauri (WebView2). Closing the window destroys it and its
//! webview; the tray icon and the daemon stay until `Zakończ`. The window loads only the embedded
//! `ui/` folder and runs only the commands `capabilities/main.json` grants it.

use std::future::Future;
use std::path::{Path, PathBuf};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, Url, WebviewUrl, WebviewWindowBuilder};
use tokio::runtime::Runtime;

use super::{Opener, UiState};
use crate::Error;

/// The window's label; the capability file grants its commands to this label only.
const LABEL: &str = "main";

pub(super) fn run(runtime: &Runtime, state: UiState, opener: Opener, quit: impl Future<Output = ()> + Send + 'static) -> Result<(), Error> {
    // Commands and Tauri's own tasks run on the daemon's runtime.
    tauri::async_runtime::set(runtime.handle().clone());
    // The webview's profile stays in the data folder (spec 8.4), not in a folder of its own.
    let profile = state.accounts.data_dir().join("webview");
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
            ipc::mcp_command
        ])
        .build(tauri::generate_context!())
        .map_err(failed)?;
    tray(&app, profile.clone()).map_err(failed)?;

    let handle = app.handle().clone();
    runtime.spawn(async move {
        while let Some(request) = opener.next().await {
            let (app, profile) = (handle.clone(), profile.clone());
            // A request the event loop drops tells its caller that the window is not available.
            let _ = handle.run_on_main_thread(move || {
                let _ = request.send(open_window(&app, &profile));
            });
        }
    });
    let handle = app.handle().clone();
    runtime.spawn(async move {
        quit.await;
        handle.exit(0);
    });

    app.run_return(|_, event| {
        // The last window closing asks to exit; only `Zakończ` and `quit` end the loop.
        if let RunEvent::ExitRequested { code: None, api, .. } = event {
            api.prevent_exit();
        }
    });
    Ok(())
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
    use crate::ui::commands::{self, AccountDto, ModuleDto, StatusDto};

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
        Ok(commands::modules(&state))
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
    pub async fn mcp_command(state: State<'_, UiState>) -> Result<String, String> {
        Ok(commands::mcp_command(&state))
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
            "open" => drop(open_window(app, &profile)),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                drop(open_window(tray.app_handle(), &on_click));
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
fn open_window<R: tauri::Runtime>(app: &AppHandle<R>, profile: &Path) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(LABEL) {
        // A window that cannot be raised is still open.
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("open_mess - ustawienia")
        .inner_size(960.0, 680.0)
        .min_inner_size(640.0, 420.0)
        .data_directory(profile.to_path_buf())
        .on_navigation(own_page)
        .build()
        .map(drop)
        .map_err(|err| format!("the window did not open: {err}"))
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
        open_window(app.handle(), profile.path()).unwrap();
        open_window(app.handle(), profile.path()).unwrap();
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
