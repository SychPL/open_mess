//! Windows account tokens through WAM, the Windows Web Account Manager (spec 9.1;
//! docs/protocols/teams.md, "Live results" item 1): the provider of work and school accounts, the
//! accounts Windows knows for a client id, and tokens for one of them, silently or through the WAM
//! dialog over the configuration window. Every call blocks: run it off the async threads. Errors
//! carry status and error codes only, never a token or an account name.

use crate::bindings::open_mess::module::sign_in::SignInError;

/// A Windows account a client id can use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WamAccount {
    /// WAM's id of the account; open_mess keeps it to ask for later tokens silently.
    pub id: String,
    /// What the user knows the account by (`name@company.example`).
    pub user_name: String,
    /// The id of the account's organization (Entra tenant), shown when one user name appears for
    /// several; empty when Windows does not say.
    pub tenant: String,
}

/// What a module asks Windows for: the manifest's client id, scopes, and the claims of a CAE
/// challenge (the JSON a 401 named), for a token that satisfies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TokenRequest {
    pub(crate) client_id: String,
    pub(crate) scopes: Vec<String>,
    pub(crate) claims: Option<String>,
}

/// Why a silent token request failed when only the WAM dialog can go on; the module may ask again
/// with `interactive`.
pub(crate) const INTERACTION_REQUIRED: &str = "interaction required";
/// The most accounts the window lists.
pub(crate) const MAX_ACCOUNTS: u32 = 32;

#[cfg(windows)]
pub(crate) use platform::{accounts, token_interactive, token_silent};

#[cfg(windows)]
mod platform {
    use windows::Security::Authentication::Web::Core::{
        FindAllWebAccountsStatus, WebAuthenticationCoreManager, WebTokenRequest, WebTokenRequestResult, WebTokenRequestStatus,
    };
    use windows::Security::Credentials::{WebAccount, WebAccountProvider};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::WinRT::IWebAuthenticationCoreManagerInterop;
    use windows::core::HSTRING;
    use windows_future::{AsyncStatus, IAsyncOperation};

    use std::time::{Duration, Instant};

    use super::{INTERACTION_REQUIRED, MAX_ACCOUNTS, SignInError, TokenRequest, WamAccount};

    /// The WAM provider of work and school accounts, as the live check found it.
    const PROVIDER: &str = "https://login.microsoft.com";
    const AUTHORITY: &str = "organizations";
    const AUTHORITY_URL: &str = "https://login.microsoftonline.com/organizations";
    /// How often a waiting dialog is looked at.
    const DIALOG_POLL: Duration = Duration::from_millis(200);

    fn failed(what: &str) -> impl Fn(windows::core::Error) -> SignInError {
        move |e| SignInError::Refused(format!("Windows account {what} failed ({})", e.code()))
    }

    fn provider() -> Result<WebAccountProvider, SignInError> {
        WebAuthenticationCoreManager::FindAccountProviderWithAuthorityAsync(&HSTRING::from(PROVIDER), &HSTRING::from(AUTHORITY))
            .and_then(|operation| operation.join())
            .map_err(failed("provider lookup"))
    }

    /// WAM's request for the client id and scopes, with the properties the live check needed, and
    /// the claims of a challenge.
    fn request(provider: &WebAccountProvider, asked: &TokenRequest) -> Result<WebTokenRequest, SignInError> {
        let build = || {
            let request = WebTokenRequest::Create(provider, &HSTRING::from(asked.scopes.join(" ")), &HSTRING::from(&asked.client_id))?;
            let properties = request.Properties()?;
            properties.Insert(&HSTRING::from("wam_compat"), &HSTRING::from("2.0"))?;
            properties.Insert(&HSTRING::from("authority"), &HSTRING::from(AUTHORITY_URL))?;
            if let Some(claims) = &asked.claims {
                properties.Insert(&HSTRING::from("claims"), &HSTRING::from(claims))?;
            }
            Ok(request)
        };
        build().map_err(failed("token request"))
    }

    fn account(account: &WebAccount) -> Result<WamAccount, SignInError> {
        // The organization tells apart one user's accounts in several tenants (as a guest, say).
        let tenant = account.Properties().and_then(|properties| properties.Lookup(&HSTRING::from("TenantId"))).map(|id| id.to_string());
        let read =
            || Ok(WamAccount { id: account.Id()?.to_string(), user_name: account.UserName()?.to_string(), tenant: tenant.unwrap_or_default() });
        read().map_err(failed("account"))
    }

    /// The token of a finished request, and the account it is for.
    fn token(result: &WebTokenRequestResult) -> Result<(String, WebAccount), SignInError> {
        let status = result.ResponseStatus().map_err(failed("token request"))?;
        if status == WebTokenRequestStatus::Success {
            let response = result.ResponseData().and_then(|data| data.GetAt(0)).map_err(failed("token response"))?;
            let token = response.Token().map_err(failed("token response"))?.to_string();
            return Ok((token, response.WebAccount().map_err(failed("token response"))?));
        }
        if status == WebTokenRequestStatus::UserInteractionRequired {
            return Err(SignInError::Refused(INTERACTION_REQUIRED.to_string()));
        }
        if status == WebTokenRequestStatus::UserCancel {
            return Err(SignInError::Cancelled);
        }
        let code = result.ResponseError().and_then(|error| error.ErrorCode()).unwrap_or_default();
        Err(SignInError::Refused(format!("Windows refused the token (status {}, error 0x{code:08x})", status.0)))
    }

    /// The work and school accounts Windows knows for `client_id`.
    pub(crate) fn accounts(client_id: &str) -> Result<Vec<WamAccount>, SignInError> {
        let provider = provider()?;
        let found = WebAuthenticationCoreManager::FindAllAccountsWithClientIdAsync(&provider, &HSTRING::from(client_id))
            .and_then(|operation| operation.join())
            .map_err(failed("account list"))?;
        let status = found.Status().map_err(failed("account list"))?;
        if status != FindAllWebAccountsStatus::Success {
            return Err(SignInError::Refused(format!("Windows did not list its accounts (status {})", status.0)));
        }
        let list = found.Accounts().map_err(failed("account list"))?;
        let count = list.Size().map_err(failed("account list"))?.min(MAX_ACCOUNTS);
        (0..count).map(|index| list.GetAt(index).map_err(failed("account list")).and_then(|found| account(&found))).collect()
    }

    /// A token for the account with WAM id `account_id`, without any dialog.
    pub(crate) fn token_silent(asked: &TokenRequest, account_id: &str) -> Result<String, SignInError> {
        let provider = provider()?;
        let account = WebAuthenticationCoreManager::FindAccountAsync(&provider, &HSTRING::from(account_id))
            .and_then(|operation| operation.join())
            .map_err(|_| SignInError::Refused("the Windows account is no longer there; sign in again".to_string()))?;
        let request = request(&provider, asked)?;
        let result = WebAuthenticationCoreManager::GetTokenSilentlyWithWebAccountAsync(&request, &account)
            .and_then(|operation| operation.join())
            .map_err(failed("token request"))?;
        token(&result).map(|(token, _)| token)
    }

    /// A token through the WAM dialog, owned by the configuration window `window`; the user picks
    /// or adds the account there. A dialog still open after `budget` is cancelled.
    pub(crate) fn token_interactive(window: isize, asked: &TokenRequest, budget: Duration) -> Result<(String, WamAccount), SignInError> {
        let provider = provider()?;
        let request = request(&provider, asked)?;
        let operation = request_for_window(HWND(window as *mut core::ffi::c_void), &request).map_err(failed("dialog"))?;
        let until = Instant::now() + budget;
        while operation.Status().map_err(failed("dialog"))? == AsyncStatus::Started {
            if Instant::now() >= until {
                let _ = operation.Cancel();
                return Err(SignInError::TimedOut);
            }
            std::thread::sleep(DIALOG_POLL);
        }
        let (token, found) = token(&operation.GetResults().map_err(failed("dialog"))?)?;
        Ok((token, account(&found)?))
    }

    /// `IWebAuthenticationCoreManagerInterop::RequestTokenForWindowAsync`, the WAM dialog owned by
    /// a window of this process. The windows crate has no safe wrapper for this interop call; it is
    /// the one unsafe call of the sign-in imports, listed in AGENTS.md.
    #[allow(unsafe_code)]
    fn request_for_window(window: HWND, request: &WebTokenRequest) -> windows::core::Result<IAsyncOperation<WebTokenRequestResult>> {
        let interop = windows::core::factory::<WebAuthenticationCoreManager, IWebAuthenticationCoreManagerInterop>()?;
        // SAFETY: `window` is the configuration window's handle, which the daemon gives while the
        // window is open; WAM only uses it as the owner of its dialog and fails on a stale one.
        // `request` is a live WebTokenRequest, and the result type is the one WAM documents.
        unsafe { interop.RequestTokenForWindowAsync(window, request) }
    }
}

#[cfg(not(windows))]
fn unsupported() -> SignInError {
    SignInError::Refused("Windows account sign-in exists only on Windows".to_string())
}

#[cfg(not(windows))]
pub(crate) fn accounts(_client_id: &str) -> Result<Vec<WamAccount>, SignInError> {
    Err(unsupported())
}

#[cfg(not(windows))]
pub(crate) fn token_silent(_asked: &TokenRequest, _account_id: &str) -> Result<String, SignInError> {
    Err(unsupported())
}

#[cfg(not(windows))]
pub(crate) fn token_interactive(_window: isize, _asked: &TokenRequest, _budget: std::time::Duration) -> Result<(String, WamAccount), SignInError> {
    Err(unsupported())
}
