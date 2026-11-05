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
use crate::component::bindings::open_mess::module::{http, sign_in, storage};
use crate::http::HttpRequest;
use crate::poll;
use crate::retry::{self, GiveUpReason, Retry};
use crate::session::{self, AUTHSVC_SCOPE, CHAT_SCOPE, Claims, MCPS_SCOPE, SEARCH_SCOPE, Session};

/// The part of the host's call limit (30 s) the module plans with: no wait starts that would end
/// past it, so a slow service ends a call with an error, not a timeout that unloads the module.
const CALL_BUDGET: Duration = Duration::from_secs(20);
/// Regional redirects of one request (docs 3.1).
const MAX_REDIRECTS: u8 = 10;

thread_local! {
    /// Signed-in accounts by id. A module instance runs one call at a time, on one thread.
    static SESSIONS: RefCell<HashMap<String, Session>> = RefCell::new(HashMap::new());
    /// Whether the poll timer is set; setting it again would postpone every account's poll.
    static POLLING: Cell<bool> = const { Cell::new(false) };
    /// When the current call's budget ends.
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    /// Whether the current call works in the background (the poll), not for an agent.
    static BACKGROUND: Cell<bool> = const { Cell::new(false) };
}

/// Starts a call's budget; every export calls it first.
pub fn begin() {
    begin_within(CALL_BUDGET);
}

/// Starts a shorter budget, for an export the host limits more (`end-sessions`, 5 s).
pub fn begin_within(budget: Duration) {
    DEADLINE.set(Some(Instant::now() + budget));
    BACKGROUND.set(false);
}

/// Marks the rest of the current call as background work: its requests go at the low priority the
/// official client gives background requests (spec 9.2, docs/protocols/teams.md 8).
pub fn background() {
    BACKGROUND.set(true);
}

/// Whether the call's budget has time left for more requests.
pub fn has_time() -> bool {
    DEADLINE.get().is_none_or(|deadline| Instant::now() < deadline)
}

/// Waits `ms` if that ends within the budget.
pub fn wait(ms: u32) -> bool {
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
    /// A meeting's artifacts (docs 9.3).
    Mcps,
}

impl Service {
    fn scope(self) -> &'static str {
        match self {
            Service::Chat | Service::Media => CHAT_SCOPE,
            Service::Search => SEARCH_SCOPE,
            Service::Mcps => MCPS_SCOPE,
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

/// A token for `scope` from WAM, and its claims. Only sign-in may ask the user. `account` names
/// the module account, so an event (a timer, a socket frame) gets that account's token when there
/// are several (spec 2026-10-29-live-captions, 5.2); a sign-in names none.
fn wam(scope: &str, interactive: bool, claims: Option<&str>, account: Option<&str>) -> Result<(String, Claims), ModuleError> {
    let token = sign_in::wam_token(&[scope.to_string()], interactive, claims, account).map_err(|e| {
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
    let (token, claims) = wam(AUTHSVC_SCOPE, true, None, None)?;
    // The user's time in the WAM dialog does not count against the host's limit (spec 8.3, R4),
    // so it does not count against the budget either.
    begin();
    let session = establish(token, claims)?;
    let info = AccountInfo { id: session.account.clone(), display_name: session.name.clone() };
    keep(session);
    Ok(info)
}

/// A session for `account` again, with tokens WAM gives without asking.
pub fn restore(account: &str) -> Result<(), ModuleError> {
    let session = open(account)?;
    keep(session);
    Ok(())
}

pub fn sign_out(account: &str) {
    SESSIONS.with(|s| s.borrow_mut().remove(account));
    storage::delete(&poll::key(account));
}

/// The accounts with a session in this instance.
pub fn accounts() -> Vec<String> {
    SESSIONS.with(|s| s.borrow().keys().cloned().collect())
}

/// Keeps a new session and polls for its messages from now on.
fn keep(session: Session) {
    start_polling(&session.account);
    SESSIONS.with(|s| s.borrow_mut().insert(session.account.clone(), session));
}

/// Messages of `account` count from now, unless a poll state is stored already; the poll timer is
/// set unless it is pending.
fn start_polling(account: &str) {
    let key = poll::key(account);
    if storage::get(&key).is_none() && storage::set(&key, &poll::PollState::new(now_ms()).encode()).is_err() {
        host::log(1, "the poll state was not stored");
    }
    if !POLLING.replace(true) {
        host::set_timer(poll::TIMER, poll::INTERVAL_MS);
    }
}

/// The poll timer fired; `polling_again` sets it for the next round.
pub fn timer_fired() {
    POLLING.set(false);
}

pub fn polling_again() {
    if !POLLING.replace(true) {
        host::set_timer(poll::TIMER, poll::INTERVAL_MS);
    }
}

fn open(account: &str) -> Result<Session, ModuleError> {
    let (token, claims) = wam(AUTHSVC_SCOPE, false, None, Some(account))?;
    if claims.oid != account {
        return Err(ModuleError::AuthRequired("Windows now signs in another account".to_string()));
    }
    establish(token, claims)
}

/// The authsvc exchange: Teams accepts the account and names its services (docs 2). Its 404 is
/// an answer, not a regional redirect.
fn establish(token: String, claims: Claims) -> Result<Session, ModuleError> {
    let auth = authorize(&token)?;
    let expires = claims.expires_ms;
    let mut session = Session::new(claims, auth.directory);
    session.region = auth.region;
    session.keep(AUTHSVC_SCOPE, token, expires);
    session.keep_skype(auth.skype_token, auth.expires_at_ms);
    Ok(session)
}

fn authorize(token: &str) -> Result<authsvc::Session, ModuleError> {
    let response = exchange(&authsvc::request(), Some(token), false, false)?;
    match response.status {
        200..=299 => {}
        401 | 403 => return Err(ModuleError::AuthRequired(format!("Teams refused the account (HTTP {})", response.status))),
        status => return Err(ModuleError::Platform(format!("Teams sign-in failed (HTTP {status})"))),
    }
    authsvc::decode(&response.body, now_ms()).map_err(proto)
}

/// A new authsvc exchange for the session: a new skype token and service directory.
fn reauthorize(session: &mut Session, token: &str) -> Result<(), ModuleError> {
    let auth = authorize(token)?;
    session.directory = auth.directory;
    session.region = auth.region;
    session.keep_skype(auth.skype_token, auth.expires_at_ms);
    Ok(())
}

/// A read from the middle tier (docs 9.1): the authsvc audience's token and the skype token. A
/// skype token about to expire is renewed by a new authsvc exchange first; a 401 gets a new token,
/// one more exchange and one more try (spec 2026-10-28-meetings-transcripts, 5.3).
pub fn middle_tier(session: &mut Session, req: &HttpRequest) -> Result<Vec<u8>, ModuleError> {
    let token = token(session, AUTHSVC_SCOPE)?;
    if session.skype(now_ms(), true).is_none() {
        reauthorize(session, &token)?;
    }
    let first = exchange(&with_skype(session, req)?, Some(&token), false, false)?;
    if first.status != 401 {
        return ok(first);
    }
    let claims = header(&first.headers, "www-authenticate").and_then(session::challenge_claims);
    let token = renew(session, AUTHSVC_SCOPE, claims.as_deref())?;
    reauthorize(session, &token)?;
    let second = exchange(&with_skype(session, req)?, Some(&token), false, false)?;
    if second.status == 401 {
        return Err(ModuleError::AuthRequired("the service refused a new token".to_string()));
    }
    ok(second)
}

/// A request to the conversation service of live captions with the skype token and no bearer, as
/// the probe sent them ("Live results" 14-18). A skype token about to expire is renewed by a new
/// authsvc exchange first; a 401, which the service did not take, gets one more exchange and one
/// more try.
pub fn conversation(session: &mut Session, req: &HttpRequest, write: bool) -> Result<http::Response, ModuleError> {
    if session.skype(now_ms(), true).is_none() {
        let token = token(session, AUTHSVC_SCOPE)?;
        reauthorize(session, &token)?;
    }
    let first = exchange(&with_skype(session, req)?, None, false, write)?;
    if first.status != 401 {
        return Ok(first);
    }
    let token = renew(session, AUTHSVC_SCOPE, None)?;
    reauthorize(session, &token)?;
    let second = exchange(&with_skype(session, req)?, None, false, write)?;
    if second.status == 401 {
        return Err(ModuleError::AuthRequired("the service refused a new skype token".to_string()));
    }
    Ok(second)
}

/// The IC3 token of Trouter's `user.authenticate`, the chat service's (docs 10.7).
pub fn chat_token(session: &mut Session) -> Result<String, ModuleError> {
    token(session, CHAT_SCOPE)
}

fn with_skype(session: &Session, req: &HttpRequest) -> Result<HttpRequest, ModuleError> {
    let skype = session.skype(now_ms(), false).ok_or_else(|| ModuleError::NotSupported("Teams gave no skype token for this account".to_string()))?;
    let mut req = req.clone();
    req.headers.push(("x-skypetoken".to_string(), skype.to_string()));
    Ok(req)
}

/// Runs `f` with the account's session, opened silently if this instance has none yet (after a
/// reload). A session that needs the user again is dropped and the core told (spec 9.1).
pub fn with_session<T>(account: &str, f: impl FnOnce(&mut Session) -> Result<T, ModuleError>) -> Result<T, ModuleError> {
    let taken = SESSIONS.with(|s| s.borrow_mut().remove(account));
    let result = match taken {
        Some(session) => Ok(session),
        // A reloaded instance lost its timer with its memory: polling starts again.
        None => open(account).inspect(|s| start_polling(&s.account)),
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
fn token(session: &mut Session, scope: &str) -> Result<String, ModuleError> {
    match session.token(scope, now_ms()) {
        Some(token) => Ok(token.to_string()),
        None => renew(session, scope, None),
    }
}

/// A new token from WAM, silently, for the session's own account only.
fn renew(session: &mut Session, scope: &str, claims: Option<&str>) -> Result<String, ModuleError> {
    let (token, c) = wam(scope, false, claims, Some(&session.account))?;
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
    let first = exchange(req, Some(&token(session, scope)?), service == Service::Chat, write)?;
    if first.status != 401 {
        return Ok(first);
    }
    let claims = header(&first.headers, "www-authenticate").and_then(session::challenge_claims);
    let token = renew(session, scope, claims.as_deref())?;
    let second = exchange(req, Some(&token), service == Service::Chat, write)?;
    if second.status == 401 {
        return Err(ModuleError::AuthRequired("the service refused a new token".to_string()));
    }
    Ok(second)
}

/// A read from the organization's SharePoint or OneDrive at `origin`, with a token for that
/// origin. A 401 gets one silent refresh, as in [`send`]; a refusal after that, or a 403, is the
/// platform's answer about this transcript, not a sign-in the account needs (spec
/// 2026-10-28-meetings-transcripts, 4.2).
pub fn sharepoint(session: &mut Session, origin: &str, req: &HttpRequest) -> Result<Vec<u8>, ModuleError> {
    let scope = format!("{origin}/.default");
    let mut response = exchange(req, Some(&token(session, &scope)?), false, false)?;
    if response.status == 401 {
        let claims = header(&response.headers, "www-authenticate").and_then(session::challenge_claims);
        let token = renew(session, &scope, claims.as_deref())?;
        response = exchange(req, Some(&token), false, false)?;
    }
    match response.status {
        401 | 403 => Err(ModuleError::Platform("access to this transcript was refused".to_string())),
        404 => Err(ModuleError::Platform("transcript not found".to_string())),
        _ => ok(response),
    }
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
/// a write only after a 429, which the service did not take. `token` goes as the bearer, if any.
/// `redirects`: follow the chat service's regional redirects, a 404 with `Location` on the same
/// host (docs 3.1).
fn exchange(req: &HttpRequest, token: Option<&str>, redirects: bool, write: bool) -> Result<http::Response, ModuleError> {
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
        if let Some(token) = token {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
        if BACKGROUND.get() {
            headers.push(("x-ms-request-priority".to_string(), "20".to_string()));
        }
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
        // A 5xx may come after the service took the write, whatever its Retry-After says.
        if write && (500..=599).contains(&status) {
            return Err(ModuleError::Uncertain(format!("the service failed (HTTP {status})")));
        }
        match retry::decide(status, header(&response.headers, "retry-after"), attempt, now_ms()) {
            Retry::Done => return Ok(response),
            Retry::After(ms) => {
                if !wait(ms) {
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
    // Blocked never left; throttled is a 429, a refusal. Any other failure may follow a write the
    // service took.
    match e {
        http::HttpError::Blocked(_) | http::HttpError::Throttled(_) => ModuleError::Platform(why),
        _ if write => ModuleError::Uncertain(why),
        _ => ModuleError::Platform(why),
    }
}
