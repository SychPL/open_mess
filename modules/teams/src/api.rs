//! The module's way out: tokens from WAM, the sessions kept in memory, and requests through the
//! host's `http.fetch` with the official client's retry policy, regional redirects and token
//! refresh (spec 9.1-9.3, docs/protocols/teams.md 1-3 and 8).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use url::Url;

use crate::ProtoError;
use crate::authsvc;
use crate::component::bindings::open_mess::module::host::{self, SessionState};
use crate::component::bindings::open_mess::module::types::{AccountInfo, ModuleError};
use crate::component::bindings::open_mess::module::{http, sign_in};
use crate::http::HttpRequest;
use crate::retry::{self, GiveUpReason, Retry};
use crate::session::{self, AUTHSVC_SCOPE, CHAT_SCOPE, Claims, SEARCH_SCOPE, Session};

/// The part of the host's call limit (30 s) the module plans with: no wait starts that would end
/// past it, so a slow service ends a call with an error, not a timeout that unloads the module.
const CALL_BUDGET: Duration = Duration::from_secs(20);
/// Regional redirects of one request (docs 3.1).
const MAX_REDIRECTS: u8 = 10;

thread_local! {
    /// Signed-in accounts by id. A module instance runs one call at a time, on one thread.
    static SESSIONS: RefCell<HashMap<String, Session>> = RefCell::new(HashMap::new());
    /// When the current call's budget ends.
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Starts a call's budget; every export calls it first.
pub fn begin() {
    DEADLINE.set(Some(Instant::now() + CALL_BUDGET));
}

/// Whether the call's budget has time left for more requests.
pub fn has_time() -> bool {
    DEADLINE.get().is_none_or(|deadline| Instant::now() < deadline)
}

/// Waits `ms` if that ends within the budget.
fn pause(ms: u32) -> bool {
    let wait = Duration::from_millis(u64::from(ms));
    if DEADLINE.get().is_some_and(|deadline| Instant::now() + wait > deadline) {
        return false;
    }
    std::thread::sleep(wait);
    true
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

/// The services the module calls, by token audience (docs 1.4).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Chat,
    Search,
    /// AMS images, with the chat service's token (docs 7; which token AMS takes is to be
    /// confirmed live).
    Media,
}

impl Service {
    fn scope(self) -> &'static str {
        match self {
            Service::Chat | Service::Media => CHAT_SCOPE,
            Service::Search => SEARCH_SCOPE,
        }
    }
}

/// 64 random bits from the host (`wasi:random` seeds the standard library's hasher keys, and each
/// `RandomState` takes new ones).
pub fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new().build_hasher().finish()
}

/// A random UUID, version 4.
pub fn uuid() -> String {
    let (a, b) = (random(), random());
    let b = (b & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
    let a = (a & 0xffff_ffff_ffff_0fff) | 0x4000;
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", a >> 32, (a >> 16) & 0xffff, a & 0xffff, b >> 48, b & 0xffff_ffff_ffff)
}

/// A token for `scope` from WAM, and its claims. Only sign-in may ask the user.
fn wam(scope: &str, interactive: bool, claims: Option<&str>) -> Result<(String, Claims), ModuleError> {
    let token = sign_in::wam_token(&[scope.to_string()], interactive, claims).map_err(|e| {
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
    let (token, claims) = wam(AUTHSVC_SCOPE, true, None)?;
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
    let (token, claims) = wam(AUTHSVC_SCOPE, false, None)?;
    if claims.oid != account {
        return Err(ModuleError::AuthRequired("Windows now signs in another account".to_string()));
    }
    establish(token, claims)
}

/// The authsvc exchange: Teams accepts the account and names its services (docs 2). Its 404 is
/// an answer, not a regional redirect.
fn establish(token: String, claims: Claims) -> Result<Session, ModuleError> {
    let response = exchange(&authsvc::request(), &token, false, false)?;
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

/// Runs `f` with the account's session, opened silently if this instance has none yet (after a
/// reload). A session that needs the user again is dropped and the core told (spec 9.1).
pub fn with_session<T>(account: &str, f: impl FnOnce(&mut Session) -> Result<T, ModuleError>) -> Result<T, ModuleError> {
    let taken = SESSIONS.with(|s| s.borrow_mut().remove(account));
    let result = match taken {
        Some(session) => Ok(session),
        None => open(account),
    }
    .and_then(|mut session| {
        let result = f(&mut session);
        if !matches!(result, Err(ModuleError::AuthRequired(_))) {
            SESSIONS.with(|s| s.borrow_mut().insert(account.to_string(), session));
        }
        result
    });
    if matches!(result, Err(ModuleError::AuthRequired(_))) {
        host::session(account, SessionState::AuthRequired);
    }
    result
}

/// A token for `scope` that is not about to expire.
fn token(session: &mut Session, scope: &'static str) -> Result<String, ModuleError> {
    match session.token(scope, now_ms()) {
        Some(token) => Ok(token.to_string()),
        None => renew(session, scope, None),
    }
}

/// A new token from WAM, silently, for the session's own account only.
fn renew(session: &mut Session, scope: &'static str, claims: Option<&str>) -> Result<String, ModuleError> {
    let (token, c) = wam(scope, false, claims)?;
    if c.oid != session.account {
        return Err(ModuleError::AuthRequired("Windows now signs in another account".to_string()));
    }
    session.keep(scope, token.clone(), c.expires_ms);
    Ok(token)
}

/// One request with the session's token. A 401 - a CAE claims challenge or a token the service no
/// longer takes - gets one silent refresh, with the challenge's claims, and one more try; a
/// second 401 means signing in again (spec 9.1). A write the service may have taken is never sent
/// again: it ends `uncertain` (spec 8.5).
pub fn send(session: &mut Session, service: Service, req: &HttpRequest, write: bool) -> Result<http::Response, ModuleError> {
    let scope = service.scope();
    let first = exchange(req, &token(session, scope)?, service == Service::Chat, write)?;
    if first.status != 401 {
        return Ok(first);
    }
    let claims = header(&first.headers, "www-authenticate").and_then(session::challenge_claims);
    let token = renew(session, scope, claims.as_deref())?;
    let second = exchange(req, &token, service == Service::Chat, write)?;
    if second.status == 401 {
        return Err(ModuleError::AuthRequired("the service refused a new token".to_string()));
    }
    Ok(second)
}

/// [`send`] for a read: the body of a 2xx answer, any other status an error.
pub fn read(session: &mut Session, service: Service, req: &HttpRequest) -> Result<Vec<u8>, ModuleError> {
    ok(send(session, service, req, false)?)
}

/// The body of a 2xx answer; any other status is the platform's error.
pub fn ok(response: http::Response) -> Result<Vec<u8>, ModuleError> {
    match response.status {
        200..=299 => Ok(response.body),
        403 => Err(ModuleError::Platform("not allowed (HTTP 403)".to_string())),
        404 => Err(ModuleError::Platform("not found (HTTP 404)".to_string())),
        status => Err(ModuleError::Platform(format!("the service answered HTTP {status}"))),
    }
}

pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// One exchange under the chat service's retry policy (docs 8): 429 and 5xx again after the wait,
/// a write only after a 429, which the service did not take. `redirects`: follow the chat
/// service's regional redirects, a 404 with `Location` on the same host (docs 3.1).
fn exchange(req: &HttpRequest, token: &str, redirects: bool, write: bool) -> Result<http::Response, ModuleError> {
    let mut url = req.url.clone();
    let (mut attempt, mut hops) = (0u8, 0u8);
    loop {
        // No request starts past the budget, so the last one has the rest of the host's limit.
        // ponytail: one answer slower than that still ends in the host's timeout; the host's read
        // timeout (30 s) is as long as its call limit.
        if !has_time() {
            return Err(ModuleError::Platform("Teams answers too slowly; try again later".to_string()));
        }
        let mut headers = req.headers.clone();
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        let request = http::Request { method: req.method.clone(), url: url.clone(), headers, body: req.body.clone() };
        let response = http::fetch(&request).map_err(|e| transport(&e, write))?;
        if redirects
            && response.status == 404
            && let Some(location) = header(&response.headers, "location")
        {
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(ModuleError::Platform("too many regional redirects".to_string()));
            }
            url = regional(&url, location)?;
            continue;
        }
        let status = response.status;
        match retry::decide(status, header(&response.headers, "retry-after"), attempt, now_ms()) {
            Retry::Done => return Ok(response),
            Retry::After(_) if write && status != 429 => return Err(ModuleError::Uncertain(format!("the service failed (HTTP {status})"))),
            Retry::After(ms) => {
                if !pause(ms) {
                    return Err(ModuleError::Platform(format!("the service asks to wait {} s; try again later", ms.div_ceil(1000))));
                }
                attempt += 1;
            }
            Retry::GiveUp(GiveUpReason::Throttled(secs)) => {
                return Err(ModuleError::Platform(format!("the service asks to wait {secs} s; try again later")));
            }
            Retry::GiveUp(GiveUpReason::Exhausted) => return Err(ModuleError::Platform(format!("the service keeps failing (HTTP {status})"))),
        }
    }
}

/// The target of a regional redirect: https on the host the request went to, or refused - the
/// token never goes anywhere else.
fn regional(current: &str, location: &str) -> Result<String, ModuleError> {
    let refused = || ModuleError::Incompatible("a regional redirect away from the chat service".to_string());
    let base = Url::parse(current).map_err(|_| refused())?;
    let target = base.join(location).map_err(|_| refused())?;
    if target.scheme() != "https" || target.host_str() != base.host_str() || target.port() != base.port() {
        return Err(refused());
    }
    Ok(target.into())
}

/// A failed fetch. A write whose answer did not arrive may have reached the service.
fn transport(e: &http::HttpError, write: bool) -> ModuleError {
    let why = match e {
        http::HttpError::Blocked(why) => format!("request blocked: {why}"),
        http::HttpError::Timeout => "the service did not answer in time".to_string(),
        http::HttpError::TooLarge => "the answer was too large".to_string(),
        http::HttpError::Throttled(secs) => format!("the service asks to wait {secs} s; try again later"),
        http::HttpError::Network(why) => format!("network error: {why}"),
    };
    match e {
        http::HttpError::Timeout | http::HttpError::Network(_) | http::HttpError::TooLarge if write => ModuleError::Uncertain(why),
        _ => ModuleError::Platform(why),
    }
}
