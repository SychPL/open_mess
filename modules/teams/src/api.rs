//! The module's way out: tokens from WAM, the sessions kept in memory, and requests through the
//! host's `http.fetch` (spec 9.1-9.3, docs/protocols/teams.md 1-2).

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ProtoError;
use crate::authsvc;
use crate::component::bindings::open_mess::module::types::{AccountInfo, ModuleError};
use crate::component::bindings::open_mess::module::{http, sign_in};
use crate::http::HttpRequest;
use crate::session::{self, AUTHSVC_SCOPE, Claims, Session};

thread_local! {
    /// Signed-in accounts by id. A module instance runs one call at a time, on one thread.
    static SESSIONS: RefCell<HashMap<String, Session>> = RefCell::new(HashMap::new());
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

pub(crate) fn proto(e: ProtoError) -> ModuleError {
    match e {
        ProtoError::Incompatible(path) => ModuleError::Incompatible(format!("unexpected server response at {path}")),
        ProtoError::Forbidden(name) => ModuleError::Platform(format!("open_mess never writes the property {name}")),
    }
}

/// A token for `scope` from WAM, and its claims. Only sign-in may ask the user.
fn wam(scope: &str, interactive: bool) -> Result<(String, Claims), ModuleError> {
    let token = sign_in::wam_token(&[scope.to_string()], interactive).map_err(|e| {
        ModuleError::AuthRequired(match e {
            sign_in::SignInError::Cancelled => "sign-in was cancelled".to_string(),
            sign_in::SignInError::TimedOut => "sign-in timed out".to_string(),
            sign_in::SignInError::Refused(why) | sign_in::SignInError::Blocked(why) => format!("Windows gave no token: {why}"),
        })
    })?;
    let claims = session::claims(&token).ok_or_else(|| ModuleError::Incompatible("the token from Windows names no account".to_string()))?;
    Ok((token, claims))
}

/// Signs in through WAM, interactively, and checks the account with Teams.
pub fn sign_in() -> Result<AccountInfo, ModuleError> {
    let (token, claims) = wam(AUTHSVC_SCOPE, true)?;
    let session = establish(token, claims)?;
    let info = AccountInfo { id: session.account.clone(), display_name: session.name.clone() };
    SESSIONS.with(|s| s.borrow_mut().insert(session.account.clone(), session));
    Ok(info)
}

/// A session for `account` again, with tokens WAM gives without asking.
pub fn restore(account: &str) -> Result<(), ModuleError> {
    let session = open(account)?;
    SESSIONS.with(|s| s.borrow_mut().insert(session.account.clone(), session));
    Ok(())
}

pub fn sign_out(account: &str) {
    SESSIONS.with(|s| s.borrow_mut().remove(account));
}

fn open(account: &str) -> Result<Session, ModuleError> {
    let (token, claims) = wam(AUTHSVC_SCOPE, false)?;
    if claims.oid != account {
        return Err(ModuleError::AuthRequired("Windows now signs in another account".to_string()));
    }
    establish(token, claims)
}

/// The authsvc exchange: Teams accepts the account and names its services (docs 2).
fn establish(token: String, claims: Claims) -> Result<Session, ModuleError> {
    let mut req = authsvc::request();
    req.headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    let response = fetch(&req)?;
    match response.status {
        200..=299 => {}
        401 | 403 => return Err(ModuleError::AuthRequired(format!("Teams refused the account (HTTP {})", response.status))),
        status => return Err(ModuleError::Platform(format!("Teams sign-in failed (HTTP {status})"))),
    }
    let auth = authsvc::decode(&response.body, now_ms()).map_err(proto)?;
    let expires = claims.expires_ms;
    let mut session = Session::new(claims, auth.directory);
    session.keep(AUTHSVC_SCOPE, token, expires);
    Ok(session)
}

fn fetch(req: &HttpRequest) -> Result<http::Response, ModuleError> {
    let request = http::Request { method: req.method.clone(), url: req.url.clone(), headers: req.headers.clone(), body: req.body.clone() };
    http::fetch(&request).map_err(|e| ModuleError::Platform(transport(&e)))
}

fn transport(e: &http::HttpError) -> String {
    match e {
        http::HttpError::Blocked(why) => format!("request blocked: {why}"),
        http::HttpError::Timeout => "the service did not answer in time".to_string(),
        http::HttpError::TooLarge => "the answer was too large".to_string(),
        http::HttpError::Throttled(secs) => format!("the service asks to wait {secs} s"),
        http::HttpError::Network(why) => format!("network error: {why}"),
    }
}
