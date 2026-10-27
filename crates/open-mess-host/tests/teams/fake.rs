//! A fake Teams for the module's tests: one loopback server stands in for every host of the Teams
//! manifest (the host's test override sends each request there, its own host in `Host`), and a
//! fake WAM hands out synthetic tokens with invented ids. Nothing here reaches Microsoft.

// Test code: unwrap and indexing are fine here (clippy.toml allows them in tests only), and each
// test uses only some of the helpers.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::response::Response;
use open_mess_host::FakeWam;
use serde_json::{Value, json};

/// The signed-in user of the tests: an invented object id and name.
pub const ME_OID: &str = "00000000-0000-0000-0000-000000000001";
pub const ME: &str = "8:orgid:00000000-0000-0000-0000-000000000001";
pub const ME_NAME: &str = "Synthetic Person One";
pub const OTHER: &str = "8:orgid:00000000-0000-0000-0000-000000000002";
pub const CHAT: &str = "https://teams.microsoft.com/api/chatsvc/emea";
pub const CHAT_PATH: &str = "/api/chatsvc/emea/v1/users/ME/conversations";
pub const AUTHZ_PATH: &str = "/api/authsvc/v1.0/authz";

/// One request the fake server received.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub host: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    /// The audience of the bearer token the request carried.
    pub fn audience(&self) -> Option<String> {
        let token = self.header("authorization")?.strip_prefix("Bearer ")?;
        Some(claims(token)["aud"].as_str()?.to_string())
    }
}

/// What the fake server answers.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub delay: Duration,
}

impl Reply {
    pub fn status(status: u16) -> Reply {
        Reply { status, headers: Vec::new(), body: Vec::new(), delay: Duration::ZERO }
    }

    pub fn json(body: Value) -> Reply {
        Reply { body: body.to_string().into_bytes(), ..Reply::status(200) }.header("content-type", "application/json")
    }

    pub fn header(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn with_status(mut self, status: u16) -> Reply {
        self.status = status;
        self
    }

    pub fn delayed(mut self, delay: Duration) -> Reply {
        self.delay = delay;
        self
    }
}

type Handler = Arc<dyn Fn(&Seen) -> Option<Reply> + Send + Sync>;

/// The fake server. Requests the test's handler leaves alone (`None`) get the defaults: authz
/// answers with the service directory, everything else is 404.
#[derive(Clone)]
pub struct Fake {
    pub addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    handler: Arc<Mutex<Handler>>,
}

impl Fake {
    pub async fn start() -> Fake {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler: Handler = Arc::new(|_| None);
        let fake = Fake { addr, seen: Arc::default(), handler: Arc::new(Mutex::new(handler)) };
        let server = fake.clone();
        let app = axum::Router::new().fallback(move |request: Request| {
            let server = server.clone();
            async move { server.answer(request).await }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        fake
    }

    /// Replaces the test's handler.
    pub fn on(&self, handler: impl Fn(&Seen) -> Option<Reply> + Send + Sync + 'static) {
        *self.handler.lock().unwrap() = Arc::new(handler);
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// Requests whose path starts with `prefix`.
    pub fn requests(&self, prefix: &str) -> Vec<Seen> {
        self.seen().into_iter().filter(|s| s.path.starts_with(prefix)).collect()
    }

    async fn answer(&self, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let seen = Seen {
            method: parts.method.to_string(),
            host: parts.headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or_default().to_string(),
            path: parts.uri.path().to_string(),
            query: parts.uri.query().unwrap_or_default().to_string(),
            headers: parts.headers.iter().map(|(n, v)| (n.to_string(), v.to_str().unwrap_or_default().to_string())).collect(),
            body: to_bytes(body, usize::MAX).await.unwrap().to_vec(),
        };
        self.seen.lock().unwrap().push(seen.clone());
        let handler = self.handler.lock().unwrap().clone();
        let reply = handler(&seen).unwrap_or_else(|| default_reply(&seen));
        tokio::time::sleep(reply.delay).await;
        let mut response = Response::builder().status(reply.status);
        for (name, value) in &reply.headers {
            response = response.header(name, value);
        }
        response.body(Body::from(reply.body)).unwrap()
    }
}

/// The authz answer of "Live results" 2, with synthetic values.
pub fn authz() -> Value {
    json!({
        "region": "emea",
        "partition": "emea01",
        "regionGtms": {
            "chatServiceAfd": CHAT,
            "chatService": "https://emea.ng.msg.teams.microsoft.com",
            "middleTier": "https://teams.microsoft.com/api/mt/emea",
            "amsV2": "https://eu-prod.asyncgw.teams.microsoft.com",
            "search": "https://teams.microsoft.com/api/search/emea",
        },
        "tokens": {"skypeToken": "synthetic-skype-token", "expiresIn": 4500, "tokenType": "SkypeToken"},
    })
}

/// A page of the chat service: `key` (`conversations`, `messages`) and the paging links.
pub fn page(key: &str, items: Vec<Value>, backward: Option<&str>) -> Value {
    json!({ key: items, "_metadata": {"backwardLink": backward, "syncState": "c3luYw"} })
}

/// A conversation whose last message is `last`, as the list returns it.
pub fn conversation(id: &str, topic: Option<&str>, last: Option<Value>) -> Value {
    json!({
        "id": id,
        "threadProperties": {"topic": topic},
        "properties": {"consumptionhorizon": "1800000000000;1800000000000;1"},
        "lastMessage": last.unwrap_or_else(|| json!({})),
    })
}

/// A message as the chat service sends it; `from` is an MRI, `ms` the arrival time.
pub fn message(id: &str, conversation: &str, from: &str, html: &str, ms: i64) -> Value {
    json!({
        "id": id,
        "conversationid": conversation,
        "from": format!("{CHAT}/v1/users/ME/contacts/{from}"),
        "imdisplayname": if from == ME { ME_NAME } else { "Synthetic Person Two" },
        "messagetype": "RichText/Html",
        "contenttype": "text",
        "content": html,
        "originalarrivaltime": ms,
        "properties": {},
    })
}

fn default_reply(seen: &Seen) -> Reply {
    if seen.method == "POST" && seen.path == AUTHZ_PATH { Reply::json(authz()) } else { Reply::status(404) }
}

/// What the fake WAM was asked.
#[derive(Clone, Debug, PartialEq)]
pub struct WamCall {
    pub scopes: Vec<String>,
    pub interactive: bool,
    pub claims: Option<String>,
}

impl WamCall {
    pub fn silent(scope: &str) -> WamCall {
        WamCall { scopes: vec![scope.to_string()], interactive: false, claims: None }
    }
}

struct WamState {
    oid: String,
    name: String,
    /// Whether a silent (non-interactive) call gets a token.
    silent: bool,
    calls: Vec<WamCall>,
    issued: u64,
}

/// A WAM that knows one Windows account.
#[derive(Clone)]
pub struct Wam(Arc<Mutex<WamState>>);

impl Wam {
    pub fn new() -> Wam {
        Wam(Arc::new(Mutex::new(WamState { oid: ME_OID.to_string(), name: ME_NAME.to_string(), silent: true, calls: Vec::new(), issued: 0 })))
    }

    pub fn set_silent(&self, silent: bool) {
        self.0.lock().unwrap().silent = silent;
    }

    /// Switches the Windows account WAM signs in.
    pub fn set_account(&self, oid: &str, name: &str) {
        let mut state = self.0.lock().unwrap();
        state.oid = oid.to_string();
        state.name = name.to_string();
    }

    pub fn calls(&self) -> Vec<WamCall> {
        self.0.lock().unwrap().calls.clone()
    }

    pub fn hook(&self) -> FakeWam {
        let wam = self.clone();
        Arc::new(move |scopes: &[String], interactive: bool, claims: Option<&str>| {
            let mut state = wam.0.lock().unwrap();
            state.calls.push(WamCall { scopes: scopes.to_vec(), interactive, claims: claims.map(str::to_string) });
            if !interactive && !state.silent {
                return Err("interaction required".to_string());
            }
            state.issued += 1;
            let audience = scopes.first().map(|s| s.trim_end_matches("/.default").to_string()).unwrap_or_default();
            Ok(jwt(&json!({
                "aud": audience,
                "oid": state.oid,
                "tid": "00000000-0000-0000-0000-0000000000aa",
                "name": state.name,
                "upn": "person.one@synthetic.example",
                "exp": now_secs() + 3600,
                "n": state.issued,
            })))
        })
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// An unsigned JWT with these claims.
pub fn jwt(claims: &Value) -> String {
    format!("{}.{}.c2lnbmF0dXJl", base64url(br#"{"alg":"none","typ":"JWT"}"#), base64url(claims.to_string().as_bytes()))
}

pub fn claims(token: &str) -> Value {
    let payload = token.split('.').nth(1).unwrap();
    serde_json::from_slice(&base64url_decode(payload)).unwrap()
}

const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn base64url(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
        }
    }
    out
}

fn base64url_decode(text: &str) -> Vec<u8> {
    let bits: Vec<u32> = text.bytes().map(|c| ALPHABET.iter().position(|a| *a == c).unwrap() as u32).collect();
    let mut out = Vec::new();
    for chunk in bits.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | b << (18 - 6 * i));
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}
