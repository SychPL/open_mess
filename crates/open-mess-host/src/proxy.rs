//! The system proxy, for media connections (spec 2026-10-29-live-captions, 5.2). WebRTC cannot go
//! through an HTTP proxy, and a direct packet against the proxy rules would show the network the
//! user's address, so `rtc` sends nothing to an address the system would proxy. The answer comes
//! from WinHTTP, as for the Windows client: a PAC script (`AutoConfigURL`), WPAD auto-detection
//! and the static settings.

use std::net::SocketAddr;

use hyper_util::client::proxy::matcher::Matcher;

use crate::net::HttpError;

/// What proxy auto-configuration said for one address. Only Windows has auto-configuration to ask.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Auto {
    Direct,
    Proxy,
    /// Auto-detection found no script: a network without WPAD, the default Windows setup.
    NoScript,
    /// A script exists but could not be fetched or run.
    Failed,
}

/// Whether traffic to an address goes directly. A script that cannot be run fails closed; no
/// script falls back to the static settings, as Windows does.
fn goes_direct(auto: Option<Auto>, static_proxy: bool) -> bool {
    match auto {
        Some(Auto::Direct) => true,
        Some(Auto::Proxy | Auto::Failed) => false,
        Some(Auto::NoScript) | None => !static_proxy,
    }
}

/// What proxy settings that cannot be read mean: none set up when Windows has no settings at all;
/// any other error fails closed, since a PAC script it hides may require a proxy (Codex review of
/// b21b25d).
#[cfg_attr(not(windows), allow(dead_code))]
fn unreadable(no_settings: bool) -> Option<Auto> {
    if no_settings { None } else { Some(Auto::Failed) }
}

/// The address as a URL, the way the proxy rules are asked about it.
fn url(addr: SocketAddr) -> String {
    format!("https://{addr}/")
}

fn static_proxy(addr: SocketAddr) -> bool {
    url(addr).parse().is_ok_and(|uri| Matcher::from_system().intercept(&uri).is_some())
}

/// The addresses of `targets` that the system sends directly, not through a proxy.
pub(crate) async fn direct(targets: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, HttpError> {
    tokio::task::spawn_blocking(move || {
        let auto = auto_config(&targets);
        targets.iter().zip(auto).filter(|(addr, auto)| goes_direct(*auto, static_proxy(**addr))).map(|(addr, _)| *addr).collect()
    })
    .await
    .map_err(|e| HttpError::Network(format!("proxy lookup: {e}")))
}

#[cfg(not(windows))]
fn auto_config(targets: &[SocketAddr]) -> Vec<Option<Auto>> {
    vec![None; targets.len()]
}

/// WinHTTP's auto-configuration answer for each target; `None` where the user has none set up.
#[cfg(windows)]
fn auto_config(targets: &[SocketAddr]) -> Vec<Option<Auto>> {
    winhttp::auto_config(targets)
}

#[cfg(windows)]
mod winhttp {
    use std::net::SocketAddr;

    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, GlobalFree, HGLOBAL};
    use windows::Win32::Networking::WinHttp::{
        ERROR_WINHTTP_AUTODETECTION_FAILED, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_AUTO_DETECT_TYPE_DHCP, WINHTTP_AUTO_DETECT_TYPE_DNS_A,
        WINHTTP_AUTOPROXY_AUTO_DETECT, WINHTTP_AUTOPROXY_CONFIG_URL, WINHTTP_AUTOPROXY_OPTIONS, WINHTTP_CURRENT_USER_IE_PROXY_CONFIG,
        WINHTTP_PROXY_INFO, WinHttpCloseHandle, WinHttpGetIEProxyConfigForCurrentUser, WinHttpGetProxyForUrl, WinHttpOpen,
    };
    use windows::core::{BOOL, HRESULT, HSTRING, PCWSTR, PWSTR};

    use super::{Auto, unreadable, url};

    /// Frees a string WinHTTP allocated for the caller, which WinHTTP documents is freed with
    /// `GlobalFree` (a Windows API binding with no safe wrapper, AGENTS.md "Never crash").
    #[allow(unsafe_code)]
    fn free(text: PWSTR) {
        if !text.is_null() {
            unsafe { GlobalFree(Some(HGLOBAL(text.0.cast()))) }.ok();
        }
    }

    // Windows API bindings with no safe wrapper (AGENTS.md, "Never crash"): the user's proxy
    // settings, a WinHTTP session and `WinHttpGetProxyForUrl`, which runs the PAC script or WPAD
    // the way the Windows client does. Every string WinHTTP hands back is freed above; the session
    // handle is closed before returning.
    #[allow(unsafe_code)]
    pub(super) fn auto_config(targets: &[SocketAddr]) -> Vec<Option<Auto>> {
        let mut settings = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        let read = unsafe { WinHttpGetIEProxyConfigForCurrentUser(&mut settings) };
        let config_url = settings.lpszAutoConfigUrl;
        let detect = settings.fAutoDetect.as_bool();
        free(settings.lpszProxy);
        free(settings.lpszProxyBypass);
        if let Err(e) = read {
            free(config_url);
            return vec![unreadable(e.code() == ERROR_FILE_NOT_FOUND.to_hresult()); targets.len()];
        }
        if !detect && config_url.is_null() {
            return vec![None; targets.len()];
        }
        let agent = HSTRING::from("open_mess");
        let session = unsafe { WinHttpOpen(&agent, WINHTTP_ACCESS_TYPE_NO_PROXY, PCWSTR::null(), PCWSTR::null(), 0) };
        if session.is_null() {
            free(config_url);
            return vec![Some(Auto::Failed); targets.len()];
        }
        let mut options = WINHTTP_AUTOPROXY_OPTIONS {
            dwFlags: if config_url.is_null() { 0 } else { WINHTTP_AUTOPROXY_CONFIG_URL } | if detect { WINHTTP_AUTOPROXY_AUTO_DETECT } else { 0 },
            dwAutoDetectFlags: if detect { WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A } else { 0 },
            lpszAutoConfigUrl: PCWSTR(config_url.0),
            fAutoLogonIfChallenged: BOOL::from(true),
            ..Default::default()
        };
        let answers = targets
            .iter()
            .map(|addr| {
                let target = HSTRING::from(url(*addr));
                let mut info = WINHTTP_PROXY_INFO::default();
                let answer = match unsafe { WinHttpGetProxyForUrl(session, &target, &mut options, &mut info) } {
                    Ok(()) if info.dwAccessType == WINHTTP_ACCESS_TYPE_NO_PROXY || info.lpszProxy.is_null() => Auto::Direct,
                    Ok(()) => Auto::Proxy,
                    // Only auto-detection that found nothing means "no script"; a configured
                    // script that failed does not.
                    Err(e) if config_url.is_null() && e.code() == HRESULT::from_win32(ERROR_WINHTTP_AUTODETECTION_FAILED) => Auto::NoScript,
                    Err(_) => Auto::Failed,
                };
                free(info.lpszProxy);
                free(info.lpszProxyBypass);
                Some(answer)
            })
            .collect();
        unsafe { WinHttpCloseHandle(session) }.ok();
        free(config_url);
        answers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_that_cannot_run_refuses_and_a_missing_one_falls_back() {
        assert!(goes_direct(Some(Auto::Direct), true));
        assert!(!goes_direct(Some(Auto::Proxy), false));
        assert!(!goes_direct(Some(Auto::Failed), false));
        assert!(goes_direct(Some(Auto::NoScript), false));
        assert!(!goes_direct(Some(Auto::NoScript), true));
        assert!(goes_direct(None, false));
        assert!(!goes_direct(None, true));
    }

    #[test]
    fn settings_that_cannot_be_read_refuse_unless_there_are_none() {
        assert_eq!(unreadable(true), None);
        assert!(!goes_direct(unreadable(false), false));
    }

    #[test]
    fn addresses_are_asked_about_as_https_urls() {
        assert_eq!(url("52.113.1.2:3478".parse().unwrap()), "https://52.113.1.2:3478/");
        assert_eq!(url("[2603:1063::1]:3478".parse().unwrap()), "https://[2603:1063::1]:3478/");
    }
}
