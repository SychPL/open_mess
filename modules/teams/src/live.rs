//! Live-captions sessions in the module instance (spec 2026-10-29-live-captions, 4 and 5.3): the
//! three tools, the Trouter socket, the media connection, the timers and the session records,
//! around the pure `captions`, `trouter` and `sdp`.

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::{Value, json};

use crate::api::{self, Service};
use crate::captions::{self, Call, Due, Links, Live, Me, Reason, Record, Session as Captions};
use crate::component::bindings::open_mess::module::http::{HttpError, Response};
use crate::component::bindings::open_mess::module::types::ModuleError;
use crate::component::bindings::open_mess::module::{host, rtc, storage, websocket};
use crate::sdp;
use crate::session::Session;
use crate::trouter::{Incoming, Tick, Trouter};

/// Sessions' timers: one per account, from this id on (the poll's is 1).
const TIMER_BASE: u32 = 100;
/// At most this many sessions at once in the module, as the host's media connections (spec 5.2).
const MAX_SESSIONS: usize = 4;
/// The call store may lag a fresh join (spec 4.1, R15): two more reads, this far apart.
const STORE_READS: usize = 3;
const STORE_WAIT_MS: u32 = 5000;
const LABEL: &str = "main-channel";
const NOT_LISTED: &str = "Teams does not list you in this meeting yet; join it in Teams first, or try again in a moment";
const CHANGED: &str = "the captions session changed, read again without session and cursor";

/// One account's session and the connections it holds.
struct Leg {
    account: String,
    captions: Captions,
    trouter: Trouter,
    ws: Option<u32>,
    rtc: Option<u32>,
    live: Live,
    endpoint: String,
    participant: String,
    /// The session id in the callback links.
    callbacks: String,
    tag: u32,
    chain: String,
    media_leg: String,
    leave: Option<String>,
    /// The join may have reached the service: the session ends with `leave`.
    joined: bool,
    registered: bool,
}

thread_local! {
    static LEGS: RefCell<HashMap<String, Leg>> = RefCell::new(HashMap::new());
    /// Accounts whose session records still wait for a `leave`: the next try and the last one.
    static RETRIES: RefCell<HashMap<String, (i64, i64)>> = RefCell::new(HashMap::new());
    /// The account of each session timer, by its place after `TIMER_BASE`.
    static SLOTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn records_key(account: &str) -> String {
    format!("captions-legs:{account}")
}

fn records(account: &str) -> Vec<Record> {
    storage::get(&records_key(account)).map(|bytes| captions::decode_records(&bytes)).unwrap_or_default()
}

fn store_records(account: &str, records: &[Record]) {
    let key = records_key(account);
    if records.is_empty() {
        storage::delete(&key);
    } else if storage::set(&key, &captions::encode_records(records)).is_err() {
        host::log(1, "a live-captions session record was not stored");
    }
}

/// open_mess's own endpoint id for the account (spec 5.3): made once, kept in storage, never the
/// official client's.
fn endpoint(account: &str) -> String {
    let key = format!("captions-endpoint:{account}");
    if let Some(id) = storage::get(&key).and_then(|bytes| String::from_utf8(bytes).ok()).filter(|id| id.len() == 36) {
        return id;
    }
    let id = api::uuid();
    if storage::set(&key, id.as_bytes()).is_err() {
        host::log(1, "the live-captions endpoint id was not stored");
    }
    id
}

fn timer(account: &str) -> u32 {
    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let at = slots.iter().position(|a| a == account).unwrap_or_else(|| {
            slots.push(account.to_string());
            slots.len() - 1
        });
        TIMER_BASE + u32::try_from(at).unwrap_or(0)
    })
}

fn account_of(find: impl Fn(&Leg) -> bool) -> Option<String> {
    LEGS.with(|legs| legs.borrow().values().find(|leg| find(leg)).map(|leg| leg.account.clone()))
}

fn take(account: &str) -> Option<Leg> {
    LEGS.with(|legs| legs.borrow_mut().remove(account))
}

/// Puts a leg back, or forgets it once its cues are gone, and sets its timer.
fn put(leg: Leg, now: i64) {
    let account = leg.account.clone();
    if !leg.captions.gone(now) {
        LEGS.with(|legs| legs.borrow_mut().insert(account.clone(), leg));
    }
    schedule(&account, now);
}

/// The account's timer, for the earliest thing due in its session and its `leave` retries.
fn schedule(account: &str, now: i64) {
    let leg = LEGS.with(|legs| {
        legs.borrow().get(account).map(|leg| {
            let trouter = if leg.captions.active() { leg.trouter.next() } else { None };
            [trouter, leg.captions.next_at()].into_iter().flatten().min()
        })
    });
    let retry = RETRIES.with(|r| r.borrow().get(account).map(|(next, _)| *next));
    if let Some(at) = [leg.flatten(), retry].into_iter().flatten().min() {
        let after = u32::try_from(at.saturating_sub(now).clamp(50, i64::from(u32::MAX))).unwrap_or(u32::MAX);
        host::set_timer(timer(account), after);
    }
}

/// Runs `f` on the account's leg with its session, outside a tool call (an event; the caller has
/// started the call's budget).
fn event(account: &str, f: impl FnOnce(&mut Leg, &mut Session, i64)) {
    let Some(mut leg) = take(account) else { return };
    let now = api::now_ms();
    let done = api::with_session(account, |s| {
        f(&mut leg, s, now);
        Ok(())
    });
    if done.is_err() {
        // No session to send anything with: the connections go, the record stays for `restore`.
        close(&mut leg);
        leg.captions.end(Reason::Protocol, now);
    }
    put(leg, now);
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn conversation_arg(args: &Value) -> Result<&str, ModuleError> {
    text(args, "conversation")
        .filter(|c| c.starts_with("19:"))
        .ok_or_else(|| ModuleError::InvalidArgs("conversation must be a 19: thread id".to_string()))
}

fn region(s: &Session) -> Option<(&str, &str)> {
    s.region.as_ref().map(|(region, partition)| (region.as_str(), partition.as_str()))
}

fn me<'a>(s: &'a Session, leg: &'a Leg, mri: &'a str) -> Me<'a> {
    Me { mri, name: &s.name, endpoint: &leg.endpoint, participant: &leg.participant }
}

fn describe(e: &HttpError) -> String {
    match e {
        HttpError::Blocked(why) | HttpError::Network(why) => why.clone(),
        HttpError::Timeout => "timed out".to_string(),
        HttpError::TooLarge => "too large".to_string(),
        HttpError::Throttled(secs) => format!("throttled for {secs} s"),
    }
}

/// `teams_join_captions`, once the user approved it (spec 4.1): the checks, the session and the
/// Trouter socket; the join itself follows in events.
pub fn join(s: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let conversation = conversation_arg(args)?.to_string();
    let account = s.account.clone();
    let (busy, active) = LEGS.with(|legs| {
        let legs = legs.borrow();
        (legs.get(&account).is_some_and(|leg| leg.captions.active()), legs.values().filter(|leg| leg.captions.active()).count())
    });
    if busy {
        return Err(ModuleError::Platform("already reading the captions of another meeting; leave it first".to_string()));
    }
    if active >= MAX_SESSIONS {
        return Err(ModuleError::Platform(format!("connection_limit: at most {MAX_SESSIONS} live-captions sessions at once")));
    }
    let mut live = None;
    for read in 0..STORE_READS {
        if read > 0 && !api::wait(STORE_WAIT_MS) {
            break;
        }
        let body = api::ok(api::conversation(s, &captions::call_store(&s.me()), false)?)?;
        live = captions::live_call(&body, &conversation).map_err(api::proto)?;
        if live.is_some() {
            break;
        }
    }
    let live = live.ok_or_else(|| ModuleError::Platform(NOT_LISTED.to_string()))?;
    let now = api::now_ms();
    let endpoint = endpoint(&account);
    let mut trouter = Trouter::new(&endpoint, &api::uuid());
    let ws = websocket::open(&trouter.url(&api::uuid()), &[])
        .map_err(|e| ModuleError::Platform(format!("Teams could not be reached: {}", describe(&e))))?;
    let token = format!("{:016x}{:016x}", api::random(), api::random());
    let leg = Leg {
        account: account.clone(),
        captions: Captions::new(&conversation, &token, now),
        trouter,
        ws: Some(ws),
        rtc: None,
        live,
        endpoint,
        participant: api::uuid(),
        callbacks: api::uuid(),
        tag: u32::try_from(api::random() >> 32).unwrap_or(0),
        chain: api::uuid(),
        media_leg: api::uuid().replace('-', ""),
        leave: None,
        joined: false,
        registered: false,
    };
    put(leg, now);
    Ok(json!({"state": "joining"}))
}

/// `teams_read_captions` (spec 4.2).
pub fn read(s: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let conversation = conversation_arg(args)?;
    let session = text(args, "session");
    let cursor = match args.get("cursor") {
        None => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| ModuleError::InvalidArgs("cursor must be a whole number".to_string()))?),
    };
    if cursor.is_some() && session.is_none() {
        return Err(ModuleError::InvalidArgs("cursor needs session".to_string()));
    }
    let max_bytes = match args.get("max_bytes") {
        None => 65536,
        Some(v) => v
            .as_u64()
            .filter(|n| (4096..=262_144).contains(n))
            .ok_or_else(|| ModuleError::InvalidArgs("max_bytes must be 4096-262144".to_string()))?,
    };
    let now = api::now_ms();
    let read = LEGS.with(|legs| {
        legs.borrow_mut()
            .get_mut(&s.account)
            .filter(|leg| leg.captions.conversation == conversation)
            .map(|leg| leg.captions.read(session, cursor, usize::try_from(max_bytes).unwrap_or(65536), now))
    });
    match read {
        Some(Ok(page)) => {
            schedule(&s.account, now);
            Ok(page)
        }
        Some(Err(_)) => Err(ModuleError::InvalidArgs(CHANGED.to_string())),
        None => Ok(json!({
            "state": "not_joined", "session": null, "captions_on": null, "next_cursor": 0, "missed": 0, "language": null,
            "reason": null, "untrusted": {"cues": []},
        })),
    }
}

/// `teams_leave_captions` (spec 4.3): leaves the meeting of this conversation's session.
pub fn leave(s: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let conversation = conversation_arg(args)?;
    let now = api::now_ms();
    let Some(mut leg) = take(&s.account) else { return Ok(json!({"state": "not_joined"})) };
    if leg.captions.conversation != conversation {
        put(leg, now);
        return Ok(json!({"state": "not_joined"}));
    }
    end(&mut leg, s, Reason::Left, now);
    let state = leg.captions.state().name();
    put(leg, now);
    Ok(json!({"state": state}))
}

/// Ends the session: the connections go, the registration is removed, and the meeting is left
/// (spec 4.3). A `leave` that fails is tried again later from the record.
fn end(leg: &mut Leg, s: &mut Session, reason: Reason, now: i64) {
    if !leg.captions.active() {
        return;
    }
    leg.captions.end(reason, now);
    close(leg);
    if leg.registered && api::send(s, Service::Chat, &leg.trouter.unregistration(), false).is_err() {
        host::log(1, "the live-captions registration was not removed; it lapses by itself");
    }
    let record = Record { controller: leg.live.controller.clone(), endpoint: leg.endpoint.clone(), participant: leg.participant.clone(), at_ms: now };
    if !leg.joined || leave_one(s, &record, leg.leave.as_deref()) {
        forget(&leg.account, &leg.participant);
    } else {
        retry_from(&leg.account, now);
    }
}

fn close(leg: &mut Leg) {
    if let Some(conn) = leg.rtc.take() {
        rtc::close(conn);
    }
    if let Some(conn) = leg.ws.take() {
        websocket::close(conn);
    }
}

fn forget(account: &str, participant: &str) {
    let mut all = records(account);
    all.retain(|r| r.participant != participant);
    store_records(account, &all);
}

/// Starts the account's `leave` retries, unless they are going on (spec 4.3: every 30 s, for at
/// most 10 minutes).
fn retry_from(account: &str, now: i64) {
    RETRIES.with(|r| {
        r.borrow_mut()
            .entry(account.to_string())
            .or_insert((now.saturating_add(captions::LEAVE_RETRY_MS), now.saturating_add(captions::LEAVE_RETRIES_FOR_MS)));
    });
}

/// A conversation request with the account's skype token.
fn service(s: &mut Session, leg: &Leg, method: &str, url: &str, body: Option<&Value>, write: bool) -> Result<Response, ModuleError> {
    let request = captions::service(method, url, body, &leg.chain, &api::uuid(), region(s));
    api::conversation(s, &request, write)
}

/// One `leave` of a recorded leg: its conversation's `leave` link (read again unless known), as
/// that endpoint. True when the leg is gone: left, or the service no longer knows it.
fn leave_one(s: &mut Session, record: &Record, link: Option<&str>) -> bool {
    let mri = s.me();
    let gone = |status: u16| (200..=299).contains(&status) || status == 404 || status == 410;
    let request = |method: &str, url: &str, body: Option<&Value>| captions::service(method, url, body, &api::uuid(), &api::uuid(), None);
    let link = match link {
        Some(link) => link.to_string(),
        None => match api::conversation(s, &request("GET", &record.controller, None), false) {
            Ok(r) if r.status == 404 || r.status == 410 => return true,
            Ok(r) if (200..=299).contains(&r.status) => match captions::conversation(&r.body, &mri, &record.endpoint) {
                Ok((link, _)) => link,
                Err(_) => return false,
            },
            _ => return false,
        },
    };
    let body = captions::leave(&Me { mri: &mri, name: &s.name, endpoint: &record.endpoint, participant: &record.participant });
    matches!(api::conversation(s, &request("POST", &link, Some(&body)), true), Ok(r) if gone(r.status))
}

/// `leave` for the account's records of legs no session holds (spec 4.3): every one, a stale one
/// once whatever the answer. Whether any is left to try again.
fn leave_records(s: &mut Session, now: i64) -> bool {
    let held = LEGS.with(|legs| legs.borrow().get(&s.account).filter(|leg| leg.captions.active()).map(|leg| leg.participant.clone()));
    let (mut kept, mut waiting) = (Vec::new(), false);
    for record in records(&s.account) {
        let stale = now.saturating_sub(record.at_ms) > captions::STALE_RECORD_MS;
        if Some(&record.participant) == held.as_ref() {
            kept.push(record);
        } else if !api::has_time() || (!leave_one(s, &record, None) && !stale) {
            waiting = true;
            kept.push(record);
        }
    }
    store_records(&s.account, &kept);
    waiting
}

/// After `restore` (daemon start, or a new instance after a crash): the meetings of legs that
/// were never left are left now (spec 4.3).
pub fn restore(account: &str) {
    if records(account).is_empty() {
        return;
    }
    let now = api::now_ms();
    match api::with_session(account, |s| Ok(leave_records(s, now))) {
        Ok(true) => retry_from(account, now),
        Ok(false) => {}
        Err(_) => host::log(1, "the meetings of earlier live-captions sessions were not left yet"),
    }
    schedule(account, now);
}

/// Before the daemon stops, an update or a module scan: the session ends and leaves (spec 4.3).
pub fn end_sessions(account: &str) {
    event(account, |leg, s, now| end(leg, s, Reason::Update, now));
}

/// Before the account goes: the session ends and leaves, with one more try (spec 4.3).
pub fn sign_out(account: &str) {
    event(account, |leg, s, now| {
        end(leg, s, Reason::Left, now);
        leave_records(s, now);
    });
    RETRIES.with(|r| r.borrow_mut().remove(account));
    LEGS.with(|legs| legs.borrow_mut().remove(account));
}

pub fn ws_message(conn: u32, text: &str) {
    if let Some(account) = account_of(|leg| leg.ws == Some(conn)) {
        event(&account, |leg, s, now| frame(leg, s, text, now));
    }
}

pub fn ws_closed(conn: u32) {
    if let Some(account) = account_of(|leg| leg.ws == Some(conn)) {
        event(&account, |leg, _, now| {
            leg.ws = None;
            leg.trouter.lost(now);
            leg.captions.down(now);
        });
    }
}

pub fn rtc_data(conn: u32, messages: &[Vec<u8>], dropped: u32) {
    if let Some(account) = account_of(|leg| leg.rtc == Some(conn)) {
        event(&account, |leg, s, now| {
            if let Some(reason) = leg.captions.data(messages, dropped, now) {
                end(leg, s, reason, now);
            }
        });
    }
}

pub fn rtc_state(conn: u32, state: &str) {
    if let Some(account) = account_of(|leg| leg.rtc == Some(conn)) {
        event(&account, |leg, s, now| match state {
            "connected" => leg.captions.channel_open(now),
            "failed" | "closed" => {
                leg.rtc = None;
                end(leg, s, Reason::MediaFailed, now);
            }
            _ => {}
        });
    }
}

/// A session timer: Trouter's reconnections, pings and renewals, the session's deadlines and the
/// account's `leave` retries.
pub fn timer_fired(id: u32) -> bool {
    let Some(account) = id.checked_sub(TIMER_BASE).and_then(|at| SLOTS.with(|slots| slots.borrow().get(usize::try_from(at).ok()?).cloned())) else {
        return false;
    };
    let retry_due = RETRIES.with(|r| r.borrow().get(&account).copied());
    let now = api::now_ms();
    if let Some((next, until)) = retry_due
        && now >= next
    {
        let waiting = api::with_session(&account, |s| Ok(leave_records(s, now))).unwrap_or(false);
        RETRIES.with(|r| {
            let mut r = r.borrow_mut();
            if waiting && now < until {
                r.insert(account.clone(), (now.saturating_add(captions::LEAVE_RETRY_MS), until));
            } else {
                r.remove(&account);
            }
        });
    }
    if LEGS.with(|legs| legs.borrow().contains_key(&account)) {
        event(&account, tick);
    } else {
        schedule(&account, now);
    }
    true
}

fn tick(leg: &mut Leg, s: &mut Session, now: i64) {
    while leg.captions.active() {
        match leg.trouter.tick(now) {
            Some(Tick::Connect) => match websocket::open(&leg.trouter.url(&api::uuid()), &[]) {
                Ok(conn) => leg.ws = Some(conn),
                Err(_) => {
                    leg.trouter.lost(now);
                    leg.captions.down(now);
                }
            },
            Some(Tick::Send(frame)) => send(leg, &frame, now),
            Some(Tick::Register) => register(leg, s, now),
            Some(Tick::Failed) => end(leg, s, Reason::Protocol, now),
            None => break,
        }
    }
    match leg.captions.due(now) {
        Some(Due::End(reason)) => end(leg, s, reason, now),
        Some(Due::Roster) => roster(leg, s, now),
        Some(Due::Forget) | None => {}
    }
}

fn send(leg: &mut Leg, frame: &str, now: i64) {
    if let Some(conn) = leg.ws
        && websocket::send(conn, frame).is_err()
    {
        websocket::close(conn);
        leg.ws = None;
        leg.trouter.lost(now);
        leg.captions.down(now);
    }
}

/// The registration with the first connection's `surl` (spec 5.3), with the IC3 token.
fn register(leg: &mut Leg, s: &mut Session, now: i64) {
    let Some(request) = leg.trouter.registration() else { return };
    let ok = matches!(api::send(s, Service::Chat, &request, false), Ok(r) if (200..=299).contains(&r.status));
    leg.registered |= ok;
    leg.trouter.registered(ok, now);
}

/// One Trouter frame (spec 5.3).
fn frame(leg: &mut Leg, s: &mut Session, text: &str, now: i64) {
    if !leg.captions.active() {
        return;
    }
    let (reply, incoming) = match leg.trouter.handle(text, now) {
        Ok(handled) => handled,
        Err(_) => return end(leg, s, Reason::Protocol, now),
    };
    if let Some(reply) = reply {
        send(leg, &reply, now);
    }
    match incoming {
        Incoming::Open => match api::chat_token(s) {
            Ok(token) => {
                let frame = leg.trouter.authenticate(&token);
                send(leg, frame.expose(), now);
            }
            Err(_) => end(leg, s, Reason::Protocol, now),
        },
        Incoming::Connected { first: true, .. } => {
            register(leg, s, now);
            call(leg, s, now);
        }
        Incoming::Connected { same_surl: false, .. } => end(leg, s, Reason::Protocol, now),
        Incoming::Connected { .. } => {
            leg.captions.up();
            roster(leg, s, now);
        }
        Incoming::Callback(callback) => match captions::callback(&callback.url, &callback.body, &leg.callbacks) {
            Ok(Call::Acceptance { answer, acknowledgement }) => accept(leg, s, &answer, acknowledgement.as_deref(), now),
            Ok(Call::Roster) => roster(leg, s, now),
            Ok(Call::Ended) => end(leg, s, Reason::MeetingEnded, now),
            Ok(Call::Other) => {}
            Err(_) => end(leg, s, Reason::Protocol, now),
        },
        Incoming::Lost => {
            leg.captions.pause(now);
            roster(leg, s, now);
        }
        Incoming::Closing => {
            if let Some(conn) = leg.ws.take() {
                websocket::close(conn);
            }
            leg.trouter.lost(now);
            leg.captions.down(now);
        }
        Incoming::Nothing => {}
    }
}

/// The join (spec 4.1, docs "Live results" 18): the media connection's offer in MS-SDP, the
/// session record, then `POST <controller>`.
fn call(leg: &mut Leg, s: &mut Session, now: i64) {
    let (conn, offer) = match rtc::open(LABEL) {
        Ok(opened) => opened,
        Err(HttpError::Blocked(why)) if why.starts_with("at most") => return end(leg, s, Reason::ConnectionLimit, now),
        Err(_) => return end(leg, s, Reason::MediaFailed, now),
    };
    leg.rtc = Some(conn);
    let offer = match sdp::to_ms(&offer, u32::try_from(api::random() >> 33).unwrap_or(1) | 1) {
        Ok(offer) => offer,
        Err(_) => return end(leg, s, Reason::Protocol, now),
    };
    let Some(surl) = leg.trouter.surl().map(str::to_string) else { return end(leg, s, Reason::Protocol, now) };
    let mut all = records(&leg.account);
    all.push(Record { controller: leg.live.controller.clone(), endpoint: leg.endpoint.clone(), participant: leg.participant.clone(), at_ms: now });
    store_records(&leg.account, &all);
    let mri = s.me();
    let links = Links { surl: &surl, session: &leg.callbacks, tag: leg.tag };
    let body = captions::join(&me(s, leg, &mri), &links, &leg.live, &leg.captions.conversation, &offer, &leg.media_leg);
    leg.joined = true;
    let controller = leg.live.controller.clone();
    match service(s, leg, "POST", &controller, Some(&body), true) {
        Ok(r) if (200..=299).contains(&r.status) => match captions::conversation(&r.body, &mri, &leg.endpoint) {
            Ok((link, roster)) => {
                leg.leave = Some(link);
                if let Some(roster) = roster
                    && let Some(reason) = leg.captions.roster(&roster, now)
                {
                    end(leg, s, reason, now);
                }
            }
            Err(_) => end(leg, s, Reason::Protocol, now),
        },
        // Refused: the service did not take it.
        Ok(_) => {
            leg.joined = false;
            end(leg, s, Reason::Refused, now);
        }
        Err(_) => end(leg, s, Reason::Protocol, now),
    }
}

/// The SDP answer of `/call/acceptance/`: to the host in standard SDP, then the acknowledgement.
fn accept(leg: &mut Leg, s: &mut Session, answer: &str, acknowledgement: Option<&str>, now: i64) {
    let Some(conn) = leg.rtc else { return };
    let plain = match sdp::from_ms(answer) {
        Ok(plain) => plain,
        Err(_) => return end(leg, s, Reason::Protocol, now),
    };
    match rtc::accept(conn, &plain) {
        Ok(()) => leg.captions.answered(),
        Err(HttpError::Blocked(why)) if why.starts_with("proxy:") => return end(leg, s, Reason::Proxy, now),
        Err(_) => return end(leg, s, Reason::MediaFailed, now),
    }
    if let Some(url) = acknowledgement
        && service(s, leg, "POST", url, Some(&captions::acknowledgement()), false).is_err()
    {
        host::log(1, "the call acceptance was not acknowledged");
    }
}

/// Reads the roster (`GET <controller>`): whether the user is still in the meeting and captions
/// are on (spec 4.1, 5.3).
fn roster(leg: &mut Leg, s: &mut Session, now: i64) {
    let controller = leg.live.controller.clone();
    let read = service(s, leg, "GET", &controller, None, false).ok().filter(|r| (200..=299).contains(&r.status));
    let mri = s.me();
    match read.map(|r| captions::conversation(&r.body, &mri, &leg.endpoint)) {
        Some(Ok((link, roster))) => {
            leg.leave.get_or_insert(link);
            if let Some(roster) = roster
                && let Some(reason) = leg.captions.roster(&roster, now)
            {
                return end(leg, s, reason, now);
            }
        }
        Some(Err(_)) | None => host::log(1, "the meeting's roster could not be read"),
    }
    leg.captions.roster_polled(now);
}
