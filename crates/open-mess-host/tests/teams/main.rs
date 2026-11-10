//! The built Teams module in the real host, against a fake Teams server on loopback and a fake
//! WAM (docs/protocols/teams.md; spec 9). No test signs in anywhere or reaches Microsoft.

// clippy.toml allows unwrap, panic and indexing in #[test] functions only; the helpers are test
// code too.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

#[path = "../common/mod.rs"]
mod common;
mod fake;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fake::{AUTHZ_PATH, CHAT, CHAT_PATH, Fake, ME, ME_NAME, ME_OID, OTHER, Reply, Wam, WamCall, conversation, message, page};
use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{
    AccountInfo, Engine, HostError, HostEvent, ModuleContext, ModuleError, ModuleHandle, Net, NewMessage, SessionState, SignInPrompts, limits,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const AUTHSVC_SCOPE: &str = "https://api.spaces.skype.com/.default";
const CHAT_SCOPE: &str = "https://ic3.teams.office.com/.default";
const GROUP: &str = "19:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@thread.v2";
const DIRECT: &str = "19:00000000-0000-0000-0000-000000000001_00000000-0000-0000-0000-000000000002@unq.gbl.spaces";
const CHANNEL: &str = "19:cccccccccccccccccccccccccccccccc@thread.tacv2";
const MEETING: &str = "19:meeting_c3ludGhldGlj@thread.v2";
/// 2027-01-15T08:00:00Z.
const T0: i64 = 1_800_000_000_000;

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
        let (module, events) = load(&fake, &wam, data.path(), None, None).await;
        Teams { module, events, fake, wam, _data: data }
    }

    /// Signed in, with the module's timers (its 60 s poll) firing within `POLL` instead.
    async fn polling() -> Teams {
        let data = tempfile::tempdir().unwrap();
        let (fake, wam) = (Fake::start().await, Wam::new());
        let (module, events) = load(&fake, &wam, data.path(), Some(POLL), None).await;
        module.sign_in().await.unwrap();
        Teams { module, events, fake, wam, _data: data }
    }

    async fn next_message(&mut self) -> Option<NewMessage> {
        next_message(&mut self.events, &self.fake).await
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

/// The longest a module timer waits in the polling tests, in milliseconds.
const POLL: u32 = 300;

async fn load(
    fake: &Fake,
    wam: &Wam,
    data_dir: &Path,
    timer_cap: Option<u32>,
    trouter: Option<SocketAddr>,
) -> (ModuleHandle, mpsc::Receiver<HostEvent>) {
    // Live captions wait for the call store within the module's budget, as in the daemon: the
    // host's own call limit, not the tests' shorter one.
    let calls = if trouter.is_some() { limits::Limits { call_timeout: limits::CALL_TIMEOUT, ..limits::for_tests() } } else { limits::for_tests() };
    let mut engine = Engine::with_limits(calls).unwrap().with_fake_wam(wam.hook());
    if let Some(cap) = timer_cap {
        engine = engine.with_timer_cap(cap);
    }
    let mut net = Net::with_limits(AddressPolicy::AllowLoopbackForTests, limits::for_tests()).serving_hosts_from(fake.addr);
    if let Some(trouter) = trouter {
        net = net.serving_websockets_from(trouter);
    }
    let (events, receiver) = mpsc::channel(64);
    // Signs in through the fake WAM: no prompt reaches a window.
    let sign_in = SignInPrompts::new(mpsc::channel(1).0);
    let module =
        engine.load(&common::wasm("open_mess_teams"), ModuleContext { data_dir: data_dir.to_path_buf(), events, net, sign_in }).await.unwrap();
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

// Task 3: read tools.

fn ids(result: &Value, key: &str) -> Vec<String> {
    result["items"].as_array().unwrap().iter().map(|item| item[key].as_str().unwrap().to_string()).collect()
}

/// Message `n` (101, 102...) of `conversation`, sent `n - 100` minutes after `T0`.
fn numbered(n: i64, conversation: &str, from: &str, html: &str) -> Value {
    message(&n.to_string(), conversation, from, html, T0 + (n - 100) * 60_000)
}

fn messages_path(conversation: &str) -> String {
    format!("{CHAT_PATH}/{conversation}/messages")
}

/// A mention of `mri` as the chat service stores it: a JSON list inside a string.
fn mentioning(mut message: Value, mri: &str) -> Value {
    message["properties"]["mentions"] = json!([{"mri": mri, "displayName": "Synthetic"}]).to_string().into();
    message
}

#[tokio::test]
async fn list_conversations_maps_kinds() {
    let teams = Teams::signed_in().await;
    let last = numbered(101, GROUP, OTHER, "<p>hi</p>");
    teams.fake.on(conversations_page(vec![
        conversation(GROUP, Some("Synthetic group"), Some(last)),
        conversation(DIRECT, None, Some(numbered(102, DIRECT, OTHER, "<p>hi</p>"))),
        conversation(CHANNEL, Some("Synthetic channel"), None),
        conversation(MEETING, Some("Synthetic meeting"), None),
        conversation("48:notes", None, None),
        // The service's own feed of copies is not a conversation; the self-chat is.
        conversation("48:mentions", None, None),
    ]));
    let result = teams.call("list_conversations", json!({"limit": 50})).await.unwrap();
    let kinds: Vec<(String, String)> = ids(&result, "id").into_iter().zip(ids(&result, "kind")).collect();
    let expected = [(GROUP, "group"), (DIRECT, "direct"), (CHANNEL, "channel"), (MEETING, "meeting"), ("48:notes", "group")];
    assert_eq!(kinds, expected.map(|(id, kind)| (id.to_string(), kind.to_string())));
    assert_eq!(result["items"][0]["last_activity"], "2027-01-15T08:01:00Z");
    assert_eq!(result["items"][0]["untrusted"], json!({"title": "Synthetic group"}));
    // Filters by kind and by part of the name.
    assert_eq!(ids(&teams.call("list_conversations", json!({"kind": "channel"})).await.unwrap(), "id"), [CHANNEL]);
    assert_eq!(ids(&teams.call("list_conversations", json!({"query": "MEETING"})).await.unwrap(), "id"), [MEETING]);
    // A 1:1 chat has no topic: the other person's name stands in, and finds it.
    assert_eq!(result["items"][1]["untrusted"], json!({"title": "Synthetic Person Two"}));
    assert_eq!(ids(&teams.call("list_conversations", json!({"query": "person two"})).await.unwrap(), "id"), [DIRECT]);
    // The core applies the limit after its read filter, so the module leaves no match out.
    assert_eq!(ids(&teams.call("list_conversations", json!({"limit": 2})).await.unwrap(), "id").len(), 5);
    // An agent's requests go at the default priority; only the poll's are background ones.
    assert!(teams.fake.requests(CHAT_PATH).iter().all(|s| s.header("x-ms-request-priority").is_none()));
}

#[tokio::test]
async fn a_direct_chat_the_user_wrote_last_in_is_found_by_name() {
    let teams = Teams::signed_in().await;
    let members_path = format!("/api/chatsvc/emea/v1/threads/{DIRECT}/members");
    let members = json!({"members": [{"id": ME, "friendlyName": ME_NAME}, {"id": OTHER, "friendlyName": "Synthetic Person Two"}]});
    let list =
        vec![conversation(DIRECT, None, Some(numbered(101, DIRECT, ME, "<p>see you</p>"))), conversation(GROUP, Some("Synthetic group"), None)];
    let path = members_path.clone();
    teams.fake.on(move |s| {
        if s.path == CHAT_PATH {
            Some(Reply::json(page("conversations", list.clone(), None)))
        } else {
            (s.path == path).then(|| Reply::json(members.clone()))
        }
    });
    // The list names the chat after no one: its members do.
    let found = teams.call("list_conversations", json!({"query": "person two"})).await.unwrap();
    assert_eq!(ids(&found, "id"), [DIRECT]);
    assert_eq!(found["items"][0]["untrusted"], json!({"title": "Synthetic Person Two"}));
    // Without a query nothing more is asked for.
    let asked = teams.fake.requests(&members_path).len();
    assert_eq!(ids(&teams.call("list_conversations", json!({})).await.unwrap(), "id"), [DIRECT, GROUP]);
    assert_eq!(teams.fake.requests(&members_path).len(), asked);
    // Members in a shape the module does not know: an explicit error, not a chat silently left out.
    let list = vec![conversation(DIRECT, None, Some(numbered(101, DIRECT, ME, "<p>see you</p>")))];
    teams.fake.on(move |s| {
        if s.path == CHAT_PATH {
            Some(Reply::json(page("conversations", list.clone(), None)))
        } else {
            (s.path == members_path).then(|| Reply::json(json!({"roster": []})))
        }
    });
    let unknown = teams.call("list_conversations", json!({"query": "person two"})).await;
    assert!(matches!(module_error(unknown), ModuleError::Incompatible(_)));
}

#[tokio::test]
async fn a_failing_later_page_keeps_what_was_found() {
    let teams = Teams::signed_in().await;
    let next = format!("{CHAT}/v1/users/ME/conversations?view=msnp24Equivalent&pageSize=50&startTime=1&syncState=two");
    teams.fake.on(move |s| {
        (s.path == CHAT_PATH).then(|| {
            if s.query.contains("syncState=two") {
                Reply::status(403)
            } else {
                Reply::json(page("conversations", vec![conversation(GROUP, Some("Synthetic group"), None)], Some(&next)))
            }
        })
    });
    assert_eq!(ids(&teams.call("list_conversations", json!({})).await.unwrap(), "id"), [GROUP]);
    // unread must not pass a failure off as nothing unread.
    assert!(matches!(module_error(teams.call("unread", json!({})).await), ModuleError::Platform(_)));
    // The first page failing is an error for a listing too.
    teams.fake.on(|s| (s.path == CHAT_PATH).then(|| Reply::status(403)));
    assert!(matches!(module_error(teams.call("list_conversations", json!({})).await), ModuleError::Platform(_)));
}

#[tokio::test]
async fn read_messages_pages_backwards() {
    let teams = Teams::signed_in().await;
    let path = messages_path(GROUP);
    let older = format!("{CHAT}/v1/users/ME/conversations/{GROUP}/messages?view=msnp24Equivalent&pageSize=50&startTime=1&syncState=older");
    let newer =
        vec![numbered(105, GROUP, OTHER, "<p>five</p>"), numbered(104, GROUP, ME, "<p><b>four</b></p>"), numbered(103, GROUP, OTHER, "three")];
    let rest = vec![numbered(102, GROUP, OTHER, "two"), numbered(101, GROUP, OTHER, "one")];
    let one = format!("{path}/103");
    let message_103 = numbered(103, GROUP, OTHER, "three");
    teams.fake.on(move |s| {
        if s.path == one {
            return Some(Reply::json(message_103.clone()));
        }
        (s.path == path).then(|| {
            let (items, link) = if s.query.contains("syncState=older") { (rest.clone(), None) } else { (newer.clone(), Some(older.as_str())) };
            Reply::json(page("messages", items, link))
        })
    });
    let result = teams.call("read_messages", json!({"conversation": GROUP, "limit": 4})).await.unwrap();
    assert_eq!(ids(&result, "id"), ["105", "104", "103", "102"]);
    let first = &result["items"][0];
    assert_eq!(
        (first["conversation"].as_str(), first["sent_at"].as_str(), first["own"].as_bool()),
        (Some(GROUP), Some("2027-01-15T08:05:00Z"), Some(false))
    );
    assert_eq!(first["untrusted"], json!({"sender_name": "Synthetic Person Two", "text": "five"}));
    assert_eq!((result["items"][1]["own"].as_bool(), result["items"][1]["untrusted"]["text"].as_str()), (Some(true), Some("**four**")));
    let pages = teams.fake.requests(&messages_path(GROUP));
    assert_eq!(pages.len(), 2);
    assert!(pages[1].query.contains("syncState=older") && pages[1].host == "teams.microsoft.com");
    // The next page after a message, asked for by its time, and only what came after a time.
    assert_eq!(ids(&teams.call("read_messages", json!({"conversation": GROUP, "before": "103"})).await.unwrap(), "id"), ["102", "101"]);
    let after_103 = teams.fake.requests(&messages_path(GROUP)).into_iter().filter(|s| s.query.contains("endTime=")).collect::<Vec<_>>();
    assert!(after_103[0].query.contains(&format!("endTime={}", T0 + 3 * 60_000)), "{}", after_103[0].query);
    let since = json!({"conversation": GROUP, "since": "2027-01-15T09:03:30+01:00"});
    assert_eq!(ids(&teams.call("read_messages", since).await.unwrap(), "id"), ["105", "104"]);
    let unknown = teams.call("read_messages", json!({"conversation": GROUP, "before": "999"})).await;
    assert!(matches!(module_error(unknown), ModuleError::InvalidArgs(_)));
}

#[tokio::test]
async fn read_messages_never_returns_a_list_cut_short() {
    let teams = Teams::signed_in().await;
    // Every page holds only a call event and points at an older one: the walk reaches its page
    // limit before any text message, the limit or `since`.
    let path = messages_path(MEETING);
    let older = format!("{CHAT}/v1/users/ME/conversations/{MEETING}/messages?view=msnp24Equivalent&pageSize=50&startTime=1&syncState=older");
    let mut event = numbered(101, MEETING, OTHER, "");
    event["messagetype"] = "Event/Call".into();
    teams.fake.on(move |s| (s.path == path).then(|| Reply::json(page("messages", vec![event.clone()], Some(older.as_str())))));
    let cut = teams.call("read_messages", json!({"conversation": MEETING, "since": "2027-01-15T07:00:00Z"})).await;
    assert!(matches!(module_error(cut), ModuleError::Platform(why) if why.contains("that far back")));
    assert_eq!(teams.fake.requests(&messages_path(MEETING)).len(), 20);
}

#[tokio::test]
async fn foreign_backward_link_is_refused() {
    let teams = Teams::signed_in().await;
    let path = messages_path(GROUP);
    let link = format!("https://evil.example/api/chatsvc/emea/v1/users/ME/conversations/{GROUP}/messages?syncState=older");
    teams.fake.on(move |s| (s.path == path).then(|| Reply::json(page("messages", vec![numbered(101, GROUP, OTHER, "one")], Some(&link)))));
    let result = teams.call("read_messages", json!({"conversation": GROUP, "limit": 10})).await;
    assert!(matches!(module_error(result), ModuleError::Incompatible(_)));
    // The token never went anywhere else, and the page was not asked for on our host either.
    assert_eq!(teams.fake.seen().iter().filter(|s| s.path.contains("/messages")).count(), 1);
    assert!(teams.fake.seen().iter().all(|s| s.host != "evil.example"));
}

/// A conversation with this consumption horizon (`None`: no property) and a last message from
/// `from` at `T0 + at_ms` (`None`: no message at all).
fn read_state(id: &str, horizon: Option<&str>, last: Option<(&str, i64)>) -> Value {
    let mut c = conversation(id, None, last.map(|(from, at)| message("1", id, from, "<p>x</p>", T0 + at)));
    c["properties"] = horizon.map_or_else(|| json!({}), |h| json!({"consumptionhorizon": h}));
    c
}

#[tokio::test]
async fn unread_uses_the_client_rule() {
    let teams = Teams::signed_in().await;
    let horizon = "1800000000000;1800000000000;1";
    teams.fake.on(conversations_page(vec![
        // A message after the horizon: unread by the conversation's own horizon, but "mark all
        // as read" may have passed it, and that horizon is not readable yet.
        read_state("19:after@thread.v2", Some(horizon), Some((OTHER, 100))),
        read_state("19:before@thread.v2", Some(horizon), Some((OTHER, -100))),
        read_state("19:mine@thread.v2", Some(horizon), Some((ME, 100))),
        read_state("19:empty@thread.v2", Some(""), None),
        read_state("19:nohorizon@thread.v2", None, Some((OTHER, 100))),
    ]));
    let result = teams.call("unread", json!({})).await.unwrap();
    assert_eq!(ids(&result, "conversation"), ["19:after@thread.v2", "19:nohorizon@thread.v2"]);
    assert_eq!(ids(&result, "unread"), ["unknown", "unknown"]);
    assert_eq!(result["items"][0]["kind"], "group");
    // Reading never writes: nothing but the conversation list was asked for.
    assert!(teams.fake.seen().iter().all(|s| s.method == "GET" || s.path == AUTHZ_PATH));
}

#[tokio::test]
async fn unread_pages_within_the_limit() {
    // 300 conversations in pages of 50, then a list that never ends.
    for endless in [false, true] {
        let teams = Teams::signed_in().await;
        teams.fake.on(move |s| {
            (s.path == CHAT_PATH).then(|| {
                let n: usize = s.query.split("syncState=p").nth(1).map_or(0, |n| n.split('&').next().unwrap().parse().unwrap());
                let items = (0..50).map(|i| read_state(&format!("19:p{n}c{i}@thread.v2"), Some(""), Some((OTHER, 1)))).collect();
                let link = format!("{CHAT}/v1/users/ME/conversations?view=msnp24Equivalent&pageSize=50&startTime=1&syncState=p{}", n + 1);
                Reply::json(page("conversations", items, (endless || n < 5).then_some(link.as_str())))
            })
        });
        let result = teams.call("unread", json!({})).await.unwrap();
        let pages = teams.fake.requests(CHAT_PATH);
        assert!(pages.iter().all(|p| p.query.contains("pageSize=50")));
        if endless {
            // Bounded: 20 pages, then what was found so far.
            assert_eq!((pages.len(), result["items"].as_array().unwrap().len()), (20, 1000));
        } else {
            assert_eq!((pages.len(), result["items"].as_array().unwrap().len()), (6, 300));
        }
    }
}

#[tokio::test]
async fn mentions_from_recent_messages() {
    let teams = Teams::signed_in().await;
    let (group, channel, feed) = (messages_path(GROUP), messages_path(CHANNEL), messages_path("48:mentions"));
    let conversations = vec![
        // The feed of copies of mentions ("Live results" 8), newest of all.
        conversation("48:mentions", None, Some(mentioning(numbered(104, "48:mentions", OTHER, "copy"), ME))),
        conversation(GROUP, None, Some(numbered(103, GROUP, OTHER, "x"))),
        conversation(CHANNEL, None, Some(numbered(102, CHANNEL, OTHER, "x"))),
    ];
    let feed_path = feed.clone();
    teams.fake.on(move |s| {
        let messages = if s.path == CHAT_PATH {
            return Some(Reply::json(page("conversations", conversations.clone(), None)));
        } else if s.path == group {
            vec![
                mentioning(numbered(103, GROUP, OTHER, "<p>hey <span itemtype=\"http://schema.skype.com/Mention\">One</span></p>"), ME),
                numbered(102, GROUP, OTHER, "no mention"),
                // Before `since`.
                mentioning(numbered(99, GROUP, OTHER, "old"), ME),
            ]
        } else if s.path == channel {
            vec![mentioning(numbered(102, CHANNEL, OTHER, "someone else"), OTHER), mentioning(numbered(101, CHANNEL, ME, "myself"), ME)]
        } else if s.path == feed_path {
            vec![mentioning(numbered(104, "48:mentions", OTHER, "copy"), ME)]
        } else {
            return None;
        };
        Some(Reply::json(page("messages", messages, None)))
    });
    let result = teams.call("mentions", json!({"since": "2027-01-15T08:00:00Z"})).await.unwrap();
    assert_eq!(ids(&result, "id"), ["103"]);
    assert_eq!(ids(&result, "conversation"), [GROUP]);
    assert_eq!(result["items"][0]["untrusted"]["text"], "hey @One");
    assert!(teams.fake.requests(&feed).is_empty());
    // Only conversations active since then were listed.
    assert!(teams.fake.requests(CHAT_PATH)[0].query.contains("startTime=1800000000000"));
}

#[tokio::test]
async fn search_posts_to_substrate() {
    let teams = Teams::signed_in().await;
    let hit = json!({"Source": {
        "ItemId": "item-1",
        "ClientConversationId": GROUP,
        "DateTimeReceived": "2027-01-15T08:00:00Z",
        "Preview": "the synthetic budget",
        "From": {"EmailAddress": {"Name": "Synthetic Person Two", "Address": "person.two@synthetic.example"}},
    }});
    teams
        .fake
        .on(move |s| (s.path == "/search/api/v2/query").then(|| Reply::json(json!({"EntitySets": [{"ResultSets": [{"Results": [hit.clone()]}]}]}))));
    let result = teams.call("search", json!({"query": "budget", "limit": 5})).await.unwrap();
    let expected = json!({
        "id": "item-1",
        "conversation": GROUP,
        "sent_at": "2027-01-15T08:00:00Z",
        "edited": false,
        "own": false,
        "untrusted": {"sender_name": "Synthetic Person Two", "text": "the synthetic budget"},
    });
    assert_eq!(result["items"], json!([expected]));
    let request = &teams.fake.requests("/search/api/v2/query")[0];
    assert_eq!((request.method.as_str(), request.host.as_str()), ("POST", "substrate.office.com"));
    assert_eq!(request.audience().as_deref(), Some("https://outlook.office.com"));
    assert_eq!(request.header("x-anchormailbox"), Some("Oid:00000000-0000-0000-0000-000000000001@00000000-0000-0000-0000-0000000000aa"));
    let body = request.json();
    assert_eq!((body["entityRequests"][0]["query"]["queryString"].as_str(), body["entityRequests"][0]["size"].as_u64()), (Some("budget"), Some(5)));
}

#[tokio::test]
async fn writing_context_filters_like_the_wrapper() {
    let teams = Teams::signed_in().await;
    let path = messages_path(GROUP);
    let messages = vec![
        numbered(110, GROUP, ME, "<p>ok, lecę na obiad</p>"),
        numbered(109, GROUP, ME, "<p>deploy na prod przeszedł</p>"),
        numbered(108, GROUP, ME, "<p>password: hunter2</p>"),
        numbered(107, GROUP, ME, "<p>const x = 1</p>"),
        numbered(106, GROUP, ME, "<p>https://example.com/x</p>"),
        numbered(105, GROUP, ME, "<p>{\"a\": 1}</p>"),
        numbered(104, GROUP, ME, "<p>zobacz <a href=\"https://example.com\">to</a></p>"),
        numbered(103, GROUP, OTHER, "<p>hej, co tam?</p>"),
        numbered(102, GROUP, ME, "<p>ok, lecę na obiad</p>"),
    ];
    teams.fake.on(move |s| (s.path == path).then(|| Reply::json(page("messages", messages.clone(), None))));
    let result = teams.call("writing_context", json!({"conversation": GROUP, "purpose": "technical_status", "limit": 6})).await.unwrap();
    let examples: Vec<&str> = result["examples"].as_array().unwrap().iter().map(|e| e["untrusted"]["text"].as_str().unwrap()).collect();
    assert_eq!(examples, ["deploy na prod przeszedł", "ok, lecę na obiad"]);
    assert_eq!(result["examples"][1]["sent_at"], "2027-01-15T08:10:00Z");
}

#[tokio::test]
async fn get_attachment_fetches_ams_image() {
    let teams = Teams::signed_in().await;
    let object = "0-weu-d1-synthetic";
    let html = format!(
        "<p><img itemtype=\"http://schema.skype.com/AMSImage\" src=\"https://eu-prod.asyncgw.teams.microsoft.com/v1/objects/{object}/views/imgo\"></p>"
    );
    let mut image = numbered(103, GROUP, OTHER, &html);
    image["amsreferences"] = json!([object]);
    let (one, list) = (format!("{}/103", messages_path(GROUP)), messages_path(GROUP));
    let view = format!("/v1/objects/{object}/views/imgo");
    teams.fake.on(move |s| {
        if s.path == one {
            Some(Reply::json(image.clone()))
        } else if s.path == list {
            Some(Reply::json(page("messages", vec![image.clone()], None)))
        } else if s.path == view {
            Some(Reply { body: b"\x89PNG synthetic".to_vec(), ..Reply::status(200) }.header("content-type", "image/png"))
        } else {
            None
        }
    });
    // The message names its image, so an agent can ask for it.
    let read = teams.call("read_messages", json!({"conversation": GROUP})).await.unwrap();
    assert_eq!(read["items"][0]["untrusted"]["text"], format!("[image: {object}]"));
    let attachment = teams.module.get_attachment(ME_OID, GROUP, "103", object).await.unwrap();
    assert_eq!((attachment.file_name.as_str(), attachment.content_type.as_str()), ("0-weu-d1-synthetic.png", "image/png"));
    assert_eq!(attachment.data, b"\x89PNG synthetic");
    let fetch = &teams.fake.requests(&format!("/v1/objects/{object}"))[0];
    assert_eq!(fetch.host, "eu-prod.asyncgw.teams.microsoft.com");
    assert_eq!(fetch.audience().as_deref(), Some("https://ic3.teams.office.com"));
    // Only what the message references.
    let other = teams.module.get_attachment(ME_OID, GROUP, "103", "0-weu-d1-other").await;
    assert!(matches!(module_error(other), ModuleError::InvalidArgs(_)));
    assert_eq!(teams.fake.requests("/v1/objects/").len(), 1);
}

// Task 4: writes and targets.

/// Writes the fake saw: anything but a GET, apart from sign-in.
fn writes(fake: &Fake) -> Vec<fake::Seen> {
    fake.seen().into_iter().filter(|s| s.method != "GET" && s.path != AUTHZ_PATH).collect()
}

#[tokio::test]
async fn send_posts_the_live_accepted_body() {
    let teams = Teams::signed_in().await;
    let path = messages_path(GROUP);
    teams.fake.on(move |s| {
        (s.path == path && s.method == "POST").then(|| {
            Reply::json(json!({"OriginalArrivalTime": T0}))
                .with_status(201)
                .header("Location", &format!("{CHAT}/v1/users/ME/conversations/{GROUP}/messages/1800000000999"))
        })
    });
    let result = teams.call("send", json!({"conversation": GROUP, "text": "**hi** <there>"})).await.unwrap();
    assert_eq!(result, json!({"id": "1800000000999"}));
    let sent = writes(&teams.fake);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str(), sent[0].host.as_str()),
        ("POST", messages_path(GROUP).as_str(), "teams.microsoft.com")
    );
    assert_eq!(sent[0].audience().as_deref(), Some("https://ic3.teams.office.com"));
    let mut body = sent[0].json();
    let client_message_id = body["clientmessageid"].take();
    assert!(client_message_id.as_str().is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())), "{client_message_id}");
    let properties = json!({"importance": "", "subject": "", "mentions": "[]", "files": "[]", "formatVariant": "TEAMS"});
    let expected = json!({
        "content": "<b>hi</b> &lt;there&gt;",
        "messagetype": "RichText/Html",
        "contenttype": "Text",
        "imdisplayname": ME_NAME,
        "clientmessageid": null,
        "properties": properties,
    });
    assert_eq!(body, expected);
}

#[tokio::test]
async fn reply_goes_to_the_channel_thread() {
    let teams = Teams::signed_in().await;
    let old_channel = "19:dddddddddddddddddddddddddddddddd@thread.skype";
    teams.fake.on(move |s| {
        if s.method == "POST" {
            return Some(Reply::status(201));
        }
        // 101 is itself a reply in the thread of 100; 100 is a root.
        let (conversation, id) = s.path.strip_prefix(&format!("{CHAT_PATH}/"))?.split_once("/messages/")?;
        let mut replied = message(id, conversation, OTHER, "<p>x</p>", T0);
        replied["rootMessageId"] = "1800000000100".into();
        Some(Reply::json(replied))
    });
    for reply_to in ["1800000000100", "1800000000101"] {
        teams.call("send", json!({"conversation": CHANNEL, "text": "ok", "reply_to": reply_to})).await.unwrap();
    }
    teams.call("send", json!({"conversation": old_channel, "text": "ok", "reply_to": "1800000000100"})).await.unwrap();
    let paths: Vec<String> = writes(&teams.fake).into_iter().map(|s| s.path).collect();
    let thread = |c: &str| format!("{CHAT_PATH}/{c};messageid=1800000000100/messages");
    assert_eq!(paths, [thread(CHANNEL), thread(CHANNEL), thread(old_channel)]);
    // A quote reply in a chat has no confirmed format yet: refused, nothing sent.
    let quote = teams.call("send", json!({"conversation": GROUP, "text": "ok", "reply_to": "1800000000100"})).await;
    assert!(matches!(module_error(quote), ModuleError::NotSupported(_)));
    assert_eq!(writes(&teams.fake).len(), 3);
}

#[tokio::test]
async fn edit_puts_the_message() {
    let teams = Teams::signed_in().await;
    teams.fake.on(|s| (s.method == "PUT").then(|| Reply::status(200)));
    teams.call("edit_message", json!({"conversation": GROUP, "message": "1800000000300", "text": "_fixed_"})).await.unwrap();
    let sent = writes(&teams.fake);
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].method.as_str(), sent[0].path.as_str()), ("PUT", format!("{}/1800000000300", messages_path(GROUP)).as_str()));
    assert_eq!(sent[0].json(), json!({"content": "<i>fixed</i>", "messagetype": "RichText/Html", "contenttype": "Text"}));
    // A refusal is the platform's error, not a success.
    teams.fake.on(|s| (s.method == "PUT").then(|| Reply::status(403)));
    let refused = teams.call("edit_message", json!({"conversation": GROUP, "message": "1800000000300", "text": "x"})).await;
    assert!(matches!(module_error(refused), ModuleError::Platform(_)));
}

#[tokio::test]
async fn react_writes_only_emotions() {
    let teams = Teams::signed_in().await;
    teams.fake.on(|s| (s.method == "PUT").then(|| Reply::status(200)));
    let before = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap();
    for reaction in ["like", "\u{2764}\u{fe0f}"] {
        teams.call("react", json!({"conversation": GROUP, "message": "1800000000300", "reaction": reaction})).await.unwrap();
    }
    let sent = writes(&teams.fake);
    assert_eq!(sent.len(), 2);
    for (request, key) in sent.iter().zip(["like", "heart"]) {
        assert_eq!((request.method.as_str(), request.query.as_str()), ("PUT", "name=emotions"));
        assert_eq!(request.path, format!("{}/1800000000300/properties", messages_path(GROUP)));
        let body = request.json();
        assert_eq!(body["emotions"]["key"], key);
        assert!(body["emotions"]["value"].as_i64().unwrap() >= before);
        assert_eq!(body.as_object().unwrap().len(), 1);
    }
}

/// Answers the conversation, its members and one message, for describe-target.
fn target_server(conversation: &'static str, topic: Option<&'static str>, members: Value) -> impl Fn(&fake::Seen) -> Option<Reply> {
    move |s| {
        if s.path == format!("{CHAT_PATH}/{conversation}") {
            Some(Reply::json(fake::conversation(conversation, topic, None)))
        } else if s.path == format!("/api/chatsvc/emea/v1/threads/{conversation}/members") {
            Some(Reply::json(members.clone()))
        } else if s.path == format!("{}/1800000000300", messages_path(conversation)) {
            Some(Reply::json(message("1800000000300", conversation, OTHER, "<p>the <b>target</b></p>", T0)))
        } else if s.path == messages_path(conversation) {
            Some(Reply::json(page("messages", vec![message("1800000000300", conversation, OTHER, "<p>hi</p>", T0)], None)))
        } else {
            None
        }
    }
}

#[tokio::test]
async fn describe_target_lists_members() {
    let teams = Teams::signed_in().await;
    // The live shape has no names ("Live results" 7): they come from the newest messages, and a
    // member who wrote none of them stays an MRI.
    let members = json!({"members": [
        {"id": ME, "role": "Admin", "friendlyName": ME_NAME},
        {"id": OTHER, "role": "User"},
        {"id": "8:orgid:00000000-0000-0000-0000-000000000003", "role": "User"},
    ]});
    teams.fake.on(target_server(GROUP, Some("Synthetic group"), members.clone()));
    let target = teams.module.describe_target(ME_OID, GROUP, None).await.unwrap();
    assert_eq!(target.conversation_title, "Synthetic group");
    let participants: Vec<(&str, &str)> = target.participants.iter().map(|p| (p.id.as_str(), p.display_name.as_str())).collect();
    let third = "8:orgid:00000000-0000-0000-0000-000000000003";
    assert_eq!(participants, [(ME, ME_NAME), (OTHER, "Synthetic Person Two"), (third, third)]);
    assert_eq!(target.target_text, None);
    // Reads only.
    assert!(writes(&teams.fake).is_empty());

    // The names are a help, not a condition: messages that cannot be read leave the MRI.
    let roster = target_server(GROUP, Some("Synthetic group"), members);
    teams.fake.on(move |s| if s.path == messages_path(GROUP) { Some(Reply::status(403)) } else { roster(s) });
    let unnamed = teams.module.describe_target(ME_OID, GROUP, None).await.unwrap();
    assert_eq!(unnamed.participants.get(1).map(|p| p.display_name.as_str()), Some(OTHER));

    // A channel is the target itself: no roster.
    teams.fake.on(target_server(CHANNEL, Some("Synthetic channel"), json!({"members": [{"id": ME}]})));
    let channel = teams.module.describe_target(ME_OID, CHANNEL, None).await.unwrap();
    assert_eq!((channel.conversation_title.as_str(), channel.participants.len()), ("Synthetic channel", 0));
}

#[tokio::test]
async fn describe_target_does_not_hide_a_needed_sign_in() {
    let teams = Teams::signed_in().await;
    let roster = target_server(GROUP, Some("Synthetic group"), json!({"members": [{"id": ME}, {"id": OTHER}]}));
    teams.fake.on(move |s| if s.path == messages_path(GROUP) { Some(Reply::status(401)) } else { roster(s) });
    assert!(matches!(module_error(teams.module.describe_target(ME_OID, GROUP, None).await), ModuleError::AuthRequired(_)));
}

#[tokio::test]
async fn describe_target_reads_the_target_text() {
    let teams = Teams::signed_in().await;
    teams.fake.on(target_server(GROUP, Some("Synthetic group"), json!({"members": [{"id": ME}, {"id": OTHER}]})));
    let target = teams.module.describe_target(ME_OID, GROUP, Some("1800000000300")).await.unwrap();
    assert_eq!(target.target_text.as_deref(), Some("the **target**"));
    let missing = teams.module.describe_target(ME_OID, GROUP, Some("1800000000301")).await;
    assert!(matches!(module_error(missing), ModuleError::Platform(_)));
    assert!(writes(&teams.fake).is_empty());
}

#[tokio::test]
async fn timeout_after_post_is_uncertain() {
    // The answer comes after the host's read timeout (1 s in tests), or the service fails: the
    // message may have been taken, and it is never sent twice.
    let replies = [
        Reply::status(201).delayed(Duration::from_millis(1500)),
        Reply::status(503),
        // Asking for a long wait does not make a 5xx a refusal (a 429 is one: see below).
        Reply::status(503).header("Retry-After", "1000"),
        Reply::status(500).header("Retry-After", "Fri, 01 Jan 2100 00:00:00 GMT"),
        // 507 is never retried, and still may follow work done.
        Reply::status(507),
    ];
    for reply in replies {
        let teams = Teams::signed_in().await;
        let reply = std::sync::Mutex::new(Some(reply));
        teams.fake.on(move |s| (s.method == "POST").then(|| reply.lock().unwrap().take().unwrap_or_else(|| Reply::status(201))));
        let result = teams.call("send", json!({"conversation": GROUP, "text": "once"})).await;
        assert!(matches!(module_error(result), ModuleError::Uncertain(_)));
        assert_eq!(writes(&teams.fake).len(), 1);
    }
    // A 429 asking for a long wait was not taken: it fails, and may be sent again.
    let teams = Teams::signed_in().await;
    teams.fake.on(|s| (s.method == "POST").then(|| Reply::status(429).header("Retry-After", "1000")));
    let refused = teams.call("send", json!({"conversation": GROUP, "text": "once"})).await;
    assert!(matches!(module_error(refused), ModuleError::Platform(_)));
}

#[tokio::test]
async fn one_to_one_id_checks_both_orders() {
    let teams = Teams::signed_in().await;
    let other_oid = OTHER.trim_start_matches("8:orgid:");
    let (mine_first, theirs_first) = (format!("19:{ME_OID}_{other_oid}@unq.gbl.spaces"), format!("19:{other_oid}_{ME_OID}@unq.gbl.spaces"));
    let existing = theirs_first.clone();
    teams.fake.on(move |s| {
        if s.method == "POST" {
            Some(Reply::status(201))
        } else if s.path == format!("{CHAT_PATH}/{existing}") {
            Some(Reply::json(fake::conversation(&existing, None, None)))
        } else if s.path.contains("/members") {
            Some(Reply::json(json!({"members": [{"id": ME}, {"id": OTHER, "friendlyName": "Synthetic Person Two"}]})))
        } else {
            None
        }
    });
    // A person's id finds the 1:1 chat with them, whichever order its id has.
    let found = teams.call("list_conversations", json!({"query": OTHER})).await.unwrap();
    assert_eq!(ids(&found, "id"), std::slice::from_ref(&theirs_first));
    assert_eq!(found["items"][0]["kind"], "direct");
    let channels = teams.call("list_conversations", json!({"query": OTHER, "kind": "channel"})).await.unwrap();
    assert_eq!(ids(&channels, "id"), Vec::<String>::new());
    let probes: Vec<String> = teams.fake.seen().into_iter().filter(|s| s.method == "GET").map(|s| s.path).take(2).collect();
    assert_eq!(probes, [format!("{CHAT_PATH}/{mine_first}"), format!("{CHAT_PATH}/{theirs_first}")]);
    // No chat with this person yet: nothing found.
    let none = teams.call("list_conversations", json!({"query": "8:orgid:00000000-0000-0000-0000-000000000004"})).await.unwrap();
    assert_eq!(ids(&none, "id"), Vec::<String>::new());

    // A person's id never stands in for the chat in a write or its approval: the core's
    // permissions for the chat apply to the chat's id only.
    let send = teams.call("send", json!({"conversation": OTHER, "text": "hi"})).await;
    assert!(matches!(module_error(send), ModuleError::InvalidArgs(_)));
    assert!(matches!(module_error(teams.module.describe_target(ME_OID, OTHER, None).await), ModuleError::InvalidArgs(_)));
    assert!(writes(&teams.fake).is_empty());
}

// Task 5: polling events.

fn now_ms() -> i64 {
    i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

/// The next message event, if a poll that starts after this is called reports one. A poll reports
/// at its end, however long it took, and polls run one at a time: once a second poll has started,
/// the first has reported everything.
async fn next_message(events: &mut mpsc::Receiver<HostEvent>, fake: &Fake) -> Option<NewMessage> {
    // A poll starts with the list's first page; the next pages carry a `syncState`.
    let polls = || fake.seen().iter().filter(|s| s.path == CHAT_PATH && !s.query.contains("syncState")).count();
    let first = polls() + 1;
    // Polls stopped (the module crashed): give up once two would have run.
    let give_up = Instant::now() + 2 * (Duration::from_millis(POLL.into()) + limits::for_tests().call_timeout);
    loop {
        let reported = polls() > first;
        match tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
            Ok(Some(HostEvent::MessageArrived(message))) => return Some(message),
            Ok(Some(_)) => {}
            Ok(None) => return None,
            Err(_) if reported || Instant::now() >= give_up => return None,
            Err(_) => {}
        }
    }
}

#[tokio::test]
async fn new_message_emits_an_event() {
    let mut teams = Teams::polling().await;
    let started = now_ms();
    let last = message("1900000000001", GROUP, OTHER, "<p>new</p>", started + 1000);
    teams.fake.on(conversations_page(vec![conversation(GROUP, Some("Synthetic group"), Some(last))]));
    let event = teams.next_message().await.unwrap();
    let expected = NewMessage {
        module: "teams".to_string(),
        account: ME_OID.to_string(),
        conversation: GROUP.to_string(),
        message: "1900000000001".to_string(),
        sender: OTHER.to_string(),
        own: false,
        mentions_me: false,
        direct: false,
    };
    assert_eq!(event, expected);
    // Once only, and polling only reads: the list of conversations updated lately.
    assert_eq!(teams.next_message().await, None);
    let polls: Vec<_> = teams.fake.seen().into_iter().filter(|s| s.path == CHAT_PATH).collect();
    assert!(!polls.is_empty());
    for poll in &polls {
        let start: i64 = poll.query.split("startTime=").nth(1).unwrap().split('&').next().unwrap().parse().unwrap();
        assert!(start > started - 10 * 60_000 && start < started + 60_000, "{}", poll.query);
        // Background requests, as the official client marks them (spec 9.2).
        assert_eq!(poll.header("x-ms-request-priority"), Some("20"));
    }
    assert!(teams.fake.seen().iter().all(|s| s.method == "GET" || s.path == AUTHZ_PATH));
}

#[tokio::test]
async fn messages_between_polls_all_arrive() {
    // A mention, then another message, in one interval: the list shows only the last one, and the
    // poll reads the conversation for the rest.
    let mut teams = Teams::polling().await;
    let at = now_ms() + 1000;
    let mention = mentioning(message("1900000000011", GROUP, OTHER, "<p>hey One</p>", at), ME);
    let plain = message("1900000000012", GROUP, OTHER, "<p>and more</p>", at + 1);
    let (list, path) = (vec![conversation(GROUP, None, Some(plain.clone()))], messages_path(GROUP));
    teams.fake.on(move |s| match s.path.as_str() {
        CHAT_PATH => Some(Reply::json(page("conversations", list.clone(), None))),
        p if p == path => Some(Reply::json(page("messages", vec![plain.clone(), mention.clone()], None))),
        _ => None,
    });
    let first = teams.next_message().await.unwrap();
    let second = teams.next_message().await.unwrap();
    assert_eq!((first.message.as_str(), first.mentions_me), ("1900000000011", true));
    assert_eq!((second.message.as_str(), second.mentions_me), ("1900000000012", false));
    assert_eq!(teams.next_message().await, None);
}

#[tokio::test]
async fn many_messages_between_polls_all_arrive() {
    // More new messages than one page: the poll reads on, back to the last one it knew.
    let mut teams = Teams::polling().await;
    let at = now_ms() + 1000;
    let path = messages_path(GROUP);
    let older = format!("{CHAT}/v1/users/ME/conversations/{GROUP}/messages?view=msnp24Equivalent&pageSize=50&startTime=1&syncState=older");
    let mention = mentioning(message("1900000000031", GROUP, OTHER, "<p>hey One</p>", at), ME);
    let newest = message("1900000000032", GROUP, OTHER, "<p>more</p>", at + 1);
    let list = vec![conversation(GROUP, None, Some(newest.clone()))];
    teams.fake.on(move |s| match s.path.as_str() {
        CHAT_PATH => Some(Reply::json(page("conversations", list.clone(), None))),
        p if p == path && s.query.contains("syncState=older") => Some(Reply::json(page("messages", vec![mention.clone()], None))),
        p if p == path => Some(Reply::json(page("messages", vec![newest.clone()], Some(&older)))),
        _ => None,
    });
    let first = teams.next_message().await.unwrap();
    assert_eq!((first.message.as_str(), first.mentions_me), ("1900000000031", true));
    assert_eq!(teams.next_message().await.unwrap().message, "1900000000032");
    assert_eq!(teams.next_message().await, None);
}

#[tokio::test]
async fn more_new_pages_than_a_poll_reads_arrive_with_the_next() {
    // One message a page, one page more than a poll reads: the oldest, a mention, comes with the
    // next poll, from where the first one stopped.
    const PAGES: usize = 21;
    let mut teams = Teams::polling().await;
    let at = now_ms() + 1000;
    let messages: Vec<Value> = (0..PAGES)
        .map(|i| {
            let m = message(&format!("19000000001{i:02}"), GROUP, OTHER, "<p>x</p>", at + i64::try_from(i).unwrap());
            if i == 0 { mentioning(m, ME) } else { m }
        })
        .collect();
    let (list, path) = (vec![conversation(GROUP, None, Some(messages[PAGES - 1].clone()))], messages_path(GROUP));
    teams.fake.on(move |s| match s.path.as_str() {
        CHAT_PATH => Some(Reply::json(page("conversations", list.clone(), None))),
        p if p == path => {
            // Page k (0 the newest) holds message PAGES - 1 - k.
            let k: usize = s.query.split("syncState=p").nth(1).map_or(0, |k| k.split('&').next().unwrap().parse().unwrap());
            let next =
                format!("{CHAT}/v1/users/ME/conversations/{GROUP}/messages?view=msnp24Equivalent&pageSize=50&startTime=1&syncState=p{}", k + 1);
            Some(Reply::json(page("messages", vec![messages[PAGES - 1 - k].clone()], (k + 1 < PAGES).then_some(next.as_str()))))
        }
        _ => None,
    });
    let mut arrived = Vec::new();
    while let Some(event) = teams.next_message().await {
        arrived.push(event);
    }
    let mut ids: Vec<&str> = arrived.iter().map(|event| event.message.as_str()).collect();
    assert_eq!(ids.len(), PAGES, "{ids:?}");
    assert_eq!(arrived.last().map(|event| (event.message.as_str(), event.mentions_me)), Some(("1900000000100", true)));
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), PAGES);
    assert!(teams.fake.requests(&messages_path(GROUP)).iter().any(|s| s.query.contains(&format!("syncState=p{}", PAGES - 1))));
}

#[tokio::test]
async fn new_messages_go_out_before_older_pages() {
    // A group with one page more than a poll reads; once its last page can be read, a channel has
    // a new message too: the new message is reported first, then the older page.
    const PAGES: usize = 21;
    let mut teams = Teams::polling().await;
    let at = now_ms() + 1000;
    let messages: Vec<Value> =
        (0..PAGES).map(|i| message(&format!("19000000001{i:02}"), GROUP, OTHER, "<p>x</p>", at + i64::try_from(i).unwrap())).collect();
    let fresh = message("1900000000200", CHANNEL, OTHER, "<p>new</p>", at + 100);
    // The list, and whether the group's last page answers: from the first list with the channel on.
    let shared = Arc::new(std::sync::Mutex::new((vec![conversation(GROUP, None, Some(messages[PAGES - 1].clone()))], false)));
    let (state, group, channel, answer) = (shared.clone(), messages_path(GROUP), messages_path(CHANNEL), fresh.clone());
    teams.fake.on(move |s| {
        let mut state = state.lock().unwrap();
        if s.path == CHAT_PATH {
            state.1 = state.0.len() > 1;
            return Some(Reply::json(page("conversations", state.0.clone(), None)));
        }
        if s.path == channel {
            return Some(Reply::json(page("messages", vec![answer.clone()], None)));
        }
        (s.path == group).then(|| {
            let k: usize = s.query.split("syncState=p").nth(1).map_or(0, |k| k.split('&').next().unwrap().parse().unwrap());
            if k == PAGES - 1 && !state.1 {
                // Failing for now: the page waits for a later poll.
                return Reply::status(400);
            }
            let next =
                format!("{CHAT}/v1/users/ME/conversations/{GROUP}/messages?view=msnp24Equivalent&pageSize=50&startTime=1&syncState=p{}", k + 1);
            Reply::json(page("messages", vec![messages[PAGES - 1 - k].clone()], (k + 1 < PAGES).then_some(next.as_str())))
        })
    });
    for _ in 0..PAGES - 1 {
        teams.next_message().await.unwrap();
    }
    shared.lock().unwrap().0.push(conversation(CHANNEL, None, Some(fresh)));
    let next = [teams.next_message().await.unwrap().message, teams.next_message().await.unwrap().message];
    assert_eq!(next, ["1900000000200", "1900000000100"]);
}

#[tokio::test]
async fn older_pages_are_read_a_few_a_poll_and_a_failing_one_holds_none_back() {
    // Two conversations with eight pages more than a poll reads (one message a page). The group's
    // older pages keep failing; the channel's are read, five pages a poll at most, and all arrive.
    const PAGES: usize = 28;
    let mut teams = Teams::polling().await;
    let at = now_ms() + 1000;
    let numbered_in = |conversation: &str, base: i64| -> Vec<Value> {
        (0..PAGES)
            .map(|i| {
                let i = i64::try_from(i).unwrap();
                message(&(base + i).to_string(), conversation, OTHER, "<p>x</p>", at + i)
            })
            .collect()
    };
    let (in_group, in_channel) = (numbered_in(GROUP, 1_900_000_000_100), numbered_in(CHANNEL, 1_900_000_000_300));
    let list = vec![conversation(GROUP, None, Some(in_group[PAGES - 1].clone())), conversation(CHANNEL, None, Some(in_channel[PAGES - 1].clone()))];
    let (group, channel) = (messages_path(GROUP), messages_path(CHANNEL));
    teams.fake.on(move |s| {
        if s.path == CHAT_PATH {
            return Some(Reply::json(page("conversations", list.clone(), None)));
        }
        let (messages, id) = if s.path == group {
            (&in_group, GROUP)
        } else if s.path == channel {
            (&in_channel, CHANNEL)
        } else {
            return None;
        };
        let k: usize = s.query.split("syncState=p").nth(1).map_or(0, |k| k.split('&').next().unwrap().parse().unwrap());
        if id == GROUP && k >= 20 {
            return Some(Reply::status(400));
        }
        let next = format!("{CHAT}/v1/users/ME/conversations/{id}/messages?view=msnp24Equivalent&pageSize=50&startTime=1&syncState=p{}", k + 1);
        Some(Reply::json(page("messages", vec![messages[PAGES - 1 - k].clone()], (k + 1 < PAGES).then_some(next.as_str()))))
    });
    let mut arrived = Vec::new();
    while let Some(event) = teams.next_message().await {
        arrived.push(event.message);
    }
    // All of the channel's; of the group's, the newest twenty.
    assert_eq!(arrived.iter().filter(|id| id.starts_with("19000000003")).count(), PAGES, "{arrived:?}");
    assert_eq!(arrived.iter().filter(|id| id.starts_with("19000000001")).count(), 20, "{arrived:?}");
    // Older pages (20 and on) asked for between two lists, that is in one poll: five at most.
    let mut per_poll = vec![0];
    for s in teams.fake.seen() {
        let older = s.query.split("syncState=p").nth(1).and_then(|k| k.split('&').next()?.parse::<usize>().ok()).is_some_and(|k| k >= 20);
        if s.path == CHAT_PATH {
            per_poll.push(0);
        } else if older {
            *per_poll.last_mut().unwrap() += 1;
        }
    }
    assert!(per_poll.iter().all(|pages| *pages <= 5), "{per_poll:?}");
}

#[tokio::test]
async fn a_call_after_a_mention_does_not_hide_it() {
    // The list's last item is a call, which is no text message: the mention before it still comes.
    let mut teams = Teams::polling().await;
    let at = now_ms() + 1000;
    let mention = mentioning(message("1900000000051", GROUP, OTHER, "<p>hey One</p>", at), ME);
    let mut call = message("1900000000052", GROUP, OTHER, "<partlist/>", at + 1);
    call["messagetype"] = "Event/Call".into();
    let (list, path) = (vec![conversation(GROUP, None, Some(call.clone()))], messages_path(GROUP));
    teams.fake.on(move |s| match s.path.as_str() {
        CHAT_PATH => Some(Reply::json(page("conversations", list.clone(), None))),
        p if p == path => Some(Reply::json(page("messages", vec![call.clone(), mention.clone()], None))),
        _ => None,
    });
    let event = teams.next_message().await.unwrap();
    assert_eq!((event.message.as_str(), event.mentions_me), ("1900000000051", true));
    assert_eq!(teams.next_message().await, None);
}

#[tokio::test]
async fn a_conversation_that_fails_waits_without_the_others() {
    // One conversation's messages fail: the other's arrive at once, and the failing one's once it
    // can be read, nothing lost in between.
    let mut teams = Teams::polling().await;
    let at = now_ms() + 1000;
    let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let fail = failing.clone();
    let (group, channel) = (messages_path(GROUP), messages_path(CHANNEL));
    let in_group = message("1900000000041", GROUP, OTHER, "<p>a</p>", at);
    let mention = mentioning(message("1900000000042", CHANNEL, OTHER, "<p>hey One</p>", at), ME);
    let in_channel = message("1900000000043", CHANNEL, OTHER, "<p>b</p>", at + 1);
    let list = vec![conversation(GROUP, None, Some(in_group.clone())), conversation(CHANNEL, None, Some(in_channel.clone()))];
    teams.fake.on(move |s| match s.path.as_str() {
        CHAT_PATH => Some(Reply::json(page("conversations", list.clone(), None))),
        p if p == group => Some(Reply::json(page("messages", vec![in_group.clone()], None))),
        p if p == channel && fail.load(Ordering::SeqCst) => Some(Reply::status(400)),
        p if p == channel => Some(Reply::json(page("messages", vec![in_channel.clone(), mention.clone()], None))),
        _ => None,
    });
    assert_eq!(teams.next_message().await.unwrap().message, "1900000000041");
    assert_eq!(teams.next_message().await, None);
    failing.store(false, Ordering::SeqCst);
    let first = teams.next_message().await.unwrap();
    assert_eq!((first.message.as_str(), first.mentions_me), ("1900000000042", true));
    assert_eq!(teams.next_message().await.unwrap().message, "1900000000043");
    assert_eq!(teams.next_message().await, None);
}

#[tokio::test]
async fn a_failed_poll_is_tried_again() {
    // The second page of the list fails: nothing is reported, and nothing is skipped either.
    let mut teams = Teams::polling().await;
    let at = now_ms() + 1000;
    let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let fail = failing.clone();
    let next = format!("{CHAT}/v1/users/ME/conversations?view=msnp24Equivalent&pageSize=50&startTime=1&syncState=two");
    let first_page = vec![conversation(GROUP, None, Some(message("1900000000021", GROUP, OTHER, "<p>a</p>", at)))];
    let second_page = vec![conversation(CHANNEL, None, Some(message("1900000000022", CHANNEL, OTHER, "<p>b</p>", at)))];
    teams.fake.on(move |s| {
        (s.path == CHAT_PATH).then(|| {
            if !s.query.contains("syncState=two") {
                Reply::json(page("conversations", first_page.clone(), Some(&next)))
            } else if fail.load(Ordering::SeqCst) {
                Reply::status(403)
            } else {
                Reply::json(page("conversations", second_page.clone(), None))
            }
        })
    });
    assert_eq!(teams.next_message().await, None);
    failing.store(false, Ordering::SeqCst);
    let mut arrived = vec![teams.next_message().await.unwrap().message, teams.next_message().await.unwrap().message];
    arrived.sort();
    assert_eq!(arrived, ["1900000000021", "1900000000022"]);
}

#[tokio::test]
async fn own_messages_are_flagged() {
    let mut teams = Teams::polling().await;
    let mine = message("1900000000002", GROUP, ME, "<p>mine</p>", now_ms() + 1000);
    teams.fake.on(conversations_page(vec![conversation(GROUP, None, Some(mine))]));
    let event = teams.next_message().await.unwrap();
    assert_eq!((event.message.as_str(), event.sender.as_str(), event.own), ("1900000000002", ME, true));
    assert_eq!(teams.next_message().await, None);
}

#[tokio::test]
async fn mentions_and_direct_are_set() {
    let mut teams = Teams::polling().await;
    let at = now_ms() + 1000;
    let direct = message("1900000000003", DIRECT, OTHER, "<p>hi</p>", at);
    let mention = mentioning(message("1900000000004", GROUP, OTHER, "<p>hey One</p>", at), ME);
    teams.fake.on(conversations_page(vec![conversation(DIRECT, None, Some(direct)), conversation(GROUP, None, Some(mention))]));
    let mut events = [teams.next_message().await.unwrap(), teams.next_message().await.unwrap()];
    events.sort_by(|a, b| a.message.cmp(&b.message));
    let flags: Vec<_> = events.iter().map(|e| (e.conversation.as_str(), e.direct, e.mentions_me, e.own)).collect();
    assert_eq!(flags, [(DIRECT, true, false, false), (GROUP, false, true, false)]);
}

#[tokio::test]
async fn restart_does_not_replay_old_messages() {
    let data = tempfile::tempdir().unwrap();
    let (fake, wam) = (Fake::start().await, Wam::new());
    let listed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let list = listed.clone();
    fake.on(move |s| (s.path == CHAT_PATH).then(|| Reply::json(page("conversations", list.lock().unwrap().clone(), None))));
    {
        let (module, mut events) = load(&fake, &wam, data.path(), Some(POLL), None).await;
        module.sign_in().await.unwrap();
        // Sent after the sign-in, which starts the count, however long loading the module took.
        let old = conversation(GROUP, None, Some(message("1900000000005", GROUP, OTHER, "<p>old</p>", now_ms() + 1000)));
        *listed.lock().unwrap() = vec![old];
        assert_eq!(next_message(&mut events, &fake).await.map(|m| m.message).as_deref(), Some("1900000000005"));
    }
    // A new instance, as after a crash or a daemon restart: the same storage, nothing in memory.
    let (module, mut events) = load(&fake, &wam, data.path(), Some(POLL), None).await;
    module.restore(ME_OID).await.unwrap();
    assert_eq!(next_message(&mut events, &fake).await, None);
    let new = conversation(GROUP, None, Some(message("1900000000006", GROUP, OTHER, "<p>new</p>", now_ms() + 61_000)));
    *listed.lock().unwrap() = vec![new];
    assert_eq!(next_message(&mut events, &fake).await.map(|m| m.message).as_deref(), Some("1900000000006"));
}

#[tokio::test]
async fn sign_out_stops_polling() {
    let teams = Teams::polling().await;
    teams.fake.on(conversations_page(Vec::new()));
    for _ in 0..20 {
        if !teams.fake.requests(CHAT_PATH).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!teams.fake.requests(CHAT_PATH).is_empty(), "the module polls once signed in");
    teams.module.sign_out(ME_OID).await.unwrap();
    let polled = teams.fake.requests(CHAT_PATH).len();
    tokio::time::sleep(Duration::from_millis(u64::from(POLL) * 4)).await;
    assert_eq!(teams.fake.requests(CHAT_PATH).len(), polled);
}

// Stage 2: meetings (spec 2026-10-28-meetings-transcripts, 4.1).

const CALENDAR_PATH: &str = "/api/mt/emea/v2.0/me/calendars/default/calendarView";

/// A middle-tier event of the meeting chat `MEETING`, synthetic values ("Live results" 10).
fn calendar_event(start: &str) -> Value {
    json!({
        "objectId": "AAMkSyntheticEvent=",
        "iCalUID": "040000008200E00074C5B7101A82E00800000000",
        "startTime": start,
        "endTime": "2027-01-15T10:00:00+01:00",
        "eventType": "Occurrence",
        "myResponseType": "Organizer",
        "subject": "Synthetic planning",
        "organizerName": ME_NAME,
        "skypeTeamsMeetingUrl": "https://teams.microsoft.com/l/meetup-join/19%3ameeting_c3ludGhldGlj%40thread.v2/0?context=%7b%7d",
    })
}

fn calendar_answer(events: Vec<Value>) -> impl Fn(&fake::Seen) -> Option<Reply> {
    move |s| (s.path == CALENDAR_PATH).then(|| Reply::json(json!({"type": "calendarView", "value": events.clone()})))
}

#[tokio::test]
async fn list_meetings_reads_the_middle_tier_calendar() {
    let teams = Teams::signed_in().await;
    teams.fake.on(calendar_answer(vec![calendar_event("2027-01-15T09:00:00+01:00")]));
    let result = teams.call("list_meetings", json!({"from": "2027-01-14T00:00:00Z", "to": "2027-01-16T00:00:00Z"})).await.unwrap();
    assert_eq!(result["items"].as_array().unwrap().len(), 1);
    let item = &result["items"][0];
    assert_eq!((&item["conversation"], &item["start"], &item["kind"]), (&json!(MEETING), &json!("2027-01-15T08:00:00Z"), &json!("Occurrence")));
    assert_eq!(item["untrusted"]["subject"], "Synthetic planning");
    // One GET, with the authsvc audience's token and the skype token of the exchange.
    let calendar = teams.fake.requests(CALENDAR_PATH);
    assert_eq!(calendar.len(), 1);
    let request = &calendar[0];
    assert_eq!((request.method.as_str(), request.host.as_str()), ("GET", "teams.microsoft.com"));
    assert_eq!(request.audience().as_deref(), Some("https://api.spaces.skype.com"));
    assert_eq!(request.header("x-skypetoken"), Some("synthetic-skype-token"));
    assert_eq!(request.query, "StartDate=2027-01-14T00%3A00%3A00Z&EndDate=2027-01-16T00%3A00%3A00Z&shouldDecryptData=false");
    assert_eq!(teams.fake.requests(AUTHZ_PATH).len(), 1);
}

#[tokio::test]
async fn list_meetings_checks_its_window() {
    let teams = Teams::signed_in().await;
    for args in [
        json!({"from": "2027-01-15T00:00:00Z", "to": "2027-01-15T00:00:00Z"}),
        json!({"from": "2027-01-01T00:00:00Z", "to": "2027-02-02T00:00:00Z"}),
        json!({"from": "yesterday"}),
        json!({"from": 1_800_000_000}),
    ] {
        assert!(matches!(module_error(teams.call("list_meetings", args.clone()).await), ModuleError::InvalidArgs(_)), "{args}");
    }
    assert!(teams.fake.requests(CALENDAR_PATH).is_empty());
    // Left out, the window is a week back to a day ahead.
    teams.fake.on(calendar_answer(Vec::new()));
    teams.call("list_meetings", json!({})).await.unwrap();
    let query = teams.fake.requests(CALENDAR_PATH)[0].query.clone();
    assert!(query.starts_with("StartDate=") && query.contains("&EndDate="), "{query}");
}

#[tokio::test]
async fn an_expiring_skype_token_is_renewed_by_authsvc() {
    let teams = Teams::start().await;
    // The skype token expires in a minute, inside the renewal margin.
    let short = {
        let mut authz = fake::authz();
        authz["tokens"]["expiresIn"] = json!(60);
        authz
    };
    let events = calendar_answer(Vec::new());
    teams.fake.on(move |s| if s.path == AUTHZ_PATH { Some(Reply::json(short.clone())) } else { events(s) });
    teams.module.sign_in().await.unwrap();
    teams.call("list_meetings", json!({})).await.unwrap();
    teams.call("list_meetings", json!({})).await.unwrap();
    // Each call exchanged once more before it read the calendar.
    assert_eq!(teams.fake.requests(AUTHZ_PATH).len(), 3);
    assert_eq!(teams.fake.requests(CALENDAR_PATH).len(), 2);
}

#[tokio::test]
async fn a_middle_tier_401_gets_one_more_exchange() {
    let teams = Teams::signed_in().await;
    let refused = Arc::new(AtomicUsize::new(1));
    let (answer, limit) = (calendar_answer(Vec::new()), refused.clone());
    teams.fake.on(move |s| {
        let reply = answer(s)?;
        Some(if limit.fetch_sub(1, Ordering::SeqCst) > 0 { Reply::status(401) } else { reply })
    });
    teams.call("list_meetings", json!({})).await.unwrap();
    let calendar = teams.fake.requests(CALENDAR_PATH);
    assert_eq!(calendar.len(), 2);
    assert_ne!(calendar[0].header("authorization"), calendar[1].header("authorization"));
    assert_eq!(teams.fake.requests(AUTHZ_PATH).len(), 2);
    // Refused again after that: the account must sign in again.
    refused.store(usize::MAX / 2, Ordering::SeqCst);
    assert!(matches!(module_error(teams.call("list_meetings", json!({})).await), ModuleError::AuthRequired(_)));
}

#[tokio::test]
async fn list_meetings_needs_the_calendar_service() {
    let teams = Teams::start().await;
    let mut authz = fake::authz();
    authz["regionGtms"].as_object_mut().unwrap().remove("middleTier");
    teams.fake.on(move |s| (s.path == AUTHZ_PATH).then(|| Reply::json(authz.clone())));
    teams.module.sign_in().await.unwrap();
    let error = module_error(teams.call("list_meetings", json!({})).await);
    assert!(matches!(&error, ModuleError::NotSupported(why) if why.contains("calendar")), "{error:?}");
}

#[tokio::test]
async fn too_many_meetings_ask_for_a_narrower_window() {
    let teams = Teams::signed_in().await;
    let mut event = calendar_event("2027-01-15T09:00:00+01:00");
    event["subject"] = json!("s".repeat(200));
    teams.fake.on(calendar_answer(vec![event; 4000]));
    let error = module_error(teams.call("list_meetings", json!({})).await);
    assert!(matches!(&error, ModuleError::Platform(why) if why.contains("narrow from/to")), "{error:?}");
}

// Stage 2: transcripts (spec 2026-10-28-meetings-transcripts, 4.2), against the shapes of "Live
// results" 12 with synthetic values.

const MCPS_PATH: &str = "/api/mcps/eu/contents";
const PROPERTIES_PATH: &str = "/api/chatsvc/emea/v1/users/ME/properties";
const SHAREPOINT: &str = "synthetic-my.sharepoint.com";
const TEAM_SITES: &str = "synthetic.sharepoint.com";
const MCPS_SCOPE: &str = "6bc3b958-689b-49f5-9006-36d165f30e00/.default";
const SHAREPOINT_SCOPE: &str = "https://synthetic-my.sharepoint.com/.default";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/teams/tests/fixtures").join(name)).unwrap()
}

fn json_fixture(name: &str) -> Value {
    serde_json::from_slice(&fixture(name)).unwrap()
}

/// MCPS, the user's properties and SharePoint as the live check saw them; `mcps` and `list` change
/// them per test, `list` with the number of the list read (0, 1, ...).
fn transcript_server(
    mcps: impl Fn(&mut Value) + Send + Sync + 'static,
    list: impl Fn(usize, &mut Value) + Send + Sync + 'static,
) -> impl Fn(&fake::Seen) -> Option<Reply> {
    let lists = AtomicUsize::new(0);
    move |s| {
        if s.path == MCPS_PATH {
            let mut body = json_fixture("mcps_contents.json");
            mcps(&mut body);
            Some(Reply::json(body))
        } else if s.path == PROPERTIES_PATH {
            Some(Reply::json(json_fixture("user_properties.json")))
        } else if [SHAREPOINT, TEAM_SITES].contains(&s.host.as_str()) && s.path.ends_with("/media/transcripts") {
            let mut body = json_fixture("odsp_transcripts.json");
            list(lists.fetch_add(1, Ordering::SeqCst), &mut body);
            Some(Reply::json(body))
        } else if [SHAREPOINT, TEAM_SITES].contains(&s.host.as_str()) && s.path.ends_with("/streamContent") {
            Some(Reply { body: fixture("transcript.vtt"), ..Reply::status(200) }.header("content-type", "text/vtt"))
        } else {
            None
        }
    }
}

fn unchanged(_: &mut Value) {}

fn same_list(_: usize, _: &mut Value) {}

fn sharepoint_tokens(teams: &Teams) -> usize {
    teams.wam.calls().iter().filter(|c| c.scopes.iter().any(|s| s.contains("sharepoint"))).count()
}

#[tokio::test]
async fn get_transcript_reads_the_users_own_recording() {
    let teams = Teams::signed_in().await;
    teams.fake.on(transcript_server(unchanged, same_list));
    let result = teams.call("get_transcript", json!({"conversation": MEETING})).await.unwrap();
    let cues = result["untrusted"]["cues"].as_array().unwrap();
    assert_eq!(cues.len(), 3);
    assert_eq!(cues[0], json!({"start": "00:00:01.000", "end": "00:00:04.500", "speaker": "Synthetic Person One", "text": "First synthetic line."}));
    assert_eq!((&result["language"], &result["next_cursor"], &result["reason"]), (&json!("pl-PL"), &Value::Null, &Value::Null));
    assert_eq!(result["items"].as_array().unwrap().len(), 1);
    assert_eq!((&result["items"][0]["start"], &result["items"][0]["selected"]), (&json!("2027-01-15T08:00:00Z"), &json!(true)));
    assert_eq!(result["transcript"], result["items"][0]["transcript"]);
    assert!(result["version"].is_string());
    // Only GETs, each with its own audience; the list read before and after the content.
    let seen: Vec<fake::Seen> = teams.fake.seen().into_iter().filter(|s| s.path != AUTHZ_PATH).collect();
    assert!(seen.iter().all(|s| s.method == "GET"), "{seen:?}");
    let mcps = teams.fake.requests(MCPS_PATH);
    assert_eq!(mcps[0].audience().as_deref(), Some("6bc3b958-689b-49f5-9006-36d165f30e00"));
    assert_eq!(mcps[0].query, "threadId=19%3Ameeting_c3ludGhldGlj%40thread.v2");
    let sharepoint: Vec<&fake::Seen> = seen.iter().filter(|s| s.host == SHAREPOINT).collect();
    assert_eq!(sharepoint.len(), 3);
    assert!(sharepoint.iter().all(|s| s.audience().as_deref() == Some("https://synthetic-my.sharepoint.com")));
    assert_eq!(sharepoint[1].query, "is=1");
    assert!(teams.wam.calls().iter().any(|c| c.scopes == [MCPS_SCOPE] && !c.interactive));
    assert!(teams.wam.calls().iter().any(|c| c.scopes == [SHAREPOINT_SCOPE] && !c.interactive));
    // The OneDrive is looked up once a session.
    teams.call("get_transcript", json!({"conversation": MEETING})).await.unwrap();
    assert_eq!(teams.fake.requests(PROPERTIES_PATH).len(), 1);
}

#[tokio::test]
async fn someone_elses_recording_in_the_organization_is_read() {
    // The owner's decision of 2026-10-30 (spec section 10, D3): every meeting the user is in.
    let teams = Teams::signed_in().await;
    let colleague = "https://synthetic-my.sharepoint.com/personal/person_two_synthetic_example/Documents/Recordings/r.mp4";
    teams.fake.on(transcript_server(move |v| v["resources"][0]["location"] = json!(colleague), same_list));
    let result = teams.call("get_transcript", json!({"conversation": MEETING})).await.unwrap();
    assert_eq!(result["untrusted"]["cues"].as_array().unwrap().len(), 3);
    let onedrive: Vec<fake::Seen> = teams.fake.seen().into_iter().filter(|s| s.host == SHAREPOINT).collect();
    assert_eq!(onedrive.len(), 3);
    assert!(onedrive.iter().all(|s| s.method == "GET" && s.audience().as_deref() == Some("https://synthetic-my.sharepoint.com")));
    // A channel meeting's recording on a team site of the organization.
    teams.fake.on(transcript_server(|v| v["resources"][0]["location"] = json!("https://synthetic.sharepoint.com/sites/team/r.mp4"), same_list));
    let result = teams.call("get_transcript", json!({"conversation": MEETING})).await.unwrap();
    assert_eq!(result["untrusted"]["cues"].as_array().unwrap().len(), 3);
    let team: Vec<fake::Seen> = teams.fake.seen().into_iter().filter(|s| s.host == TEAM_SITES).collect();
    assert_eq!(team.len(), 3);
    assert!(team.iter().all(|s| s.method == "GET" && s.audience().as_deref() == Some("https://synthetic.sharepoint.com")));
    assert!(teams.wam.calls().iter().any(|c| c.scopes == ["https://synthetic.sharepoint.com/.default"] && !c.interactive));
}

#[tokio::test]
async fn transcripts_elsewhere_get_no_token() {
    let teams = Teams::signed_in().await;
    teams.fake.on(transcript_server(|v| v["resources"][0]["location"] = json!("https://other-my.sharepoint.com/personal/x/r.mp4"), same_list));
    let error = module_error(teams.call("get_transcript", json!({"conversation": MEETING})).await);
    assert!(matches!(&error, ModuleError::Platform(why) if why.contains("outside your organization")), "{error:?}");
    teams.fake.on(transcript_server(|v| v["resources"][0]["metadata"]["ownerTenantId"] = json!("00000000-0000-0000-0000-0000000000bb"), same_list));
    let error = module_error(teams.call("get_transcript", json!({"conversation": MEETING})).await);
    assert!(matches!(&error, ModuleError::NotSupported(why) if why.contains("another organization")), "{error:?}");
    assert_eq!(sharepoint_tokens(&teams), 0);
}

#[tokio::test]
async fn more_mcps_pages_are_never_picked_from() {
    let teams = Teams::signed_in().await;
    teams.fake.on(transcript_server(|v| v["paginationInfo"]["hasNextPage"] = json!(true), same_list));
    let error = module_error(teams.call("get_transcript", json!({"conversation": MEETING})).await);
    assert!(matches!(&error, ModuleError::Platform(why) if why.contains("pass ical_uid")), "{error:?}");
    // base64url of 040000008200E00074C5B7101A82E008.
    let ical_uid = "MDQwMDAwMDA4MjAwRTAwMDc0QzVCNzEwMUE4MkUwMDg";
    let error = module_error(teams.call("get_transcript", json!({"conversation": MEETING, "ical_uid": ical_uid})).await);
    assert!(matches!(error, ModuleError::Incompatible(_)), "{error:?}");
    assert!(teams.fake.requests(MCPS_PATH)[1].query.ends_with("&iCalUid=040000008200E00074C5B7101A82E008"));
    assert_eq!(sharepoint_tokens(&teams), 0);
}

#[tokio::test]
async fn a_transcript_that_changes_is_not_passed_off_as_the_old_one() {
    let teams = Teams::signed_in().await;
    teams.fake.on(transcript_server(unchanged, |n, v| v["value"][0]["cTag"] = json!(format!("c:{n}"))));
    let error = module_error(teams.call("get_transcript", json!({"conversation": MEETING})).await);
    assert!(matches!(&error, ModuleError::Platform(why) if why.contains("transcript changed")), "{error:?}");
    // A later page of a version that is gone.
    teams.fake.on(transcript_server(unchanged, same_list));
    let first = teams.call("get_transcript", json!({"conversation": MEETING})).await.unwrap();
    let stale = json!({"conversation": MEETING, "transcript": first["transcript"], "version": "c3RhbGU", "cursor": 1});
    let error = module_error(teams.call("get_transcript", stale).await);
    assert!(matches!(&error, ModuleError::Platform(why) if why.contains("transcript changed")), "{error:?}");
}

#[tokio::test]
async fn pages_follow_the_cursor_of_one_version() {
    let teams = Teams::signed_in().await;
    teams.fake.on(transcript_server(unchanged, same_list));
    let first = teams.call("get_transcript", json!({"conversation": MEETING, "max_bytes": 4096})).await.unwrap();
    assert_eq!(first["next_cursor"], Value::Null);
    // A cursor needs the transcript and version it pages through.
    for args in [
        json!({"conversation": MEETING, "cursor": 1}),
        json!({"conversation": MEETING, "cursor": 1, "transcript": first["transcript"]}),
        json!({"conversation": MEETING, "max_bytes": 100}),
        json!({"conversation": MEETING, "transcript": "not+a+token"}),
        json!({"conversation": GROUP}),
        json!({"conversation": MEETING, "start": "yesterday"}),
        json!({"conversation": MEETING, "start": "2027-01-15T09:00:00Z", "end": "2027-01-15T08:00:00Z"}),
    ] {
        assert!(matches!(module_error(teams.call("get_transcript", args.clone()).await), ModuleError::InvalidArgs(_)), "{args}");
    }
    let next = json!({"conversation": MEETING, "transcript": first["transcript"], "version": first["version"], "cursor": 2});
    let page = teams.call("get_transcript", next).await.unwrap();
    assert_eq!(page["untrusted"]["cues"].as_array().unwrap().len(), 1);
    assert_eq!(page["untrusted"]["cues"][0]["text"], "Third.");
}

#[tokio::test]
async fn no_transcript_and_not_ready_are_answers() {
    let teams = Teams::signed_in().await;
    teams.fake.on(transcript_server(|v| v["resources"].as_array_mut().unwrap().retain(|r| r["type"] != "TranscriptV2"), same_list));
    let result = teams.call("get_transcript", json!({"conversation": MEETING})).await.unwrap();
    assert_eq!((&result["reason"], &result["items"]), (&json!("no transcript"), &json!([])));
    teams.fake.on(transcript_server(unchanged, |_, v| v["value"] = json!([])));
    let result = teams.call("get_transcript", json!({"conversation": MEETING})).await.unwrap();
    assert_eq!(result["reason"], "transcript not ready");
    assert!(result["transcript"].is_string() && result["version"].is_null());
}

#[tokio::test]
async fn sharepoint_refusals_are_answers_not_a_lost_sign_in() {
    let teams = Teams::signed_in().await;
    for (status, why) in [(403, "refused"), (401, "refused"), (404, "not found")] {
        let ok = transcript_server(unchanged, same_list);
        teams.fake.on(move |s| if s.host == SHAREPOINT { Some(Reply::status(status)) } else { ok(s) });
        let error = module_error(teams.call("get_transcript", json!({"conversation": MEETING})).await);
        assert!(matches!(&error, ModuleError::Platform(text) if text.contains(why)), "{status}: {error:?}");
    }
    // The account is still signed in.
    teams.fake.on(transcript_server(unchanged, same_list));
    teams.call("get_transcript", json!({"conversation": MEETING})).await.unwrap();
}

// Live captions (spec 2026-10-29-live-captions, 4, and 2026-11-04-captions-archive, 4.2-4.3): the
// Teams side of a call against the fake server, a fake Trouter and a WebRTC peer on loopback
// standing in for Teams' media processor. Shapes from "Live results" 18-20, with invented ids.

const SERVICE: &str = "https://api.flightproxy.teams.microsoft.com";
const CONTROLLER: &str = "https://api.flightproxy.teams.microsoft.com/api/v2/ep/conv.synthetic/conv/c1";
const CONTROLLER_PATH: &str = "/api/v2/ep/conv.synthetic/conv/c1";
const CALL_STORE_PATH: &str = "/api/v2/ep/api.userstore.skype.com/api/v1/user/8:orgid:00000000-0000-0000-0000-000000000001/conversations";
const REGISTRAR_PATH: &str = "/registrar/prod/V2/registrations";
/// The call's id in the call store, a GUID as "Live results" 20 saw them.
const CALL: &str = "5a3e1b9c-0000-4000-8000-00000000000c";
/// The combined recording and transcription bot (docs 10.8).
const BOT: &str = "28:bdd75849-e0a6-4cce-8fc1-d7c0d4da43e5";
const COMMAND_PATH: &str = "/api/v2/ep/bot.synthetic/command";
const RECORDING: &str = "4580fd1d-e5a3-4f56-9ad1-aab0e3bf8f76";

impl Teams {
    /// Signed in, its WebSockets served by a fake Trouter.
    async fn live() -> (Teams, fake::Trouter) {
        let data = tempfile::tempdir().unwrap();
        let (fake, wam, trouter) = (Fake::start().await, Wam::new(), fake::Trouter::start().await);
        let (module, events) = load(&fake, &wam, data.path(), None, Some(trouter.addr)).await;
        module.sign_in().await.unwrap();
        (Teams { module, events, fake, wam, _data: data }, trouter)
    }
}

/// The user's official client in the call: audio, video and data ("Live results" 20).
fn official() -> Value {
    json!({"official-client": {"call": {"mediaStreams": [{"type": "audio", "direction": "sendrecv"}, {"type": "video", "direction": "recvonly"},
        {"type": "data", "direction": "sendrecv"}]}}})
}

/// The captions bot with captions `state`, and its command URL on the service's host.
fn bot(state: &str) -> Value {
    json!({"endpoints": {"bot-endpoint": {"call": {"mediaStreams": [{"type": "audio", "direction": "recvonly"}, {"type": "data", "direction": "sendrecv"}]},
        "endpointMetadata": {"commandUrl": format!("{SERVICE}{COMMAND_PATH}"), "spokenLanguage": "pl-pl",
            "processingModes": {"closedCaptions": {"state": state}, "recording": {"state": "Inactive"}}}}}})
}

/// The conversation of the call at `path` as the service answers the join and reads of it: its
/// links, the user's devices and the captions bot in `bot_state`, if it is in the call.
fn conversation_at(path: &str, user_endpoints: &Value, bot_state: Option<&str>) -> Value {
    let url = format!("{SERVICE}{path}");
    let mut participants = json!({ME: {"state": "active", "endpoints": user_endpoints}});
    if let Some(state) = bot_state {
        participants[BOT] = bot(state);
    }
    json!({"links": {"leave": format!("{url}/leave"), "addParticipant": format!("{url}/add"), "addParticipantAndModality": format!("{url}/addm")},
           "roster": {"participants": participants}})
}

fn live_conversation(user_endpoints: &Value) -> Value {
    conversation_at(CONTROLLER_PATH, user_endpoints, Some("Active"))
}

/// A call store item: a meeting or group call with its chat `thread`, or without one.
fn call_item(call: &str, path: &str, thread: Option<&str>) -> Value {
    let mut item =
        json!({"correlationId": call, "conversationController": format!("{SERVICE}{path}"), "userJoinTime": "2027-01-15T08:00:00.1234567Z"});
    if let Some(thread) = thread {
        item["groupContext"] = json!({"threadId": thread, "messageId": "0"});
        item["meetingInfo"] = json!({"organizerId": ME_OID, "tenantId": "00000000-0000-0000-0000-0000000000aa"});
    }
    item
}

/// A fake call: its call store item, its conversation's path and what the path answers.
struct FakeCall {
    item: Value,
    path: String,
    conversation: Arc<std::sync::Mutex<Value>>,
}

impl FakeCall {
    fn new(call: &str, path: &str, thread: Option<&str>, conversation: Value) -> FakeCall {
        FakeCall { item: call_item(call, path, thread), path: path.to_string(), conversation: Arc::new(std::sync::Mutex::new(conversation)) }
    }
}

type Calls = Arc<std::sync::Mutex<Vec<FakeCall>>>;

/// The call store with `calls`, their conversations, and the registrar, `leave`, the
/// acknowledgement, adding the bot and the bot's commands.
fn calls_server(calls: Calls) -> impl Fn(&fake::Seen) -> Option<Reply> {
    move |s| {
        let path = s.path.as_str();
        let calls = calls.lock().unwrap();
        match s.method.as_str() {
            "GET" if path == CALL_STORE_PATH => Some(Reply::json(Value::Array(calls.iter().map(|c| c.item.clone()).collect()))),
            "POST" if path == REGISTRAR_PATH => Some(Reply::status(202)),
            "DELETE" if path.starts_with(REGISTRAR_PATH) => Some(Reply::status(202)),
            "POST" if path == COMMAND_PATH => Some(Reply::status(202)),
            "GET" | "POST" => {
                let call = calls.iter().find(|c| path.starts_with(&c.path))?;
                match path.strip_prefix(&call.path).unwrap_or_default() {
                    "" => Some(Reply::json(call.conversation.lock().unwrap().clone())),
                    "/leave" => Some(Reply::status(204)),
                    "/ack" | "/add" | "/addm" => Some(Reply::status(202)),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// A meeting with captions on, the user in it on the devices `user_endpoints` (open_mess's own
/// is never listed, so any with audio is another device), and the call store's item for it.
fn meeting_server(user_endpoints: Value, in_call_store: bool) -> impl Fn(&fake::Seen) -> Option<Reply> {
    let call = FakeCall::new(CALL, CONTROLLER_PATH, Some(MEETING), live_conversation(&user_endpoints));
    calls_server(Arc::new(std::sync::Mutex::new(if in_call_store { vec![call] } else { Vec::new() })))
}

fn join_meeting() -> Value {
    json!({"conversation": MEETING, "call": CALL})
}

async fn wait_within<T>(seconds: u64, mut found: impl FnMut() -> Option<T>) -> T {
    for _ in 0..seconds * 10 {
        if let Some(found) = found() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("not within {seconds} s");
}

async fn wait_for<T>(found: impl FnMut() -> Option<T>) -> T {
    wait_within(10, found).await
}

fn requests(fake: &Fake, method: &str, path: &str) -> Vec<fake::Seen> {
    fake.seen().into_iter().filter(|s| s.method == method && s.path == path).collect()
}

/// The module's MS-SDP offer as plain SDP for the test's peer, and the peer's answer as Teams gives
/// it (docs "Live results" 16 and 18).
fn plain_offer(ms: &str) -> String {
    let lines = ms.lines().filter_map(|l| match l.strip_prefix("m=x-data ") {
        Some(rest) => Some(format!("m=application {} UDP/DTLS/SCTP webrtc-datachannel", rest.split(' ').next().unwrap())),
        None => (!["a=rtpmap", "a=fmtp", "a=x-", "a=label", "b="].iter().any(|p| l.starts_with(p))).then(|| l.to_string()),
    });
    lines.collect::<Vec<_>>().join("\r\n") + "\r\n"
}

fn ms_answer(plain: &str) -> String {
    let lines = plain.lines().filter_map(|l| match l.strip_prefix("m=application ") {
        Some(rest) => Some(format!("m=x-data {} RTP/SAVP 127 126\r\na=x-data-protocol:sctp\r\na=label:data", rest.split(' ').next().unwrap())),
        None => (!["a=extmap-allow-mixed", "a=msid-semantic", "a=ice-options"].iter().any(|p| l.starts_with(p))).then(|| l.to_string()),
    });
    lines.collect::<Vec<_>>().join("\r\n") + "\r\n"
}

struct MediaPeer {
    pc: Arc<dyn webrtc::peer_connection::PeerConnection>,
    channels: mpsc::Receiver<Arc<dyn webrtc::data_channel::DataChannel>>,
}

struct MediaEvents {
    channels: mpsc::Sender<Arc<dyn webrtc::data_channel::DataChannel>>,
    gathered: mpsc::Sender<()>,
}

#[async_trait::async_trait]
impl webrtc::peer_connection::PeerConnectionEventHandler for MediaEvents {
    async fn on_ice_gathering_state_change(&self, state: webrtc::peer_connection::RTCIceGatheringState) {
        if state == webrtc::peer_connection::RTCIceGatheringState::Complete {
            self.gathered.try_send(()).ok();
        }
    }

    async fn on_data_channel(&self, channel: Arc<dyn webrtc::data_channel::DataChannel>) {
        self.channels.try_send(channel).ok();
    }
}

/// A WebRTC peer on loopback that answers `offer`, as Teams' media processor would.
async fn media_peer(offer: &str) -> (MediaPeer, String) {
    use webrtc::peer_connection::{
        MediaEngine, PeerConnection, PeerConnectionBuilder, RTCConfigurationBuilder, RTCSessionDescription, SettingEngineBuilder,
    };
    let (channels_tx, channels) = mpsc::channel(4);
    let (gathered_tx, mut gathered) = mpsc::channel(1);
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(MediaEngine::default())
        .with_setting_engine(SettingEngineBuilder::new().with_include_loopback_candidate(true).build())
        .with_handler(Arc::new(MediaEvents { channels: channels_tx, gathered: gathered_tx }))
        .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
        .build()
        .await
        .unwrap();
    let pc: Arc<dyn PeerConnection> = Arc::new(pc);
    pc.set_remote_description(RTCSessionDescription::offer(offer.to_string()).unwrap()).await.unwrap();
    let answer = pc.create_answer(None).await.unwrap();
    pc.set_local_description(answer).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), gathered.recv()).await.ok();
    let sdp = pc.local_description().await.unwrap().sdp;
    (MediaPeer { pc, channels }, sdp)
}

/// Teams' media processor answers the join on Trouter, and the data channel opens.
async fn answer(trouter: &fake::Trouter, join: &Value) -> (MediaPeer, Arc<dyn webrtc::data_channel::DataChannel>) {
    let offer = join["callInvitation"]["mediaContent"]["blob"].as_str().unwrap();
    let acceptance = join["callInvitation"]["links"]["acceptance"].as_str().unwrap();
    let (mut peer, answer) = media_peer(&plain_offer(offer)).await;
    let accepted =
        json!({"callAcceptance": {"mediaContent": {"blob": ms_answer(&answer)}, "links": {"acknowledgement": format!("{CONTROLLER}/ack")}}});
    assert!(trouter.callback(1, acceptance, &accepted));
    let channel = tokio::time::timeout(Duration::from_secs(10), peer.channels.recv()).await.unwrap().unwrap();
    while !matches!(
        tokio::time::timeout(Duration::from_secs(10), channel.poll()).await.unwrap(),
        Some(webrtc::data_channel::DataChannelEvent::OnOpen)
    ) {}
    (peer, channel)
}

/// A caption message on the data channel (docs 10.7): the 11-byte header, data id 3, one final
/// result said by `by`.
fn caption_message(text: &str, by: &str) -> Vec<u8> {
    let name = if by == ME { ME_NAME } else { "Synthetic Person Two" };
    let result = json!({"isFinal": true, "text": text, "displayName": name, "spokenLanguage": "en-us", "userId": by});
    let mut message = vec![0x10, 11, 0x80 | 3, 0, 1, 0, 0, 0, 0, 7, 0];
    message.extend(json!({"recognitionResults": [result], "events": [], "version": 1}).to_string().as_bytes());
    message
}

#[tokio::test(flavor = "multi_thread")]
async fn live_captions_are_joined_read_and_left() {
    let (teams, trouter) = Teams::live().await;
    teams.fake.on(meeting_server(official(), true));
    assert_eq!(teams.call("join_captions", join_meeting()).await.unwrap(), json!({"state": "joining"}));
    // The roster read before joining, Trouter, the registration, then the join with the skype
    // token and no bearer.
    let join = wait_for(|| requests(&teams.fake, "POST", CONTROLLER_PATH).pop()).await;
    assert_eq!(requests(&teams.fake, "GET", CONTROLLER_PATH).len(), 1);
    assert!(trouter.seen().iter().any(|f| f.contains("\"user.authenticate\"")));
    assert_eq!(requests(&teams.fake, "POST", REGISTRAR_PATH).len(), 1);
    assert_eq!((join.header("x-skypetoken"), join.header("authorization")), (Some("synthetic-skype-token"), None));
    let body = join.json();
    assert_eq!((body["endpointState"]["state"]["isMuted"].as_bool(), &body["callInvitation"]["callModalities"]), (Some(true), &json!([])));
    assert_eq!(body["groupChat"]["threadId"], MEETING);
    let offer = body["callInvitation"]["mediaContent"]["blob"].as_str().unwrap();
    assert!(offer.contains("m=x-data ") && offer.matches("m=").count() == 1, "{offer}");
    let acceptance = body["callInvitation"]["links"]["acceptance"].as_str().unwrap();
    assert!(acceptance.starts_with(&format!("{}callAgent/", fake::SURL)) && acceptance.ends_with("/call/acceptance/"));
    // Teams' media processor answers on Trouter; the data channel opens and captions come, one of
    // them the user's own.
    let (peer, channel) = answer(&trouter, &body).await;
    channel.send(caption_message("Hello from the meeting.", OTHER).as_slice().into()).await.unwrap();
    channel.send(caption_message("And hello back.", ME).as_slice().into()).await.unwrap();
    let mut page = Value::Null;
    for _ in 0..100 {
        page = teams.call("read_captions", json!({"conversation": MEETING})).await.unwrap();
        if page["untrusted"]["cues"].as_array().is_some_and(|cues| cues.len() == 2) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!((page["state"].as_str(), page["captions_on"].as_bool(), page["next_cursor"].as_u64()), (Some("live"), Some(true), Some(2)), "{page}");
    assert_eq!((page["call"].as_str(), page["captions_seen"].as_bool(), page["skipped"].as_u64()), (Some(CALL), Some(true), Some(0)));
    let cues = &page["untrusted"]["cues"];
    assert_eq!((cues[0]["speaker"].as_str(), cues[0]["own"].as_bool()), (Some("Synthetic Person Two"), Some(false)));
    assert_eq!(cues[0]["text"], "Hello from the meeting.");
    assert_eq!((cues[1]["own"].as_bool(), cues[1]["text"].as_str()), (Some(true), Some("And hello back.")));
    // The time of arrival to the millisecond.
    let at = cues[0]["at"].as_str().unwrap();
    assert!(at.len() == 24 && at.as_bytes()[19] == b'.' && at.ends_with('Z'), "{at}");
    assert!(page.get("threads").is_none());
    assert_eq!(page["language"], "en-us");
    assert_eq!(requests(&teams.fake, "POST", &format!("{CONTROLLER_PATH}/ack")).len(), 1);
    assert!(trouter.seen().iter().any(|f| f.starts_with("3:::") && f.contains("\"status\":200")));
    // Another conversation's read shows nothing of this one.
    let other = teams.call("read_captions", json!({"conversation": GROUP})).await.unwrap();
    assert_eq!((other["state"].as_str(), &other["untrusted"]["cues"]), (Some("not_joined"), &json!([])));
    // Leaving: `leave` as open_mess's endpoint and the registration's removal.
    assert_eq!(teams.call("leave_captions", json!({"conversation": MEETING})).await.unwrap(), json!({"state": "ended"}));
    let leave = requests(&teams.fake, "POST", &format!("{CONTROLLER_PATH}/leave"));
    assert_eq!(leave.len(), 1);
    assert_eq!(leave[0].json()["participants"]["from"]["endpointId"], body["participants"]["from"]["endpointId"]);
    assert_eq!(teams.fake.seen().iter().filter(|s| s.method == "DELETE" && s.path.starts_with(REGISTRAR_PATH)).count(), 1);
    let session = page["session"].as_str().unwrap();
    let after = teams.call("read_captions", json!({"conversation": MEETING, "session": session, "cursor": 0})).await.unwrap();
    assert_eq!((after["state"].as_str(), after["reason"].as_str(), after["next_cursor"].as_u64()), (Some("ended"), Some("left"), Some(2)));
    let changed = teams.call("read_captions", json!({"conversation": MEETING, "session": "b3RoZXI", "cursor": 0})).await;
    assert!(matches!(module_error(changed), ModuleError::InvalidArgs(why) if why.contains("session changed")));
    // Captions were on all along: nothing was sent to turn them on.
    assert!(requests(&teams.fake, "POST", COMMAND_PATH).is_empty() && requests(&teams.fake, "POST", &format!("{CONTROLLER_PATH}/addm")).is_empty());
    peer.pc.close().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_user_must_be_in_the_call_before_and_after_the_join() {
    let (teams, trouter) = Teams::live().await;
    let data_only = json!({"call": {"mediaStreams": [{"type": "data", "direction": "sendrecv"}]}});
    // Before joining: only a data-only leg of the account, another open_mess's - not the user.
    teams.fake.on(meeting_server(json!({"another-open-mess": data_only}), true));
    let refused = module_error(teams.call("join_captions", join_meeting()).await);
    assert!(matches!(&refused, ModuleError::Platform(why) if why.starts_with("user_not_in_meeting:")), "{refused:?}");
    assert!(requests(&teams.fake, "POST", CONTROLLER_PATH).is_empty() && trouter.seen().is_empty());
    let page = teams.call("read_captions", json!({"conversation": MEETING})).await.unwrap();
    assert_eq!(page["state"], "not_joined");
    // A call whose policy turns captions off (`allowTranscription: false`): no join either.
    let mut no_captions = live_conversation(&official());
    no_captions["meetingDetails"] = json!({"meetingCapability": {"allowTranscription": false}});
    let call = FakeCall::new(CALL, CONTROLLER_PATH, Some(MEETING), no_captions);
    teams.fake.on(calls_server(Arc::new(std::sync::Mutex::new(vec![call]))));
    let refused = module_error(teams.call("join_captions", join_meeting()).await);
    assert!(matches!(&refused, ModuleError::Platform(why) if why.starts_with("policy:")), "{refused:?}");
    assert!(requests(&teams.fake, "POST", CONTROLLER_PATH).is_empty() && trouter.seen().is_empty());
    // In the call before, gone by the join's answer: open_mess leaves at once.
    let (before, after) = (meeting_server(official(), true), live_conversation(&json!({})));
    teams.fake.on(move |s| if s.method == "POST" && s.path == CONTROLLER_PATH { Some(Reply::json(after.clone())) } else { before(s) });
    teams.call("join_captions", join_meeting()).await.unwrap();
    let leave = wait_for(|| requests(&teams.fake, "POST", &format!("{CONTROLLER_PATH}/leave")).pop()).await;
    assert_eq!(leave.json()["callTransactionEnd"]["phrase"], "LOCAL_USER_INITIATED");
    let page = teams.call("read_captions", json!({"conversation": MEETING})).await.unwrap();
    assert_eq!((page["state"].as_str(), page["reason"].as_str()), (Some("failed"), Some("user_not_in_meeting")));
    // Another session of the account can start again.
    teams.fake.on(meeting_server(official(), true));
    teams.call("join_captions", join_meeting()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_end_of_the_leg_is_a_refusal_and_the_end_of_the_conversation_the_meeting_end() {
    let (teams, trouter) = Teams::live().await;
    teams.fake.on(meeting_server(official(), true));
    // The session's state and reason once it is no longer joining.
    async fn settled(teams: &Teams) -> (String, String) {
        for _ in 0..100 {
            let page = teams.call("read_captions", json!({"conversation": MEETING})).await.unwrap();
            if page["state"] != "joining" {
                return (page["state"].as_str().unwrap_or("").into(), page["reason"].as_str().unwrap_or("").into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("not within 10 s");
    }
    let joins = |teams: &Teams| requests(&teams.fake, "POST", CONTROLLER_PATH);
    let leaves = |teams: &Teams| requests(&teams.fake, "POST", &format!("{CONTROLLER_PATH}/leave")).len();
    // `/call/end/` ends open_mess's leg (430/10067 came live when Teams refused it, "Live results" 19).
    teams.call("join_captions", join_meeting()).await.unwrap();
    let join = wait_for(|| joins(&teams).pop()).await.json();
    let end = join["callInvitation"]["links"]["end"].as_str().unwrap().to_string();
    assert!(trouter.callback(1, &end, &json!({"callEnd": {"code": 430, "subCode": 10067}})));
    assert_eq!(settled(&teams).await, ("failed".into(), "refused".into()));
    // `/conversation/conversationEnd/` is the meeting's end.
    teams.call("join_captions", join_meeting()).await.unwrap();
    let join = wait_for(|| joins(&teams).get(1).map(|join| join.json())).await;
    let end = join["conversationRequest"]["links"]["conversationEnd"].as_str().unwrap().to_string();
    assert!(trouter.callback(2, &end, &json!({"conversationEnd": {"code": 0}})));
    assert_eq!(settled(&teams).await, ("ended".into(), "meeting_ended".into()));
    // The codes of an end-to-end encrypted meeting (docs 10.8).
    teams.call("join_captions", join_meeting()).await.unwrap();
    let join = wait_for(|| joins(&teams).get(2).map(|join| join.json())).await;
    let end = join["callInvitation"]["links"]["end"].as_str().unwrap().to_string();
    assert!(trouter.callback(3, &end, &json!({"callEnd": {"code": 403, "subCode": 4158}})));
    assert_eq!(settled(&teams).await, ("failed".into(), "e2ee".into()));
    // The lobby: the acceptance names it, and open_mess leaves.
    let before = leaves(&teams);
    teams.call("join_captions", join_meeting()).await.unwrap();
    let join = wait_for(|| joins(&teams).get(3).map(|join| join.json())).await;
    let acceptance = join["callInvitation"]["links"]["acceptance"].as_str().unwrap().to_string();
    assert!(trouter.callback(4, &acceptance, &json!({"callAcceptance": {"controllerName": "lobby"}})));
    assert_eq!(settled(&teams).await, ("failed".into(), "lobby".into()));
    assert_eq!(leaves(&teams), before + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn joining_needs_its_call_in_the_call_store() {
    let (teams, trouter) = Teams::live().await;
    teams.fake.on(meeting_server(official(), false));
    let gone = module_error(teams.call("join_captions", join_meeting()).await);
    assert!(matches!(&gone, ModuleError::Platform(why) if why == "that call is no longer running"), "{gone:?}");
    // One read of the call store, and nothing reached the call or Trouter.
    assert_eq!(requests(&teams.fake, "GET", CALL_STORE_PATH).len(), 1);
    assert!(requests(&teams.fake, "GET", CONTROLLER_PATH).is_empty() && trouter.seen().is_empty());
    // The call is there, but not in this conversation, or another call is: the same.
    teams.fake.on(meeting_server(official(), true));
    for args in [json!({"conversation": GROUP, "call": CALL}), json!({"conversation": MEETING, "call": "0ther-ca11"})] {
        let gone = module_error(teams.call("join_captions", args).await);
        assert!(matches!(&gone, ModuleError::Platform(why) if why == "that call is no longer running"), "{gone:?}");
    }
    // `call` is required, and checked.
    for args in
        [json!({"conversation": MEETING}), json!({"conversation": MEETING, "call": "a b"}), json!({"conversation": "8:orgid:x", "call": CALL})]
    {
        assert!(matches!(module_error(teams.call("join_captions", args).await), ModuleError::InvalidArgs(_)));
    }
    let bad = json!({"conversation": MEETING, "call": CALL, "language": "polish!"});
    assert!(matches!(module_error(teams.call("join_captions", bad).await), ModuleError::InvalidArgs(_)));
    assert!(requests(&teams.fake, "GET", CONTROLLER_PATH).is_empty() && trouter.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_has_a_session_a_call_three_at_most() {
    let (teams, _trouter) = Teams::live().await;
    let call = |id: &str, path: &str, thread: Option<&str>| FakeCall::new(id, path, thread, conversation_at(path, &official(), Some("Active")));
    let calls: Calls = Arc::new(std::sync::Mutex::new(vec![
        call(CALL, CONTROLLER_PATH, Some(MEETING)),
        call("c2", "/api/v2/ep/conv.synthetic/conv/c2", Some(GROUP)),
        call("c3", "/api/v2/ep/conv.synthetic/conv/c3", None),
        call("c4", "/api/v2/ep/conv.synthetic/conv/c4", Some(CHANNEL)),
    ]));
    teams.fake.on(calls_server(calls.clone()));
    let joins = |path: &str| requests(&teams.fake, "POST", path).len();
    for args in [join_meeting(), json!({"conversation": GROUP, "call": "c2"}), json!({"conversation": "call:c3", "call": "c3"})] {
        assert_eq!(teams.call("join_captions", args).await.unwrap(), json!({"state": "joining"}));
    }
    wait_for(|| {
        ([CONTROLLER_PATH, "/api/v2/ep/conv.synthetic/conv/c2", "/api/v2/ep/conv.synthetic/conv/c3"].iter().all(|p| joins(p) == 1)).then_some(())
    })
    .await;
    // Each session is a device of its own: three endpoints.
    let endpoints: std::collections::HashSet<String> = teams
        .fake
        .seen()
        .iter()
        .filter(|s| s.method == "POST" && s.path.starts_with("/api/v2/ep/conv.synthetic/conv/c") && !s.path.contains("/leave"))
        .map(|s| s.json()["participants"]["from"]["endpointId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(endpoints.len(), 3);
    // The same call again, even under another name: the session goes on, no second join.
    let again = teams.call("join_captions", json!({"conversation": "call:".to_string() + CALL, "call": CALL})).await.unwrap();
    assert!(matches!(again["state"].as_str(), Some("joining" | "live")), "{again}");
    // Under a name that is not the call's, it is refused, and the session never takes that name.
    let foreign = module_error(teams.call("join_captions", json!({"conversation": CHANNEL, "call": CALL})).await);
    assert!(matches!(&foreign, ModuleError::Platform(why) if why == "that call is no longer running"), "{foreign:?}");
    assert_eq!(teams.call("read_captions", json!({"conversation": CHANNEL})).await.unwrap()["state"], "not_joined");
    // A fourth call of the account: refused before anything reaches it.
    let limit = module_error(teams.call("join_captions", json!({"conversation": CHANNEL, "call": "c4"})).await);
    assert!(matches!(&limit, ModuleError::Platform(why) if why.starts_with("connection_limit:")), "{limit:?}");
    assert!(teams.fake.seen().iter().all(|s| !s.path.starts_with("/api/v2/ep/conv.synthetic/conv/c4")));
    let first = teams.call("read_captions", json!({"conversation": MEETING})).await.unwrap();
    assert_eq!(first["call"], CALL);
    // The meeting's call ended and a new one started in its chat: the old session ends first,
    // then the new call is joined; the old cues stay readable with the old session.
    {
        let mut calls = calls.lock().unwrap();
        calls.retain(|c| c.path != CONTROLLER_PATH);
        calls.push(call("c5", "/api/v2/ep/conv.synthetic/conv/c5", Some(MEETING)));
    }
    teams.call("join_captions", json!({"conversation": MEETING, "call": "c5"})).await.unwrap();
    assert_eq!(requests(&teams.fake, "POST", &format!("{CONTROLLER_PATH}/leave")).len(), 1);
    wait_for(|| (joins("/api/v2/ep/conv.synthetic/conv/c5") == 1).then_some(())).await;
    assert_eq!(joins(CONTROLLER_PATH), 1);
    let new = teams.call("read_captions", json!({"conversation": MEETING})).await.unwrap();
    assert_eq!(new["call"], "c5");
    let old = teams.call("read_captions", json!({"conversation": MEETING, "session": first["session"], "cursor": 0})).await.unwrap();
    assert_eq!((old["call"].as_str(), old["state"].as_str(), old["reason"].as_str()), (Some(CALL), Some("ended"), Some("meeting_ended")));
    // Leaving one conversation leaves only its session.
    teams.call("leave_captions", json!({"conversation": GROUP})).await.unwrap();
    assert_eq!(requests(&teams.fake, "POST", "/api/v2/ep/conv.synthetic/conv/c2/leave").len(), 1);
    assert!(requests(&teams.fake, "POST", "/api/v2/ep/conv.synthetic/conv/c5/leave").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_are_listed_by_their_chat_or_id() {
    let teams = Teams::signed_in().await;
    let other_oid = OTHER.trim_start_matches("8:orgid:");
    let theirs_first = format!("19:{other_oid}_{ME_OID}@unq.gbl.spaces");
    let third = "8:orgid:00000000-0000-0000-0000-000000000003";
    let direct = |call: &str, people: [&str; 2]| {
        let mut item = call_item(call, "/api/v2/ep/conv.synthetic/conv/d", None);
        item["participants"] = json!(people);
        item["additionalData"] = json!({"participantList": [{"id": people[0], "endpointType": null}, {"id": people[1], "endpointType": null}]});
        item
    };
    let mut escalated = direct("c4", [ME, OTHER]);
    escalated["groupContext"] = json!({"threadId": GROUP, "messageId": "1"});
    escalated["userJoinTime"] = Value::Null;
    let mut items = vec![
        call_item(CALL, CONTROLLER_PATH, Some(MEETING)),
        direct("c2", [OTHER, ME]),
        direct("c3", [ME, third]),
        escalated,
        call_item("not a call id", CONTROLLER_PATH, Some(MEETING)),
        json!({"correlationId": "c6", "conversationController": "https://evil.example/conv"}),
    ];
    items.extend((0..30).map(|n| call_item(&format!("x{n}"), CONTROLLER_PATH, Some(&format!("19:x{n}@thread.v2")))));
    let existing = theirs_first.clone();
    teams.fake.on(move |s| {
        if s.path == CALL_STORE_PATH {
            Some(Reply::json(Value::Array(items.clone())))
        } else if s.path == format!("{CHAT_PATH}/{existing}") {
            Some(Reply::json(fake::conversation(&existing, None, None)))
        } else {
            None
        }
    });
    let listed = teams.call("list_calls", json!({})).await.unwrap();
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 20);
    assert_eq!(items[0], json!({"conversation": MEETING, "call": CALL, "joined_at": null}));
    // A 1:1 call by its chat, whichever order its ids have; a chat that does not exist leaves the id.
    assert_eq!((items[1]["conversation"].as_str(), items[1]["call"].as_str()), (Some(theirs_first.as_str()), Some("c2")));
    assert_eq!(items[2]["conversation"], "call:c3");
    // Someone added to a 1:1 call: the same call, now in its group chat.
    assert_eq!((items[3]["conversation"].as_str(), items[3]["call"].as_str()), (Some(GROUP), Some("c4")));
    assert_eq!(items[4]["call"], "x0");
    assert!(listed.get("untrusted").is_none());
    // Reads only.
    assert!(writes(&teams.fake).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_without_a_chat_is_described_for_its_approval() {
    let teams = Teams::signed_in().await;
    let mut conversation = conversation_at(CONTROLLER_PATH, &official(), Some("Active"));
    conversation["subject"] = json!("");
    conversation["conversationStartTime"] = json!("2027-01-15T08:00:00.1234567Z");
    conversation["roster"]["participants"][ME]["displayName"] = json!(ME_NAME);
    let call = FakeCall::new(CALL, CONTROLLER_PATH, None, conversation);
    let calls: Calls = Arc::new(std::sync::Mutex::new(vec![call]));
    teams.fake.on(calls_server(calls.clone()));
    let target = teams.module.describe_target(ME_OID, &format!("call:{CALL}"), None).await.unwrap();
    assert_eq!(target.conversation_title, "Teams call without a chat, started 2027-01-15T08:00:00Z");
    let mut people: Vec<(String, String)> = target.participants.into_iter().map(|p| (p.id, p.display_name)).collect();
    people.sort();
    assert_eq!(people, [(BOT.to_string(), BOT.to_string()), (ME.to_string(), ME_NAME.to_string())]);
    // Reads only, and no chat is looked for.
    assert!(writes(&teams.fake).is_empty() && teams.fake.seen().iter().all(|s| !s.path.starts_with(CHAT_PATH)));
    calls.lock().unwrap().clear();
    let gone = module_error(teams.module.describe_target(ME_OID, &format!("call:{CALL}"), None).await);
    assert!(matches!(&gone, ModuleError::Platform(why) if why == "that call is no longer running"), "{gone:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn captions_are_turned_on_once_and_not_after_someone_had_them_on() {
    let (teams, trouter) = Teams::live().await;
    // A 1:1 call without captions: no chat thread, no bot.
    let path = "/api/v2/ep/conv.synthetic/conv/d1";
    let one_to_one = FakeCall::new(CALL, path, None, conversation_at(path, &official(), None));
    let conversation = one_to_one.conversation.clone();
    teams.fake.on(calls_server(Arc::new(std::sync::Mutex::new(vec![one_to_one]))));
    let args = json!({"conversation": format!("call:{CALL}"), "call": CALL});
    teams.call("join_captions", args.clone()).await.unwrap();
    let join = wait_for(|| requests(&teams.fake, "POST", path).pop()).await.json();
    assert_eq!(join["groupChat"], Value::Null);
    let (peer, _channel) = answer(&trouter, &join).await;
    // 10 s after the data channel opened, no one has captions on: the bot is added the client's
    // way for a call without a chat, with the language and the user's token for the recording
    // resource.
    let added = wait_within(20, || requests(&teams.fake, "POST", &format!("{path}/add")).pop()).await;
    assert!(requests(&teams.fake, "POST", &format!("{path}/addm")).is_empty());
    assert_eq!(added.header("x-skypetoken"), Some("synthetic-skype-token"));
    let body = added.json();
    assert_eq!(body["participants"]["to"][0]["id"], BOT);
    let bot_data = &body["participantInvitationData"]["botData"];
    assert_eq!((bot_data["spokenLanguage"].as_str(), bot_data["callId"].as_str()), (Some("pl-pl"), Some(CALL)));
    assert_eq!(fake::claims(bot_data["initiatorUserToken"].as_str().unwrap())["aud"], RECORDING);
    // The bot comes in without captions: it gets `start`, with both tokens.
    *conversation.lock().unwrap() = conversation_at(path, &official(), Some("Inactive"));
    let roster_update = join["conversationRequest"]["roster"]["rosterUpdate"].as_str().unwrap().to_string();
    assert!(trouter.callback(5, &roster_update, &json!({})));
    let start = wait_for(|| requests(&teams.fake, "POST", COMMAND_PATH).pop()).await;
    assert_eq!((start.audience().as_deref(), start.header("x-skypetoken")), (Some(RECORDING), Some("synthetic-skype-token")));
    assert_eq!(start.header("x-microsoft-skype-chain-id"), Some(CALL));
    let command = start.json();
    assert_eq!((command["action"].as_str(), &command["processingModes"]), (Some("start"), &json!(["closedCaptions"])));
    assert!(command.get("language").is_none());
    // Captions on: the session saw them.
    *conversation.lock().unwrap() = conversation_at(path, &official(), Some("Active"));
    assert!(trouter.callback(6, &roster_update, &json!({})));
    let mut seen = Value::Null;
    for _ in 0..50 {
        seen = teams.call("read_captions", json!({"conversation": format!("call:{CALL}")})).await.unwrap();
        if seen["captions_on"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!((seen["captions_on"].as_bool(), seen["captions_seen"].as_bool()), (Some(true), Some(true)), "{seen}");
    teams.call("leave_captions", json!({"conversation": format!("call:{CALL}")})).await.unwrap();
    peer.pc.close().await.ok();
    // Someone turned them off; a new session of the same call does not turn them on, even asked to.
    *conversation.lock().unwrap() = conversation_at(path, &official(), Some("Inactive"));
    teams.call("join_captions", json!({"conversation": format!("call:{CALL}"), "call": CALL, "start_captions": true})).await.unwrap();
    let join = wait_for(|| requests(&teams.fake, "POST", path).get(1).map(fake::Seen::json)).await;
    let (peer, _channel) = answer(&trouter, &join).await;
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(requests(&teams.fake, "POST", &format!("{path}/add")).len(), 1);
    assert_eq!(requests(&teams.fake, "POST", COMMAND_PATH).len(), 1);
    // Nothing but `start` ever went to the bot: no language change, no stop.
    assert!(
        teams
            .fake
            .seen()
            .iter()
            .all(|s| !String::from_utf8_lossy(&s.body).contains("setLanguage") && !String::from_utf8_lossy(&s.body).contains("\"stop\""))
    );
    peer.pc.close().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn end_sessions_leaves_the_meeting() {
    let (teams, _trouter) = Teams::live().await;
    teams.fake.on(meeting_server(official(), true));
    teams.call("join_captions", join_meeting()).await.unwrap();
    wait_for(|| requests(&teams.fake, "POST", CONTROLLER_PATH).pop()).await;
    teams.module.end_sessions(ME_OID).await.unwrap();
    assert_eq!(requests(&teams.fake, "POST", &format!("{CONTROLLER_PATH}/leave")).len(), 1);
    let page = teams.call("read_captions", json!({"conversation": MEETING})).await.unwrap();
    assert_eq!((page["state"].as_str(), page["reason"].as_str()), (Some("ended"), Some("update")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_leave_is_sent_again_from_the_session_record() {
    let (teams, _trouter) = Teams::live().await;
    // `leave` fails until the test lets it through; the rest is the usual meeting.
    let status = Arc::new(AtomicUsize::new(503));
    let (meeting, answer) = (meeting_server(official(), true), status.clone());
    let leave_path = format!("{CONTROLLER_PATH}/leave");
    teams.fake.on(move |s| if s.path == leave_path { Some(Reply::status(answer.load(Ordering::SeqCst) as u16)) } else { meeting(s) });
    let leaves = |teams: &Teams| requests(&teams.fake, "POST", &format!("{CONTROLLER_PATH}/leave")).len();
    teams.call("join_captions", join_meeting()).await.unwrap();
    wait_for(|| requests(&teams.fake, "POST", CONTROLLER_PATH).pop()).await;
    assert_eq!(teams.call("leave_captions", json!({"conversation": MEETING})).await.unwrap(), json!({"state": "ended"}));
    assert_eq!(leaves(&teams), 1);
    // A new instance (after a crash, or the daemon's start) leaves from the record: the
    // conversation is read for its `leave` link, and the record stays while it fails.
    let reads = requests(&teams.fake, "GET", CONTROLLER_PATH).len();
    teams.module.restore(ME_OID).await.unwrap();
    assert_eq!((leaves(&teams), requests(&teams.fake, "GET", CONTROLLER_PATH).len()), (2, reads + 1));
    // Signing out tries once more, with no session holding the leg; then the record is gone.
    status.store(204, Ordering::SeqCst);
    teams.module.sign_out(ME_OID).await.unwrap();
    assert_eq!(leaves(&teams), 3);
    teams.module.sign_in().await.unwrap();
    teams.module.restore(ME_OID).await.unwrap();
    assert_eq!(leaves(&teams), 3);
}
