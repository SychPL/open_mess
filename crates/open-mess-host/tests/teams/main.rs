//! The built Teams module in the real host, against a fake Teams server on loopback and a fake
//! WAM (docs/protocols/teams.md; spec 9). No test signs in anywhere or reaches Microsoft.

// clippy.toml allows unwrap, panic and indexing in #[test] functions only; the helpers are test
// code too.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

#[path = "../common/mod.rs"]
mod common;
mod fake;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fake::{AUTHZ_PATH, CHAT_PATH, Fake, ME_NAME, ME_OID, Reply, Wam, WamCall, conversation, page};
use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{AccountInfo, Engine, HostError, HostEvent, ModuleContext, ModuleError, ModuleHandle, Net, SessionState, limits};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const AUTHSVC_SCOPE: &str = "https://api.spaces.skype.com/.default";
const CHAT_SCOPE: &str = "https://ic3.teams.office.com/.default";
const GROUP: &str = "19:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@thread.v2";

/// The Teams module, loaded with its hosts served by `fake` and its tokens from `wam`.
struct Teams {
    module: ModuleHandle,
    events: mpsc::Receiver<HostEvent>,
    fake: Fake,
    wam: Wam,
    _data: tempfile::TempDir,
}

impl Teams {
    async fn start() -> Teams {
        let data = tempfile::tempdir().unwrap();
        let (fake, wam) = (Fake::start().await, Wam::new());
        let (module, events) = load(&fake, &wam, data.path()).await;
        Teams { module, events, fake, wam, _data: data }
    }

    /// The module signed in as the test user.
    async fn signed_in() -> Teams {
        let teams = Teams::start().await;
        teams.module.sign_in().await.unwrap();
        teams
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, HostError> {
        let result = self.module.call(ME_OID, tool, &args.to_string()).await?;
        Ok(serde_json::from_str(&result).unwrap())
    }
}

/// Answers the conversation list with one page of `items`, and nothing else.
fn conversations_page(items: Vec<Value>) -> impl Fn(&fake::Seen) -> Option<Reply> {
    move |s| (s.path == CHAT_PATH).then(|| Reply::json(page("conversations", items.clone(), None)))
}

async fn load(fake: &Fake, wam: &Wam, data_dir: &Path) -> (ModuleHandle, mpsc::Receiver<HostEvent>) {
    let engine = Engine::with_limits(limits::for_tests()).unwrap().with_fake_wam(wam.hook());
    let net = Net::with_limits(AddressPolicy::AllowLoopbackForTests, limits::for_tests()).serving_hosts_from(fake.addr);
    let (events, receiver) = mpsc::channel(64);
    let module = engine.load(&common::wasm("open_mess_teams"), ModuleContext { data_dir: data_dir.to_path_buf(), events, net }).await.unwrap();
    (module, receiver)
}

fn module_error(result: Result<impl std::fmt::Debug, HostError>) -> ModuleError {
    match result {
        Err(HostError::Module(e)) => e,
        other => panic!("expected a module error, got {other:?}"),
    }
}

// Task 1: sign-in and restore.

#[tokio::test]
async fn sign_in_returns_the_account_from_the_token() {
    let teams = Teams::start().await;
    let account = teams.module.sign_in().await.unwrap();
    assert_eq!(account, AccountInfo { id: ME_OID.to_string(), display_name: ME_NAME.to_string() });
    assert_eq!(teams.wam.calls(), [WamCall { scopes: vec![AUTHSVC_SCOPE.to_string()], interactive: true, claims: None }]);
    // The account is checked with Teams itself: authsvc got the token, on its own host.
    let authz = teams.fake.requests(AUTHZ_PATH);
    assert_eq!(authz.len(), 1);
    assert_eq!((authz[0].method.as_str(), authz[0].host.as_str()), ("POST", "teams.microsoft.com"));
    assert_eq!(authz[0].audience().as_deref(), Some("https://api.spaces.skype.com"));
}

#[tokio::test]
async fn sign_in_refused_by_teams_is_auth_required() {
    let teams = Teams::start().await;
    teams.fake.on(|_| Some(fake::Reply::status(403)));
    assert!(matches!(module_error(teams.module.sign_in().await), ModuleError::AuthRequired(_)));
}

#[tokio::test]
async fn restore_gets_a_new_token_silently() {
    let teams = Teams::start().await;
    teams.module.restore(ME_OID).await.unwrap();
    assert_eq!(teams.wam.calls(), [WamCall::silent(AUTHSVC_SCOPE)]);
    assert_eq!(teams.fake.requests(AUTHZ_PATH).len(), 1);
}

#[tokio::test]
async fn restore_without_wam_session_is_auth_required() {
    let teams = Teams::start().await;
    teams.wam.set_silent(false);
    assert!(matches!(module_error(teams.module.restore(ME_OID).await), ModuleError::AuthRequired(_)));
    // Never interactive outside sign-in, and nothing was sent anywhere.
    assert!(teams.wam.calls().iter().all(|call| !call.interactive));
    assert!(teams.fake.seen().is_empty());
}

#[tokio::test]
async fn restore_of_another_windows_account_is_auth_required() {
    let teams = Teams::start().await;
    teams.wam.set_account("00000000-0000-0000-0000-000000000009", "Synthetic Someone Else");
    assert!(matches!(module_error(teams.module.restore(ME_OID).await), ModuleError::AuthRequired(_)));
    assert!(teams.fake.seen().is_empty());
}

// Task 2: calls, directory and retries.

#[tokio::test]
async fn directory_comes_from_authsvc_once() {
    let teams = Teams::signed_in().await;
    teams.fake.on(conversations_page(vec![conversation(GROUP, Some("Synthetic group"), None)]));
    for _ in 0..2 {
        teams.call("list_conversations", json!({"limit": 50})).await.unwrap();
    }
    assert_eq!(teams.fake.requests(AUTHZ_PATH).len(), 1);
    // The chat service at the directory's address, with a plain AAD token for its audience.
    let chat = teams.fake.requests(CHAT_PATH);
    assert_eq!(chat.len(), 2);
    for request in chat {
        assert_eq!(request.host, "teams.microsoft.com");
        assert_eq!(request.audience().as_deref(), Some("https://ic3.teams.office.com"));
        assert_eq!(request.header("behavioroverride"), Some("redirectAs404"));
    }
    assert_eq!(teams.wam.calls().iter().filter(|call| **call == WamCall::silent(CHAT_SCOPE)).count(), 1);
}

#[tokio::test]
async fn retry_after_is_honoured() {
    let teams = Teams::signed_in().await;
    let tries = Arc::new(AtomicUsize::new(0));
    let answer = conversations_page(Vec::new());
    teams.fake.on(move |s| {
        let reply = answer(s)?;
        Some(if tries.fetch_add(1, Ordering::SeqCst) == 0 { Reply::status(429).header("Retry-After", "1") } else { reply })
    });
    let started = Instant::now();
    teams.call("list_conversations", json!({})).await.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1), "{:?}", started.elapsed());
    assert_eq!(teams.fake.requests(CHAT_PATH).len(), 2);
}

#[tokio::test]
async fn long_retry_after_gives_up() {
    // The host ends a 429 asking for more than 300 s; the module itself ends an HTTP-date wait
    // that long, which the host passes on (docs/protocols/teams.md 8).
    for (status, retry_after) in [(429, "301"), (503, "Fri, 01 Jan 2100 00:00:00 GMT")] {
        let teams = Teams::signed_in().await;
        teams.fake.on(move |s| (s.path == CHAT_PATH).then(|| Reply::status(status).header("Retry-After", retry_after)));
        let started = Instant::now();
        let error = module_error(teams.call("list_conversations", json!({})).await);
        assert!(matches!(&error, ModuleError::Platform(why) if why.contains("wait")), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(teams.fake.requests(CHAT_PATH).len(), 1);
    }
}

#[tokio::test]
async fn cae_challenge_refreshes_once() {
    let mut teams = Teams::signed_in().await;
    let claims = r#"{"access_token":{"nbf":{"essential":true,"value":"1800000000"}}}"#;
    let challenge = format!(r#"Bearer realm="", error="insufficient_claims", claims="{}""#, fake::base64url(claims.as_bytes()));
    let refused = Arc::new(AtomicUsize::new(1));
    let (answer, limit) = (conversations_page(Vec::new()), refused.clone());
    let header = challenge.clone();
    teams.fake.on(move |s| {
        let reply = answer(s)?;
        // The first `limit` calls get the challenge.
        Some(if limit.fetch_sub(1, Ordering::SeqCst) > 0 { Reply::status(401).header("WWW-Authenticate", &header) } else { reply })
    });
    teams.call("list_conversations", json!({})).await.unwrap();
    let chat = teams.fake.requests(CHAT_PATH);
    assert_eq!(chat.len(), 2);
    assert_ne!(chat[0].header("authorization"), chat[1].header("authorization"), "the call is retried with a new token");
    let refresh = WamCall { claims: Some(claims.to_string()), ..WamCall::silent(CHAT_SCOPE) };
    assert_eq!(teams.wam.calls().last(), Some(&refresh));

    // A second 401 after the refresh: sign in again, and the core is told.
    refused.store(usize::MAX / 2, Ordering::SeqCst);
    let wam_calls = teams.wam.calls().len();
    assert!(matches!(module_error(teams.call("list_conversations", json!({})).await), ModuleError::AuthRequired(_)));
    assert_eq!(teams.fake.requests(CHAT_PATH).len(), 4);
    assert_eq!(teams.wam.calls().len(), wam_calls + 1, "one refresh only");
    let event = tokio::time::timeout(Duration::from_secs(1), teams.events.recv()).await.unwrap().unwrap();
    assert_eq!(event, HostEvent::Session { module: "teams".to_string(), account: ME_OID.to_string(), state: SessionState::AuthRequired });
}

#[tokio::test]
async fn regional_404_redirect_is_followed_on_the_chat_host_only() {
    let teams = Teams::signed_in().await;
    let regional = "/api/chatsvc/amer/v1/users/ME/conversations";
    let items = vec![conversation(GROUP, Some("Synthetic group"), None)];
    teams.fake.on(move |s| match s.path.as_str() {
        CHAT_PATH => Some(Reply::status(404).header("Location", &format!("https://teams.microsoft.com{regional}?view=msnp24Equivalent&pageSize=50"))),
        path if path == regional => Some(Reply::json(page("conversations", items.clone(), None))),
        _ => None,
    });
    let result = teams.call("list_conversations", json!({})).await.unwrap();
    assert_eq!(result["items"].as_array().unwrap().len(), 1);
    assert_eq!(teams.fake.requests(regional)[0].host, "teams.microsoft.com");

    // A redirect to another host is refused, and nothing reaches that host.
    teams.fake.on(|s| (s.path == CHAT_PATH).then(|| Reply::status(404).header("Location", "https://substrate.office.com/search/api/v2/query")));
    assert!(matches!(module_error(teams.call("list_conversations", json!({})).await), ModuleError::Incompatible(_)));
    assert!(teams.fake.seen().iter().all(|s| s.host != "substrate.office.com"));

    // Redirects end after 10 hops.
    teams.fake.on(|s| (s.path == CHAT_PATH).then(|| Reply::status(404).header("Location", CHAT_PATH)));
    let before = teams.fake.requests(CHAT_PATH).len();
    assert!(matches!(module_error(teams.call("list_conversations", json!({})).await), ModuleError::Platform(_)));
    assert_eq!(teams.fake.requests(CHAT_PATH).len() - before, 11);

    // Only the chat service redirects; authsvc's 404 is an answer.
    teams.fake.on(|s| (s.path == AUTHZ_PATH).then(|| Reply::status(404).header("Location", "/api/authsvc/v1.0/elsewhere")));
    assert!(teams.module.sign_in().await.is_err());
    assert!(teams.fake.requests("/api/authsvc/v1.0/elsewhere").is_empty());
}
