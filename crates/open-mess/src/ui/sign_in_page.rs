//! A module's sign-in page in a window of its own (spec 8.3, R3 and R10): a webview with no IPC
//! (the capability file grants commands to the main window only) that loads only HTTPS pages on the
//! module's approved sign-in hosts, shows the current host in its title, and stops at the first
//! navigation that starts with the module's redirect prefix, whose URL becomes the module's answer.
//! A page that goes to another host waits for the user: yes adds the host to that module's approved
//! sign-in hosts, no ends the sign-in as refused.

use url::Url;

use super::sign_in::SignIns;
use crate::accounts::Accounts;
use crate::modules;
use open_mess_host::SignInReply;
use open_mess_host::hosts::HostList;

/// What the sign-in window does with a navigation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Navigation {
    Load,
    /// The redirect the module waits for: its URL is the answer, and the window closes.
    Redirect,
    /// An HTTPS page on a host that is not approved: the user decides.
    Ask(String),
    Block,
}

/// What the sign-in window does with a navigation to `url`, given the module's approved sign-in
/// hosts and its redirect prefix.
pub fn navigation(url: &Url, approved: &[String], redirect_prefix: &Url) -> Navigation {
    if url.as_str().starts_with(redirect_prefix.as_str()) {
        return Navigation::Redirect;
    }
    match url.host_str() {
        _ if url.scheme() != "https" => Navigation::Block,
        Some(host) if approved_list(approved).allows(host) => Navigation::Load,
        Some(host) => Navigation::Ask(host.to_owned()),
        None => Navigation::Block,
    }
}

fn approved_list(approved: &[String]) -> HostList {
    // The manifest's patterns were checked when the module loaded, the user's are host names.
    HostList::parse(approved).unwrap_or_default()
}

/// The start page and the redirect prefix of a sign-in page, if both are HTTPS URLs on approved
/// sign-in hosts; a module asks for nothing else (spec 8.3).
pub fn check_page(url: &str, redirect_prefix: &str, approved: &[String]) -> Result<(Url, Url), String> {
    let approved_https = |text: &str| {
        let url = Url::parse(text).ok().filter(|url| url.scheme() == "https")?;
        url.host_str().is_some_and(|host| approved_list(approved).allows(host)).then_some(url)
    };
    match (approved_https(url), approved_https(redirect_prefix)) {
        (Some(start), Some(prefix)) => Ok((start, prefix)),
        _ => Err("moduł poprosił o stronę logowania spoza zatwierdzonych hostów logowania".into()),
    }
}

/// The sign-in page of prompt `page` went on to `host`, which is not approved: the window asks.
/// Yes adds the host to the module's approved sign-in hosts and returns true; no (or the prompt
/// ending meanwhile) ends the sign-in as refused.
pub async fn unknown_host(sign_ins: &SignIns, accounts: &Accounts, page: u64, module: &str, host: &str) -> bool {
    if !sign_ins.ask_host(page, host).await {
        let _ = sign_ins.refuse(page, format!("the sign-in page went on to {host}, which the user did not allow"));
        return false;
    }
    match modules::allow_sign_in_host(&accounts.store(), module, host) {
        Ok(()) => true,
        Err(err) => {
            let _ = sign_ins.answer(page, SignInReply::Refused(err.to_string()));
            false
        }
    }
}

#[cfg(windows)]
pub(super) use window::{close, open, page_of};

#[cfg(windows)]
mod window {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use open_mess_host::SignInReply;
    use tauri::webview::NewWindowResponse;
    use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder};

    use super::{Navigation, check_page, navigation, unknown_host};
    use crate::modules;
    use crate::ui::UiState;

    /// The label of the sign-in window of prompt `page`. No capability names it, so its pages call
    /// no command.
    pub fn label(page: u64) -> String {
        format!("sign-in-{page}")
    }

    /// The prompt whose sign-in window has this label.
    pub fn page_of(label: &str) -> Option<u64> {
        label.strip_prefix("sign-in-")?.parse().ok()
    }

    /// Opens the sign-in page of prompt `page`; on the main thread. The window closes when the
    /// prompt ends (window.rs); the redirect, the user's answers and closing it end the prompt.
    pub fn open<R: tauri::Runtime>(
        app: &AppHandle<R>,
        profile: &Path,
        page: u64,
        module: &str,
        url: &str,
        redirect_prefix: &str,
    ) -> Result<(), String> {
        let state = app.try_state::<UiState>().ok_or("the window has no state")?;
        let manifest =
            state.accounts.modules().handle(module).and_then(|handle| handle.manifest().ok()).ok_or_else(|| format!("moduł {module} nie działa"))?;
        let approved = modules::approved_sign_in_hosts(&state.accounts.store(), &manifest).map_err(|err| err.to_string())?;
        let (start, prefix) = check_page(url, redirect_prefix, &approved)?;
        let approved = Arc::new(Mutex::new(approved));
        // One question at a time; navigations meanwhile go nowhere.
        let asking = Arc::new(AtomicBool::new(false));
        let (sign_ins, accounts, module, handle) = (state.sign_ins.clone(), Arc::clone(&state.accounts), module.to_owned(), app.clone());
        let on_navigation = move |url: &Url| {
            let current = approved.lock().unwrap_or_else(PoisonError::into_inner).clone();
            match navigation(url, &current, &prefix) {
                Navigation::Load => true,
                Navigation::Block => false,
                Navigation::Redirect => {
                    let _ = sign_ins.answer(page, SignInReply::Done(Some(url.to_string())));
                    false
                }
                Navigation::Ask(host) => {
                    if !asking.swap(true, Ordering::SeqCst) {
                        let (sign_ins, accounts, module, handle, approved, asking, url) = (
                            sign_ins.clone(),
                            Arc::clone(&accounts),
                            module.clone(),
                            handle.clone(),
                            Arc::clone(&approved),
                            Arc::clone(&asking),
                            url.clone(),
                        );
                        tauri::async_runtime::spawn(async move {
                            if unknown_host(&sign_ins, &accounts, page, &module, &host).await {
                                // ponytail: the page is loaded anew with GET, so a form POST to the
                                // new host is not repeated (the user sends it again); federated
                                // sign-in hands over with GET redirects.
                                approved.lock().unwrap_or_else(PoisonError::into_inner).push(host);
                                if let Some(window) = handle.get_webview_window(&label(page)) {
                                    let _ = window.navigate(url);
                                }
                            }
                            asking.store(false, Ordering::SeqCst);
                        });
                    }
                    false
                }
            }
        };
        let title = |url: &Url| format!("Logowanie - {}", url.host_str().unwrap_or_default());
        WebviewWindowBuilder::new(app, label(page), WebviewUrl::External(start.clone()))
            .title(title(&start))
            .inner_size(520.0, 720.0)
            .data_directory(profile.to_path_buf())
            .on_navigation(on_navigation)
            // The title always names the host of the page shown.
            .on_page_load(move |window, payload| {
                // A navigation stopped by the policy loads no page of a host (about:blank, say):
                // the title keeps the last host.
                if payload.url().host_str().is_some() {
                    let _ = window.set_title(&title(payload.url()));
                }
            })
            // No pop-ups and no downloads: they would escape the navigation policy.
            .on_new_window(|_, _| NewWindowResponse::Deny)
            .on_download(|_, _| false)
            .build()
            .map(drop)
            .map_err(|err| format!("okno logowania się nie otworzyło: {err}"))
    }

    /// Closes the sign-in window of prompt `page`, if it is open.
    pub fn close<R: tauri::Runtime>(app: &AppHandle<R>, page: u64) {
        if let Some(window) = app.get_webview_window(&label(page)) {
            let _ = window.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn approved() -> Vec<String> {
        vec!["login.microsoftonline.com".into(), "*.login.example.com".into()]
    }

    const PREFIX: &str = "https://login.microsoftonline.com/common/oauth2/nativeclient";

    fn go(to: &str) -> Navigation {
        navigation(&url(to), &approved(), &url(PREFIX))
    }

    #[test]
    fn only_approved_hosts_load() {
        assert_eq!(go("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?client_id=x"), Navigation::Load);
        assert_eq!(go("https://eu.login.example.com/start"), Navigation::Load);
        assert_eq!(go("https://login.microsoftonline.com.evil.example/"), Navigation::Ask("login.microsoftonline.com.evil.example".into()));
        assert_eq!(go("https://login.contoso-adfs.example/adfs/ls"), Navigation::Ask("login.contoso-adfs.example".into()));
        // The user's yes adds the host.
        let mut allowed = approved();
        allowed.push("login.contoso-adfs.example".into());
        assert_eq!(navigation(&url("https://login.contoso-adfs.example/adfs/ls"), &allowed, &url(PREFIX)), Navigation::Load);
    }

    #[test]
    fn redirect_prefix_returns_url() {
        assert_eq!(go("https://login.microsoftonline.com/common/oauth2/nativeclient?code=abc&state=1"), Navigation::Redirect);
        assert_eq!(go(PREFIX), Navigation::Redirect);
        // Only the prefix itself: a look-alike path is an ordinary page.
        assert_eq!(go("https://login.microsoftonline.com/common/oauth2/native"), Navigation::Load);
    }

    #[test]
    fn non_https_is_blocked() {
        for page in ["http://login.microsoftonline.com/", "file:///C:/Windows/win.ini", "javascript:alert(1)", "data:text/html,hi", "ms-settings:"] {
            assert_eq!(go(page), Navigation::Block, "{page}");
        }
    }

    #[test]
    fn module_asks_only_for_approved_pages() {
        let (start, prefix) = check_page("https://login.microsoftonline.com/common/oauth2/v2.0/authorize", PREFIX, &approved()).unwrap();
        assert_eq!((start.host_str(), prefix.as_str()), (Some("login.microsoftonline.com"), PREFIX));
        assert!(check_page("http://login.microsoftonline.com/", PREFIX, &approved()).is_err());
        assert!(check_page("https://evil.example/", PREFIX, &approved()).is_err());
        // A redirect prefix elsewhere would hand the module any URL on that host.
        assert!(check_page("https://login.microsoftonline.com/", "https://evil.example/", &approved()).is_err());
        assert!(check_page("https://login.microsoftonline.com/", "not a url", &approved()).is_err());
    }
}
