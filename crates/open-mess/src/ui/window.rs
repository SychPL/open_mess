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
    let app = tauri::Builder::default().manage(state).build(tauri::generate_context!()).map_err(failed)?;
    tray(&app, profile.clone()).map_err(failed)?;

    let handle = app.handle().clone();
    runtime.spawn(async move {
        loop {
            opener.requested().await;
            let (app, profile) = (handle.clone(), profile.clone());
            // ponytail: a request the event loop refuses is lost; `om config` can be run again.
            let _ = handle.run_on_main_thread(move || open_window(&app, &profile));
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

fn tray(app: &tauri::App, profile: PathBuf) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Otwórz ustawienia", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Zakończ", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let on_click = profile.clone();
    let mut tray = TrayIconBuilder::with_id("open_mess")
        .tooltip("open_mess")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "open" => open_window(app, &profile),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                open_window(tray.app_handle(), &on_click);
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
fn open_window<R: tauri::Runtime>(app: &AppHandle<R>, profile: &Path) {
    if let Some(window) = app.get_webview_window(LABEL) {
        // ponytail: a window that cannot be raised stays where it is; reported once a log sink exists.
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("open_mess - ustawienia")
        .inner_size(960.0, 680.0)
        .min_inner_size(640.0, 420.0)
        .data_directory(profile.to_path_buf())
        .on_navigation(own_page)
        .build();
    // ponytail: a window that fails to open is reported once a log sink exists.
    drop(built);
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
        open_window(app.handle(), profile.path());
        open_window(app.handle(), profile.path());
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
