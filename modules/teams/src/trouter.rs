//! Trouter v4c, the push channel of a live-captions session (docs/protocols/teams.md 5 and 10.7,
//! "Live results" 13 and 18; spec 2026-10-29-live-captions, 5.3): one WebSocket of Socket.IO 0.9
//! frames. It opens anonymously; on `1::` the module sends `user.authenticate` with the chat
//! service's IC3 token, and `trouter.connected` names the callback base (`surl`), the registrar
//! and the reconnect URL. Every downstream request is answered at once; only call callbacks
//! (`/callAgent/`) are handed on, anything else (chat notifications on `/messaging`) is dropped
//! unread. No network and no clock: frames, times and results come in, frames and requests go out.

use serde_json::{Map, Value, json};
use url::Url;
use url::form_urlencoded;

use crate::http::{HttpRequest, SecretString};
use crate::{ProtoError, incompatible};

/// The v4c endpoint (docs 10.7); `trouter.connected` may name another one for reconnections.
pub const DEFAULT_URL: &str = "wss://go.trouter.teams.microsoft.com/v4/c";
const DEFAULT_REGISTRAR: &str = "https://teams.microsoft.com/registrar/prod/V2/registrations";
/// The Trouter client version and the client build the live probe sent ("Live results" 13).
const TROUTER_VERSION: &str = "2026.31.01.1";
const CLIENT_BUILD: &str = "1415/26260.1717.5229.7049";
/// The live probe's ping interval ("Live results" 13), below the server's 40 s timeout.
pub const PING_MS: i64 = 15_000;
/// The registration's lifetime, as the probe registered ("Live results" 13), and when it is
/// renewed.
const REGISTRATION_TTL_S: u32 = 900;
const RENEW_MS: i64 = 600_000;
/// The wait before a failed renewal is tried again.
const REGISTER_RETRY_MS: i64 = 15_000;
/// How long a lost connection or a failing renewal may take to recover; past it the session ends
/// (spec 5.3).
pub const RECOVERY_MS: i64 = 120_000;
/// The longest wait between reconnection attempts; the waits double from 1 s up to it.
const MAX_BACKOFF_MS: i64 = 30_000;

/// What a frame from Trouter means for the session.
pub enum Incoming {
    /// The socket is open: send [`Trouter::authenticate`].
    Open,
    /// `trouter.connected`. `first`: the session's first connection, whose `surl` the
    /// registration and the call's callbacks use. `same_surl`: false when a reconnection got
    /// another callback base, which the call's callbacks no longer reach (spec 5.3).
    Connected { first: bool, same_surl: bool },
    /// A call callback (already answered).
    Callback(Callback),
    /// `trouter.message_loss`, or a request that could not be read: callbacks may be missing.
    Lost,
    /// The server ends the connection (`0::`): close the socket and report [`Trouter::lost`].
    Closing,
    /// Nothing for the session.
    Nothing,
}

/// A call callback. No `Debug`: the body carries names and ids.
pub struct Callback {
    pub url: String,
    pub body: String,
}

/// What is due ([`Trouter::tick`]).
#[derive(Debug, PartialEq)]
pub enum Tick {
    /// Open a new socket at [`Trouter::url`].
    Connect,
    /// Send this frame (a ping).
    Send(String),
    /// Register again ([`Trouter::registration`]), then report it with [`Trouter::registered`].
    Register,
    /// The connection or the registration did not recover in time: the session ends.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Link {
    /// A socket is open or opening, without `trouter.connected` yet.
    Opening,
    Up,
    /// No socket; the next attempt is at this time.
    Down(i64),
}

/// One session's Trouter connection, from the first socket on.
pub struct Trouter {
    /// open_mess's own endpoint id for this account (spec 5.3): `epid` and `registrationId`.
    endpoint: String,
    /// A random id of this client, and its attempts so far, for `con_num`.
    client: String,
    attempts: u32,
    link: Link,
    /// From the first `trouter.connected`: the callback base and the registrar.
    surl: Option<String>,
    registrar: Option<String>,
    /// From the latest `trouter.connected`, for the next connection.
    connectparams: Option<Map<String, Value>>,
    reconnect_url: Option<String>,
    /// Since when the connection, or the renewal, has been failing.
    lost_since: Option<i64>,
    renewal_failing_since: Option<i64>,
    /// Failed connections in a row, for the backoff.
    failures: u32,
    ping_at: Option<i64>,
    pings: u64,
    renew_at: Option<i64>,
}

impl Trouter {
    /// The state of a first socket, about to be opened at [`Trouter::url`].
    pub fn new(endpoint: &str, client: &str) -> Trouter {
        Trouter {
            endpoint: endpoint.to_string(),
            client: client.to_string(),
            attempts: 0,
            link: Link::Opening,
            surl: None,
            registrar: None,
            connectparams: None,
            reconnect_url: None,
            lost_since: None,
            renewal_failing_since: None,
            failures: 0,
            ping_at: None,
            pings: 0,
            renew_at: None,
        }
    }

    /// The callback base of the first connection.
    pub fn surl(&self) -> Option<&str> {
        self.surl.as_deref()
    }

    /// The URL of the next socket: the reconnect URL once known, else the default, with the
    /// probe's v4c query where the URL lacks a key ("Live results" 13). `correlation` is a new
    /// random id.
    pub fn url(&mut self, correlation: &str) -> String {
        self.attempts = self.attempts.saturating_add(1);
        let base = self.reconnect_url.as_deref().unwrap_or(DEFAULT_URL);
        let present: Vec<String> = base
            .split_once('?')
            .map(|(_, query)| form_urlencoded::parse(query.as_bytes()).map(|(key, _)| key.into_owned()).collect())
            .unwrap_or_default();
        let tc = json!({"cv": TROUTER_VERSION, "ua": "TeamsCDL", "hr": "", "v": CLIENT_BUILD}).to_string();
        let con_num = format!("{}_{}", self.client, self.attempts);
        let mut query = form_urlencoded::Serializer::new(String::new());
        for (key, value) in
            [("tc", tc.as_str()), ("timeout", "40"), ("epid", self.endpoint.as_str()), ("cor_id", correlation), ("con_num", con_num.as_str())]
        {
            if !present.iter().any(|p| p == key) {
                query.append_pair(key, value);
            }
        }
        let query = query.finish();
        match (query.is_empty(), base.contains('?')) {
            (true, _) => base.to_string(),
            (false, true) => format!("{base}&{query}"),
            (false, false) => format!("{base}?{query}"),
        }
    }

    /// `user.authenticate` with the IC3 token, and on a reconnection the `connectparams` of the
    /// latest connection (docs 10.7).
    pub fn authenticate(&self, token: &str) -> SecretString {
        let headers = json!({"X-Ms-Test-User": "False", "Authorization": format!("Bearer {token}"), "X-MS-Migration": "True"});
        let mut args = Map::new();
        args.insert("headers".to_string(), headers);
        if let Some(params) = &self.connectparams {
            args.insert("connectparams".to_string(), Value::Object(params.clone()));
        }
        SecretString::new(format!("5:::{}", json!({"name": "user.authenticate", "args": [args]})))
    }

    /// One frame from the server: the frame to send back, if any, and what it means. A
    /// `trouter.connected` the module cannot use is an error; anything else unknown is nothing.
    pub fn handle(&mut self, text: &str, now: i64) -> Result<(Option<String>, Incoming), ProtoError> {
        // `type:id[+]:endpoint[:data]` (docs 10.7).
        let mut parts = text.splitn(4, ':');
        let (kind, id) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let data = parts.nth(1).unwrap_or_default();
        Ok(match kind {
            "0" => (None, Incoming::Closing),
            "1" => (None, Incoming::Open),
            "2" => (Some("2::".to_string()), Incoming::Nothing),
            "3" => request(data),
            "5" => {
                let ack = id.strip_suffix('+').filter(|n| (1..=20).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()));
                (ack.map(|n| format!("6:::{n}")), self.event(data, now)?)
            }
            _ => (None, Incoming::Nothing),
        })
    }

    fn event(&mut self, data: &str, now: i64) -> Result<Incoming, ProtoError> {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Ok(Incoming::Nothing);
        };
        match event.get("name").and_then(Value::as_str) {
            Some("trouter.connected") => self.connected(event.get("args").and_then(|args| args.get(0)), now),
            Some("trouter.message_loss") => Ok(Incoming::Lost),
            _ => Ok(Incoming::Nothing),
        }
    }

    fn connected(&mut self, args: Option<&Value>, now: i64) -> Result<Incoming, ProtoError> {
        let args = args.and_then(Value::as_object).ok_or_else(|| incompatible("trouter.connected.args"))?;
        let surl = args
            .get("surl")
            .and_then(Value::as_str)
            .filter(|surl| Url::parse(surl).is_ok_and(|u| u.scheme() == "https" && u.host_str().is_some()))
            .ok_or_else(|| incompatible("trouter.connected.surl"))?;
        let mut params =
            args.get("connectparams").and_then(Value::as_object).cloned().ok_or_else(|| incompatible("trouter.connected.connectparams"))?;
        params.remove("reconnectUrl");
        params.remove("serviceUrl");
        self.connectparams = Some(params);
        // Only a URL on the manifest's Trouter hosts; the default URL reconnects as well
        // ("Live results" 18).
        self.reconnect_url = args
            .get("reconnectUrl")
            .and_then(Value::as_str)
            .filter(|url| on_host(url, "wss", |host| host.ends_with(".trouter.teams.microsoft.com")))
            .map(str::to_string);
        let first = self.surl.is_none();
        if first {
            // The token goes only to Teams' own registrar host.
            self.registrar = args
                .get("registrarUrl")
                .and_then(Value::as_str)
                .filter(|url| on_host(url, "https", |host| host == "teams.microsoft.com"))
                .map(str::to_string);
        }
        let same_surl = self.surl.get_or_insert_with(|| surl.to_string()) == surl;
        self.link = Link::Up;
        self.failures = 0;
        self.lost_since = None;
        self.ping_at = Some(now.saturating_add(PING_MS));
        Ok(Incoming::Connected { first, same_surl })
    }

    /// The registration of open_mess's endpoint with the first connection's `surl` as its only
    /// transport (docs 5, "Live results" 13), without the token, which the caller adds.
    pub fn registration(&self) -> Option<HttpRequest> {
        let surl = self.surl.as_deref()?;
        let body = json!({
            "clientDescription": {
                "appId": "TeamsCDLWebWorker",
                "aesKey": "",
                "languageId": "en-US",
                "platform": "edge",
                "templateKey": "TeamsCDLWebWorker_2.6",
                "platformUIVersion": CLIENT_BUILD,
            },
            "registrationId": self.endpoint,
            "nodeId": "",
            "transports": {"TROUTER": [{"context": "", "path": surl, "ttl": REGISTRATION_TTL_S}]},
        });
        let mut headers = registrar_headers();
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
        Some(HttpRequest { method: "POST".to_string(), url: self.registrar().to_string(), headers, body: body.to_string().into_bytes() })
    }

    /// The registration's removal at the session's end.
    pub fn unregistration(&self) -> HttpRequest {
        let url = format!("{}/{}", self.registrar().trim_end_matches('/'), self.endpoint);
        HttpRequest { method: "DELETE".to_string(), url, headers: registrar_headers(), body: Vec::new() }
    }

    fn registrar(&self) -> &str {
        self.registrar.as_deref().unwrap_or(DEFAULT_REGISTRAR)
    }

    /// The outcome of a registration: the next renewal, or another try soon.
    pub fn registered(&mut self, ok: bool, now: i64) {
        if ok {
            self.renewal_failing_since = None;
            self.renew_at = Some(now.saturating_add(RENEW_MS));
        } else {
            self.renewal_failing_since.get_or_insert(now);
            self.renew_at = Some(now.saturating_add(REGISTER_RETRY_MS));
        }
    }

    /// The socket closed, failed to open or was closed for `0::`: the next one after a wait that
    /// doubles with each failure. A second report of the same loss changes nothing.
    pub fn lost(&mut self, now: i64) {
        if matches!(self.link, Link::Down(_)) {
            return;
        }
        let wait = (1000_i64 << self.failures.min(5)).min(MAX_BACKOFF_MS);
        self.failures = self.failures.saturating_add(1);
        self.link = Link::Down(now.saturating_add(wait));
        self.lost_since.get_or_insert(now);
        self.ping_at = None;
    }

    /// The earliest time something is due, for the session's timer.
    pub fn next(&self) -> Option<i64> {
        let down = match self.link {
            Link::Down(at) => Some(at),
            Link::Opening | Link::Up => None,
        };
        [down, self.ping_at, self.renew_at, self.fail_at()].into_iter().flatten().min()
    }

    fn fail_at(&self) -> Option<i64> {
        [self.lost_since, self.renewal_failing_since].into_iter().flatten().min().map(|since| since.saturating_add(RECOVERY_MS))
    }

    /// What is due at `now`, one thing per call; `None` once nothing is.
    pub fn tick(&mut self, now: i64) -> Option<Tick> {
        if self.fail_at().is_some_and(|at| now >= at) {
            return Some(Tick::Failed);
        }
        if let Link::Down(at) = self.link
            && now >= at
        {
            self.link = Link::Opening;
            return Some(Tick::Connect);
        }
        if self.ping_at.is_some_and(|at| now >= at) {
            self.ping_at = Some(now.saturating_add(PING_MS));
            self.pings = self.pings.saturating_add(1);
            return Some(Tick::Send(format!("5:{}+::{{\"name\":\"ping\"}}", self.pings)));
        }
        if self.renew_at.is_some_and(|at| now >= at) {
            self.renew_at = None;
            return Some(Tick::Register);
        }
        None
    }
}

fn registrar_headers() -> Vec<(String, String)> {
    [("X-Ms-Test-User", "False"), ("X-MS-Migration", "True"), ("Accept", "application/json, text/javascript")]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn on_host(url: &str, scheme: &str, host: impl Fn(&str) -> bool) -> bool {
    Url::parse(url).is_ok_and(|u| {
        u.scheme() == scheme
            && u.username().is_empty()
            && u.password().is_none()
            && u.port_or_known_default() == Some(443)
            && u.host_str().is_some_and(&host)
    })
}

/// The most a compressed callback may inflate to.
const MAX_CALLBACK: u64 = 1024 * 1024;

/// The body of a compressed callback (`X-Microsoft-Skype-Content-Encoding: gzip`): base64 of
/// gzip, else of zlib, as the probe read them (docs, "Live results" 19).
fn inflate(body: &str) -> Option<String> {
    use std::io::Read;
    let bytes = crate::session::base64_decode(body)?;
    let read = |reader: Box<dyn Read + '_>| -> Option<Vec<u8>> {
        let mut out = Vec::new();
        reader.take(MAX_CALLBACK + 1).read_to_end(&mut out).ok()?;
        (out.len() as u64 <= MAX_CALLBACK).then_some(out)
    };
    let out = read(Box::new(flate2::read::GzDecoder::new(bytes.as_slice())))
        .or_else(|| read(Box::new(flate2::read::ZlibDecoder::new(bytes.as_slice()))))?;
    String::from_utf8(out).ok()
}

/// A downstream request (`3:::`): answered 200 (as the probe did, "Live results" 13), and handed
/// on only when it is a call callback. One that cannot be answered or read may have been a
/// callback, so it counts as lost.
fn request(data: &str) -> (Option<String>, Incoming) {
    let Ok(Value::Object(request)) = serde_json::from_str::<Value>(data) else {
        return (None, Incoming::Lost);
    };
    let Some(id) = request.get("id").filter(|id| id.is_number()) else {
        return (None, Incoming::Lost);
    };
    let url = request.get("url").and_then(Value::as_str).unwrap_or_default();
    if !url.contains("/callAgent/") {
        return (Some(response(id, 200)), Incoming::Nothing);
    }
    // The join's endpoint capabilities ask for compressed callbacks (bit 128, as the web client;
    // the acceptance comes compressed, "Live results" 19). One that cannot be inflated is answered
    // as the client answers it.
    let compressed = request.get("headers").and_then(Value::as_object).is_some_and(|headers| {
        headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("X-Microsoft-Skype-Content-Encoding") && value.as_str().is_some_and(|v| v.eq_ignore_ascii_case("gzip"))
        })
    });
    let body = request.get("body").and_then(Value::as_str);
    match if compressed { body.and_then(inflate) } else { body.map(str::to_string) } {
        Some(body) => (Some(response(id, 200)), Incoming::Callback(Callback { url: url.to_string(), body })),
        None => (Some(response(id, 400)), Incoming::Lost),
    }
}

fn response(id: &Value, status: u16) -> String {
    format!("3:::{}", json!({"id": id, "status": status, "headers": {}, "body": ""}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENDPOINT: &str = "00000000-0000-4000-8000-0000000000e1";
    const SURL: &str = "https://pub-ent-synthetic.trouter.teams.microsoft.com/v4/f/c3ludGhldGlj/";
    const T0: i64 = 1_800_000_000_000;

    fn connected(surl: &str, extra: Value) -> String {
        let mut args = json!({
            "surl": surl,
            "connectparams": {"sig": "c2ln", "se": "1800000000", "reconnectUrl": "x", "serviceUrl": "y"},
            "ttl": 604_800,
        });
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        format!("5:::{}", json!({"name": "trouter.connected", "args": [args]}))
    }

    /// Standard base64, for the compressed callbacks.
    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
            for i in 0..=chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            }
        }
        out
    }

    /// A Trouter through its first `trouter.connected`.
    fn up() -> Trouter {
        let mut t = Trouter::new(ENDPOINT, "client");
        t.url("cor");
        t.handle(&connected(SURL, json!({})), T0).unwrap();
        t
    }

    fn query(url: &str) -> Vec<(String, String)> {
        let (_, q) = url.split_once('?').unwrap();
        form_urlencoded::parse(q.as_bytes()).into_owned().collect()
    }

    fn get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
        pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn the_first_socket_goes_to_the_default_url_with_the_probes_query() {
        let mut t = Trouter::new(ENDPOINT, "client");
        let url = t.url("cor-1");
        assert!(url.starts_with("wss://go.trouter.teams.microsoft.com/v4/c?"), "{url}");
        let q = query(&url);
        let tc: Value = serde_json::from_str(get(&q, "tc").unwrap()).unwrap();
        assert_eq!(tc, json!({"cv": "2026.31.01.1", "ua": "TeamsCDL", "hr": "", "v": CLIENT_BUILD}));
        assert_eq!(get(&q, "timeout"), Some("40"));
        assert_eq!(get(&q, "epid"), Some(ENDPOINT));
        assert_eq!(get(&q, "cor_id"), Some("cor-1"));
        assert_eq!(get(&q, "con_num"), Some("client_1"));
        assert_eq!(get(&q, "ccid"), None);
        assert_eq!(get(&query(&t.url("cor-2")), "con_num"), Some("client_2"));
    }

    #[test]
    fn open_asks_for_authentication_without_connectparams_the_first_time() {
        let mut t = Trouter::new(ENDPOINT, "client");
        assert!(matches!(t.handle("1::", T0).unwrap(), (None, Incoming::Open)));
        let frame = t.authenticate("tok");
        let event: Value = serde_json::from_str(frame.expose().strip_prefix("5:::").unwrap()).unwrap();
        assert_eq!(event["name"], "user.authenticate");
        assert_eq!(event["args"][0]["headers"], json!({"X-Ms-Test-User": "False", "Authorization": "Bearer tok", "X-MS-Migration": "True"}));
        assert!(event["args"][0].get("connectparams").is_none());
        // The token never shows in a log.
        assert_eq!(format!("{frame:?}"), "***");
    }

    #[test]
    fn connected_gives_the_surl_and_the_registration() {
        let mut t = Trouter::new(ENDPOINT, "client");
        let (reply, incoming) = t.handle(&connected(SURL, json!({})), T0).unwrap();
        assert!(reply.is_none());
        assert!(matches!(incoming, Incoming::Connected { first: true, same_surl: true }));
        assert_eq!(t.surl(), Some(SURL));
        let reg = t.registration().unwrap();
        assert_eq!((reg.method.as_str(), reg.url.as_str()), ("POST", DEFAULT_REGISTRAR));
        let body: Value = serde_json::from_slice(&reg.body).unwrap();
        assert_eq!(body["registrationId"], ENDPOINT);
        assert_eq!(body["transports"]["TROUTER"], json!([{"context": "", "path": SURL, "ttl": 900}]));
        assert_eq!(body["clientDescription"]["templateKey"], "TeamsCDLWebWorker_2.6");
        assert!(reg.headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("authorization")));
        let unreg = t.unregistration();
        assert_eq!((unreg.method.as_str(), unreg.url), ("DELETE", format!("{DEFAULT_REGISTRAR}/{ENDPOINT}")));
    }

    #[test]
    fn only_teams_own_registrar_gets_the_registration() {
        let named = "https://teams.microsoft.com/registrar/prod/V2/registrations/";
        let mut t = Trouter::new(ENDPOINT, "client");
        t.handle(&connected(SURL, json!({"registrarUrl": named})), T0).unwrap();
        assert_eq!(t.registration().unwrap().url, named);
        assert_eq!(t.unregistration().url, format!("{}/{ENDPOINT}", named.trim_end_matches('/')));
        for elsewhere in [
            "https://registrar.example.com/r",
            "http://teams.microsoft.com/r",
            "https://teams.microsoft.com:8443/r",
            "https://u@teams.microsoft.com/r",
        ] {
            let mut t = Trouter::new(ENDPOINT, "client");
            t.handle(&connected(SURL, json!({"registrarUrl": elsewhere})), T0).unwrap();
            assert_eq!(t.registration().unwrap().url, DEFAULT_REGISTRAR, "{elsewhere}");
        }
    }

    #[test]
    fn a_reconnection_uses_the_reconnect_url_and_the_cached_connectparams() {
        let reconnect = "wss://pub-ent-synthetic.trouter.teams.microsoft.com:443/v4/c";
        let mut t = Trouter::new(ENDPOINT, "client");
        t.url("cor");
        t.handle(&connected(SURL, json!({"reconnectUrl": reconnect})), T0).unwrap();
        t.lost(T0);
        let url = t.url("cor-2");
        assert!(url.starts_with("wss://pub-ent-synthetic.trouter.teams.microsoft.com:443/v4/c?"), "{url}");
        assert_eq!(get(&query(&url), "con_num"), Some("client_2"));
        let frame = t.authenticate("tok");
        let event: Value = serde_json::from_str(frame.expose().strip_prefix("5:::").unwrap()).unwrap();
        assert_eq!(event["args"][0]["connectparams"], json!({"sig": "c2ln", "se": "1800000000"}));
        // The same surl again: the call's callbacks still arrive.
        assert!(matches!(t.handle(&connected(SURL, json!({})), T0).unwrap().1, Incoming::Connected { first: false, same_surl: true }));
        // No reconnect URL this time: the default one.
        assert!(t.url("cor-3").starts_with(DEFAULT_URL));
    }

    #[test]
    fn a_reconnect_url_off_the_trouter_hosts_is_not_used() {
        for elsewhere in
            ["wss://evil.example.com/v4/c", "ws://a.trouter.teams.microsoft.com/v4/c", "wss://trouter.teams.microsoft.com.example.com/v4/c"]
        {
            let mut t = Trouter::new(ENDPOINT, "client");
            t.handle(&connected(SURL, json!({"reconnectUrl": elsewhere})), T0).unwrap();
            assert!(t.url("cor").starts_with(DEFAULT_URL), "{elsewhere}");
        }
    }

    #[test]
    fn a_new_surl_after_a_reconnection_is_reported() {
        let mut t = up();
        t.lost(T0);
        let other = "https://pub-ent-other.trouter.teams.microsoft.com/v4/f/b3RoZXI/";
        assert!(matches!(t.handle(&connected(other, json!({})), T0).unwrap().1, Incoming::Connected { first: false, same_surl: false }));
        // The registration keeps the first one.
        assert_eq!(t.surl(), Some(SURL));
    }

    #[test]
    fn a_connected_event_without_a_usable_surl_is_an_error() {
        for args in [
            json!([]),
            json!(["text"]),
            json!([{"connectparams": {}}]),
            json!([{"surl": "http://a.trouter.teams.microsoft.com/", "connectparams": {}}]),
            json!([{"surl": 7, "connectparams": {}}]),
            json!([{"surl": SURL}]),
            json!([{"surl": SURL, "connectparams": "x"}]),
        ] {
            let frame = format!("5:::{}", json!({"name": "trouter.connected", "args": args}));
            let mut t = Trouter::new(ENDPOINT, "client");
            assert!(matches!(t.handle(&frame, T0), Err(ProtoError::Incompatible(_))), "{args}");
        }
    }

    #[test]
    fn heartbeats_and_acks_are_answered() {
        let mut t = up();
        assert!(matches!(t.handle("2::", T0).unwrap(), (Some(r), Incoming::Nothing) if r == "2::"));
        assert!(matches!(t.handle(r#"5:7+::{"name":"audience.subscriptionresult"}"#, T0).unwrap(), (Some(r), Incoming::Nothing) if r == "6:::7"));
        // An ack id that is not a short number gets no ack.
        for frame in [r#"5:x+::{"name":"a"}"#, r#"5:+::{"name":"a"}"#, r#"5:123456789012345678901+::{"name":"a"}"#] {
            assert!(t.handle(frame, T0).unwrap().0.is_none(), "{frame}");
        }
        // The server's acks of our pings.
        assert!(matches!(t.handle("6:::3", T0).unwrap(), (None, Incoming::Nothing)));
    }

    #[test]
    fn a_call_callback_is_answered_and_handed_on() {
        let mut t = up();
        let url = format!("{SURL}callAgent/session/tag/call/acceptance/");
        let frame = format!("3:::{}", json!({"id": 5, "method": "POST", "url": url, "headers": {"MS-CV": "cv"}, "body": "{\"callAcceptance\":{}}"}));
        let (reply, incoming) = t.handle(&frame, T0).unwrap();
        let reply: Value = serde_json::from_str(reply.unwrap().strip_prefix("3:::").unwrap()).unwrap();
        assert_eq!(reply, json!({"id": 5, "status": 200, "headers": {}, "body": ""}));
        let Incoming::Callback(callback) = incoming else { panic!("not a callback") };
        assert_eq!((callback.url, callback.body.as_str()), (url, "{\"callAcceptance\":{}}"));
    }

    #[test]
    fn chat_notifications_are_answered_and_dropped_unread() {
        let mut t = up();
        // The body is not even JSON: it is never parsed.
        let frame = format!("3:::{}", json!({"id": 9, "method": "POST", "url": "https://synthetic/v4/f/x/messaging", "body": "not json"}));
        assert!(matches!(t.handle(&frame, T0).unwrap(), (Some(r), Incoming::Nothing) if r.contains(r#""id":9"#) && r.contains(r#""status":200"#)));
    }

    #[test]
    fn a_callback_that_cannot_be_read_counts_as_lost() {
        let mut t = up();
        let url = format!("{SURL}callAgent/s/t/conversation/rosterUpdate/");
        let gzip = format!("3:::{}", json!({"id": 1, "url": url, "headers": {"x-microsoft-skype-content-encoding": "GZIP"}, "body": "H4sI"}));
        assert!(matches!(t.handle(&gzip, T0).unwrap(), (Some(r), Incoming::Lost) if r.contains(r#""status":400"#)));
        // A compressed callback that inflates (gzip or zlib) is read; one past the limit is not.
        let packed = |bytes: &[u8], gzip: bool| {
            use std::io::Write;
            let compressed = if gzip {
                let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(bytes).unwrap();
                e.finish().unwrap()
            } else {
                let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(bytes).unwrap();
                e.finish().unwrap()
            };
            base64(&compressed)
        };
        for gzip in [true, false] {
            let frame = format!(
                "3:::{}",
                json!({"id": 2, "url": url, "headers": {"X-Microsoft-Skype-Content-Encoding": "gzip"}, "body": packed(b"{\"callAcceptance\":{}}", gzip)})
            );
            let (reply, incoming) = t.handle(&frame, T0).unwrap();
            assert!(reply.unwrap().contains(r#""status":200"#));
            assert!(matches!(incoming, Incoming::Callback(c) if c.body == "{\"callAcceptance\":{}}"));
        }
        let bomb = format!(
            "3:::{}",
            json!({"id": 3, "url": url, "headers": {"X-Microsoft-Skype-Content-Encoding": "gzip"}, "body": packed(&vec![b' '; 2 * 1024 * 1024], true)})
        );
        assert!(matches!(t.handle(&bomb, T0).unwrap(), (_, Incoming::Lost)));
        let no_body = format!("3:::{}", json!({"id": 2, "url": url}));
        assert!(matches!(t.handle(&no_body, T0).unwrap(), (Some(r), Incoming::Lost) if r.contains(r#""status":400"#)));
        // Without a numeric id it cannot be answered at all.
        for frame in ["3:::", "3:::not json", "3:::[1]", r#"3:::{"url":"x/callAgent/"}"#, r#"3:::{"id":"5","url":"x/callAgent/","body":""}"#] {
            assert!(matches!(t.handle(frame, T0).unwrap(), (None, Incoming::Lost)), "{frame}");
        }
    }

    #[test]
    fn message_loss_and_the_servers_close_are_reported() {
        let mut t = up();
        assert!(matches!(t.handle(r#"5:::{"name":"trouter.message_loss","args":[{}]}"#, T0).unwrap().1, Incoming::Lost));
        assert!(matches!(t.handle("0::", T0).unwrap().1, Incoming::Closing));
        assert!(matches!(t.handle("0::timeout", T0).unwrap().1, Incoming::Closing));
    }

    #[test]
    fn hostile_frames_never_panic() {
        let mut t = up();
        let long = "5".repeat(100_000);
        for frame in ["", ":", ":::", "5", "5:", "5::", "5:::", "5:::{", "5:::null", "5:::{\"name\":7}", "9:::x", "3", "\u{0}", "é:é:é:é", &long]
        {
            assert!(t.handle(frame, T0).is_ok(), "{frame:.20}");
        }
        assert_eq!(t.surl(), Some(SURL));
    }

    #[test]
    fn a_live_connection_pings_every_15_s() {
        let mut t = up();
        assert_eq!(t.next(), Some(T0 + PING_MS));
        assert_eq!(t.tick(T0 + PING_MS - 1), None);
        assert_eq!(t.tick(T0 + PING_MS), Some(Tick::Send(r#"5:1+::{"name":"ping"}"#.to_string())));
        assert_eq!(t.tick(T0 + PING_MS), None);
        assert_eq!(t.tick(T0 + 2 * PING_MS), Some(Tick::Send(r#"5:2+::{"name":"ping"}"#.to_string())));
    }

    #[test]
    fn a_lost_connection_is_retried_with_doubling_waits_then_fails_after_two_minutes() {
        let mut t = up();
        t.lost(T0);
        // A second report of the same loss (the server's `0::`, then the socket's close).
        t.lost(T0);
        // Every attempt fails; no ping is due on a socket that is gone.
        let mut attempts = vec![T0];
        let failed = loop {
            let at = t.next().unwrap();
            match t.tick(at) {
                Some(Tick::Connect) => {
                    attempts.push(at);
                    t.lost(at);
                }
                Some(Tick::Failed) => break at,
                other => panic!("{other:?} at {at}"),
            }
        };
        let gaps: Vec<i64> = attempts.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(gaps, [1000, 2000, 4000, 8000, 16_000, 30_000, 30_000]);
        assert_eq!(failed, T0 + RECOVERY_MS);
    }

    #[test]
    fn a_reconnection_in_time_starts_over() {
        let mut t = up();
        t.lost(T0);
        assert_eq!(t.tick(T0 + 1000), Some(Tick::Connect));
        t.url("cor");
        t.handle(&connected(SURL, json!({})), T0 + 1500).unwrap();
        assert_eq!(t.next(), Some(T0 + 1500 + PING_MS));
        assert_ne!(t.tick(T0 + RECOVERY_MS + 5000), Some(Tick::Failed));
        // The next loss waits 1 s again.
        t.lost(T0 + 200_000);
        assert_eq!(t.next(), Some(T0 + 201_000));
    }

    #[test]
    fn the_registration_is_renewed_and_a_failing_renewal_ends_the_session() {
        let mut t = up();
        t.registered(true, T0);
        assert_eq!(t.tick(T0 + RENEW_MS - 1), Some(Tick::Send(r#"5:1+::{"name":"ping"}"#.to_string())));
        while t.tick(T0 + RENEW_MS - 1).is_some() {}
        assert_eq!(t.tick(T0 + RENEW_MS), Some(Tick::Register));
        let failing = T0 + RENEW_MS;
        t.registered(false, failing);
        assert_eq!(t.renew_at, Some(failing + REGISTER_RETRY_MS));
        t.registered(false, failing + REGISTER_RETRY_MS);
        assert_eq!(t.tick(failing + RECOVERY_MS), Some(Tick::Failed));
        // A renewal that works in time clears it.
        let mut t = up();
        t.registered(false, T0);
        t.registered(true, T0 + REGISTER_RETRY_MS);
        assert_ne!(t.tick(T0 + RECOVERY_MS), Some(Tick::Failed));
    }
}
