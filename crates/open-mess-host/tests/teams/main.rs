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

use fake::{AUTHZ_PATH, CHAT, CHAT_PATH, Fake, ME, ME_NAME, ME_OID, OTHER, Reply, Wam, WamCall, conversation, message, page};
use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{AccountInfo, Engine, HostError, HostEvent, ModuleContext, ModuleError, ModuleHandle, Net, NewMessage, SessionState, limits};
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
        let (module, events) = load(&fake, &wam, data.path(), None).await;
        Teams { module, events, fake, wam, _data: data }
    }

    /// Signed in, with the module's timers (its 60 s poll) firing within `POLL` instead.
    async fn polling() -> Teams {
        let data = tempfile::tempdir().unwrap();
        let (fake, wam) = (Fake::start().await, Wam::new());
        let (module, events) = load(&fake, &wam, data.path(), Some(POLL)).await;
        module.sign_in().await.unwrap();
        Teams { module, events, fake, wam, _data: data }
    }

    async fn next_message(&mut self) -> Option<NewMessage> {
        next_message(&mut self.events).await
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

async fn load(fake: &Fake, wam: &Wam, data_dir: &Path, timer_cap: Option<u32>) -> (ModuleHandle, mpsc::Receiver<HostEvent>) {
    let mut engine = Engine::with_limits(limits::for_tests()).unwrap().with_fake_wam(wam.hook());
    if let Some(cap) = timer_cap {
        engine = engine.with_timer_cap(cap);
    }
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
    // The first page failing is still an error.
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
    let (group, channel) = (messages_path(GROUP), messages_path(CHANNEL));
    let conversations = vec![
        conversation(GROUP, None, Some(numbered(103, GROUP, OTHER, "x"))),
        conversation(CHANNEL, None, Some(numbered(102, CHANNEL, OTHER, "x"))),
    ];
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
        } else {
            return None;
        };
        Some(Reply::json(page("messages", messages, None)))
    });
    let result = teams.call("mentions", json!({"since": "2027-01-15T08:00:00Z"})).await.unwrap();
    assert_eq!(ids(&result, "id"), ["103"]);
    assert_eq!(result["items"][0]["untrusted"]["text"], "hey @One");
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
    teams.fake.on(|s| (s.method == "POST").then(|| Reply::status(201)));
    teams.call("send", json!({"conversation": CHANNEL, "text": "ok", "reply_to": "1800000000100"})).await.unwrap();
    assert_eq!(writes(&teams.fake)[0].path, format!("{CHAT_PATH}/{CHANNEL};messageid=1800000000100/messages"));
    // A quote reply in a chat has no confirmed format yet: refused, nothing sent.
    let quote = teams.call("send", json!({"conversation": GROUP, "text": "ok", "reply_to": "1800000000100"})).await;
    assert!(matches!(module_error(quote), ModuleError::NotSupported(_)));
    assert_eq!(writes(&teams.fake).len(), 1);
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
        } else {
            None
        }
    }
}

#[tokio::test]
async fn describe_target_lists_members() {
    let teams = Teams::signed_in().await;
    let members = json!({"members": [
        {"id": ME, "role": "Admin", "friendlyName": ME_NAME},
        {"id": OTHER, "role": "User", "friendlyName": "Synthetic Person Two"},
        {"id": "8:orgid:00000000-0000-0000-0000-000000000003", "role": "User"},
    ]});
    teams.fake.on(target_server(GROUP, Some("Synthetic group"), members));
    let target = teams.module.describe_target(ME_OID, GROUP, None).await.unwrap();
    assert_eq!(target.conversation_title, "Synthetic group");
    let participants: Vec<(&str, &str)> = target.participants.iter().map(|p| (p.id.as_str(), p.display_name.as_str())).collect();
    let third = "8:orgid:00000000-0000-0000-0000-000000000003";
    assert_eq!(participants, [(ME, ME_NAME), (OTHER, "Synthetic Person Two"), (third, third)]);
    assert_eq!(target.target_text, None);
    // Reads only.
    assert!(writes(&teams.fake).is_empty());

    // A channel is the target itself: no roster.
    teams.fake.on(target_server(CHANNEL, Some("Synthetic channel"), json!({"members": [{"id": ME}]})));
    let channel = teams.module.describe_target(ME_OID, CHANNEL, None).await.unwrap();
    assert_eq!((channel.conversation_title.as_str(), channel.participants.len()), ("Synthetic channel", 0));
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
    for reply in [Reply::status(201).delayed(Duration::from_millis(1500)), Reply::status(503)] {
        let teams = Teams::signed_in().await;
        let reply = std::sync::Mutex::new(Some(reply));
        teams.fake.on(move |s| (s.method == "POST").then(|| reply.lock().unwrap().take().unwrap_or_else(|| Reply::status(201))));
        let result = teams.call("send", json!({"conversation": GROUP, "text": "once"})).await;
        assert!(matches!(module_error(result), ModuleError::Uncertain(_)));
        assert_eq!(writes(&teams.fake).len(), 1);
    }
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
    teams.call("send", json!({"conversation": OTHER, "text": "hi"})).await.unwrap();
    let probes: Vec<String> = teams.fake.seen().into_iter().filter(|s| s.method == "GET").map(|s| s.path).collect();
    assert_eq!(probes, [format!("{CHAT_PATH}/{mine_first}"), format!("{CHAT_PATH}/{theirs_first}")]);
    assert_eq!(writes(&teams.fake)[0].path, messages_path(&theirs_first));
    // The approval shows the same chat.
    let target = teams.module.describe_target(ME_OID, OTHER, None).await.unwrap();
    assert_eq!(target.conversation_title, "Synthetic Person Two");

    // No chat with this person yet: refused, nothing sent.
    teams.fake.on(|s| (s.method == "POST").then(|| Reply::status(201)));
    let none = teams.call("send", json!({"conversation": "8:orgid:00000000-0000-0000-0000-000000000004", "text": "hi"})).await;
    assert!(matches!(module_error(none), ModuleError::Platform(_)));
    assert_eq!(writes(&teams.fake).len(), 1);
}

// Task 5: polling events.

fn now_ms() -> i64 {
    i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

/// The next message event, if one comes within a few polls.
async fn next_message(events: &mut mpsc::Receiver<HostEvent>) -> Option<NewMessage> {
    let deadline = Instant::now() + Duration::from_millis(u64::from(POLL) * 6);
    loop {
        match tokio::time::timeout_at(deadline.into(), events.recv()).await {
            Ok(Some(HostEvent::MessageArrived(message))) => return Some(message),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return None,
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
    let polls = teams.fake.requests(CHAT_PATH);
    for poll in &polls {
        let start: i64 = poll.query.split("startTime=").nth(1).unwrap().split('&').next().unwrap().parse().unwrap();
        assert!(start > started - 10 * 60_000 && start < started + 60_000, "{}", poll.query);
    }
    assert!(teams.fake.seen().iter().all(|s| s.method == "GET" || s.path == AUTHZ_PATH));
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
    let old = conversation(GROUP, None, Some(message("1900000000005", GROUP, OTHER, "<p>old</p>", now_ms() + 1000)));
    let listed = Arc::new(std::sync::Mutex::new(vec![old]));
    let list = listed.clone();
    fake.on(move |s| (s.path == CHAT_PATH).then(|| Reply::json(page("conversations", list.lock().unwrap().clone(), None))));
    {
        let (module, mut events) = load(&fake, &wam, data.path(), Some(POLL)).await;
        module.sign_in().await.unwrap();
        assert_eq!(next_message(&mut events).await.map(|m| m.message).as_deref(), Some("1900000000005"));
    }
    // A new instance, as after a crash or a daemon restart: the same storage, nothing in memory.
    let (module, mut events) = load(&fake, &wam, data.path(), Some(POLL)).await;
    module.restore(ME_OID).await.unwrap();
    assert_eq!(next_message(&mut events).await, None);
    let new = conversation(GROUP, None, Some(message("1900000000006", GROUP, OTHER, "<p>new</p>", now_ms() + 1000)));
    *listed.lock().unwrap() = vec![new];
    assert_eq!(next_message(&mut events).await.map(|m| m.message).as_deref(), Some("1900000000006"));
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
