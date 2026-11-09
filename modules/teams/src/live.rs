//! Live-captions sessions in the module instance (spec 2026-10-29-live-captions, 4 and 5.3, and
//! 2026-11-04-captions-archive, 4.2-4.3): the tools, the Trouter sockets, the media connections,
//! the timers and the session records, around the pure `captions`, `trouter` and `sdp`.

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::{Value, json};

use crate::api::{self, Service};
use crate::captions::{self, Call, Due, Links, Live, Me, Reason, Record, Resource, Session as Captions};
use crate::component::bindings::open_mess::module::http::{HttpError, Response};
use crate::component::bindings::open_mess::module::types::{ModuleError, Participant, Target};
use crate::component::bindings::open_mess::module::{host, rtc, storage, websocket};
use crate::meetings::language_tag;
use crate::sdp;
use crate::session::Session;
use crate::trouter::{Incoming, Tick, Trouter};

/// Sessions' timers: one per account, from this id on (the poll's is 1).
const TIMER_BASE: u32 = 100;
/// At most this many sessions at once in the module, as the host's media connections (spec 5.2),
/// and per account, each in another call (archive spec 4.3).
const MAX_SESSIONS: usize = 4;
const MAX_ACCOUNT_SESSIONS: usize = 3;
/// Ended sessions kept for reading their cues (each for 15 minutes); past this many the one that
/// ended first goes.
const MAX_ENDED: usize = 8;
/// The captions language when the caller gives none (archive spec 4.3).
const LANGUAGE: &str = "pl-pl";
const LABEL: &str = "main-channel";
const GONE: &str = "that call is no longer running";
const CHANGED: &str = "the captions session changed, read again without session and cursor";
/// Calls whose captions were on, for every account (archive spec 4.3).
const SEEN_KEY: &str = "captions-seen";

/// One session and the connections it holds.
struct Leg {
    account: String,
    captions: Captions,
    trouter: Trouter,
    ws: Option<u32>,
    rtc: Option<u32>,
    live: Live,
    /// Where the conversation adds the captions bot (docs 10.8).
    add: Option<String>,
    add_with_chat: Option<String>,
    /// The captions language if open_mess turns captions on.
    language: String,
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
    /// The sessions, live and ended, by their token.
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

/// Whether the records were stored.
fn store_records(account: &str, records: &[Record]) -> bool {
    let key = records_key(account);
    if records.is_empty() {
        storage::delete(&key);
        return true;
    }
    let stored = storage::set(&key, &captions::encode_records(records)).is_ok();
    if !stored {
        host::log(1, "a live-captions session record was not stored");
    }
    stored
}

/// open_mess's own endpoint ids for the account (spec 5.3): one per session the account can have
/// at once, made once, kept in storage, never the official client's. Each session is a device of
/// its own, with its own Trouter socket and registration.
fn endpoint(account: &str, slot: usize) -> String {
    let key = if slot == 0 { format!("captions-endpoint:{account}") } else { format!("captions-endpoint:{account}:{slot}") };
    if let Some(id) = storage::get(&key).and_then(|bytes| String::from_utf8(bytes).ok()).filter(|id| id.len() == 36) {
        return id;
    }
    let id = api::uuid();
    if storage::set(&key, id.as_bytes()).is_err() {
        host::log(1, "the live-captions endpoint id was not stored");
    }
    id
}

/// Whether the account saw captions on in `call` lately (archive spec 4.3).
fn seen(account: &str, call: &str) -> bool {
    captions::seen(storage::get(SEEN_KEY).as_deref(), account, call, api::now_ms())
}

/// Notes at once that captions were on in the call: a crash before anyone reads the session does
/// not undo a participant's choice to turn them off later (archive spec 4.3, R7).
fn note_seen(account: &str, call: &str, now: i64) {
    let list = captions::note_seen(storage::get(SEEN_KEY).as_deref(), account, call, now);
    if storage::set(SEEN_KEY, &list).is_err() {
        host::log(1, "a call with captions on was not stored");
    }
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

/// The tokens of the sessions `find` picks.
fn tokens(find: impl Fn(&Leg) -> bool) -> Vec<String> {
    LEGS.with(|legs| legs.borrow().iter().filter(|(_, leg)| find(leg)).map(|(token, _)| token.clone()).collect())
}

fn token_of(find: impl Fn(&Leg) -> bool) -> Option<String> {
    tokens(find).into_iter().next()
}

fn take(token: &str) -> Option<Leg> {
    LEGS.with(|legs| legs.borrow_mut().remove(token))
}

/// Puts a session back, or forgets it once its cues are gone, and sets its account's timer. Past
/// [`MAX_ENDED`] ended sessions, the one that ended first goes.
fn put(leg: Leg, now: i64) {
    let account = leg.account.clone();
    if !leg.captions.gone(now) {
        LEGS.with(|legs| {
            let mut legs = legs.borrow_mut();
            legs.insert(leg.captions.token.clone(), leg);
            let ended: Vec<(i64, String)> = legs.iter().filter_map(|(token, leg)| leg.captions.ended().map(|at| (at, token.clone()))).collect();
            if ended.len() > MAX_ENDED
                && let Some((_, first)) = ended.into_iter().min()
            {
                legs.remove(&first);
            }
        });
    }
    schedule(&account, now);
}

/// The account's timer, for the earliest thing due in its sessions and its `leave` retries.
fn schedule(account: &str, now: i64) {
    let legs = LEGS.with(|legs| {
        legs.borrow()
            .values()
            .filter(|leg| leg.account == account)
            .flat_map(|leg| {
                let trouter = if leg.captions.active() { leg.trouter.next() } else { None };
                [trouter, leg.captions.next_at()]
            })
            .flatten()
            .min()
    });
    let retry = RETRIES.with(|r| r.borrow().get(account).map(|(next, _)| *next));
    if let Some(at) = [legs, retry].into_iter().flatten().min() {
        let after = u32::try_from(at.saturating_sub(now).clamp(50, i64::from(u32::MAX))).unwrap_or(u32::MAX);
        host::set_timer(timer(account), after);
    }
}

/// Runs `f` on a session with its account's session, outside a tool call (an event; the caller
/// has started the call's budget).
fn event(token: &str, f: impl FnOnce(&mut Leg, &mut Session, i64)) {
    let Some(mut leg) = take(token) else { return };
    let now = api::now_ms();
    let account = leg.account.clone();
    let done = api::with_session(&account, |s| {
        f(&mut leg, s, now);
        Ok(())
    });
    if done.is_err() {
        // No session to send anything with: the connections go, and the record stays for the
        // retries, a new sign-in or `restore` (GLM review of 7385fdd).
        close(&mut leg);
        if leg.captions.active() && leg.joined {
            retry_from(&account, now);
        }
        leg.captions.end(Reason::Protocol, now);
    }
    put(leg, now);
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// A conversation of `teams_list_calls`: a chat thread or `call:<id>` (archive spec 4.3).
fn conversation_arg(args: &Value) -> Result<&str, ModuleError> {
    text(args, "conversation")
        .filter(|c| captions::is_thread(c) || c.strip_prefix("call:").is_some_and(captions::is_call))
        .ok_or_else(|| ModuleError::InvalidArgs("conversation must be a 19: thread id or call:<id> from teams_list_calls".to_string()))
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

/// The user's calls in the call store.
fn calls(s: &mut Session) -> Result<Vec<Live>, ModuleError> {
    let me = s.me();
    let body = api::ok(api::conversation(s, &captions::call_store(&me), false)?)?;
    let (calls, skipped) = captions::calls(&body, &me).map_err(api::proto)?;
    if skipped > 0 {
        host::log(1, &format!("live captions: {skipped} items of the call store were left out"));
    }
    Ok(calls)
}

/// A read of the call's conversation, without joining it (archive spec 4.2); a call that is gone
/// is [`GONE`].
fn read_call(s: &mut Session, live: &Live) -> Result<Vec<u8>, ModuleError> {
    let request = captions::service("GET", &live.controller, None, &api::uuid(), &api::uuid(), region(s));
    let read = api::conversation(s, &request, false)?;
    if matches!(read.status, 404 | 410) {
        return Err(ModuleError::Platform(GONE.to_string()));
    }
    api::ok(read)
}

/// `teams_list_calls` (archive spec 4.2): the user's calls from the call store, each named by its
/// chat thread, the 1:1 chat with the other person, or `call:<id>`. Nobody else's text in it.
pub fn list_calls(s: &mut Session, _args: &Value) -> Result<Value, ModuleError> {
    let mut items = Vec::new();
    for live in calls(s)?.into_iter().take(captions::MAX_CALLS) {
        let conversation = match (&live.chat, &live.other) {
            (Some((thread, _)), _) => Some(thread.clone()),
            // A 1:1 call: its chat, in whichever order of ids it has. A lookup that fails names the
            // call by its id this time; the call, not its name, is what stays (archive spec 4.2).
            (None, Some(other)) if api::has_time() => match crate::tools::one_to_one(s, other) {
                Ok(chat) => chat.map(|c| c.id).filter(|id| captions::is_thread(id)),
                Err(e @ ModuleError::AuthRequired(_)) => return Err(e),
                Err(_) => None,
            },
            _ => None,
        };
        let conversation = conversation.unwrap_or_else(|| format!("call:{}", live.call));
        // `joined_at` waits for the meaning of `userJoinTime` (archive spec 4.2, step 0b).
        items.push(json!({"conversation": conversation, "call": live.call, "joined_at": null}));
    }
    Ok(json!({ "items": items }))
}

/// `teams_join_captions`, once the user approved it (spec 4.1, archive spec 4.2-4.3): the checks,
/// the session and the Trouter socket; the join itself follows in events.
pub fn join(s: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let conversation = conversation_arg(args)?.to_string();
    let call = text(args, "call")
        .filter(|c| captions::is_call(c))
        .ok_or_else(|| ModuleError::InvalidArgs("call must be a call id from teams_list_calls".to_string()))?
        .to_string();
    let language = match args.get("language") {
        None | Some(Value::Null) => LANGUAGE.to_string(),
        Some(v) => v
            .as_str()
            .filter(|tag| language_tag(tag))
            .ok_or_else(|| ModuleError::InvalidArgs("language must be a BCP 47 tag such as pl-pl".to_string()))?
            .to_ascii_lowercase(),
    };
    let start_captions = match args.get("start_captions") {
        None | Some(Value::Null) => true,
        Some(v) => v.as_bool().ok_or_else(|| ModuleError::InvalidArgs("start_captions must be true or false".to_string()))?,
    };
    let account = s.account.clone();
    let now = api::now_ms();
    let want = start_captions && !seen(&account, &call);
    // Only this call, and only under a name of its own: its chat, its 1:1 chat or `call:<id>` -
    // a session never takes a thread it does not have (archive spec 4.2-4.3).
    let mut live =
        calls(s)?.into_iter().find(|c| c.call == call && c.named(&conversation, &account)).ok_or_else(|| ModuleError::Platform(GONE.to_string()))?;
    // The call's session goes on: no second join (archive spec 4.3).
    if let Some(token) = token_of(|leg| leg.account == account && leg.captions.active() && leg.captions.call == call)
        && let Some(mut leg) = take(&token)
    {
        if !leg.captions.thread(&conversation) {
            end(&mut leg, s, Reason::Protocol, now);
        } else if want {
            leg.captions.want_captions();
        }
        let state = leg.captions.state().name();
        put(leg, now);
        return Ok(json!({"state": state}));
    }
    // Another call's session in this conversation is over: it ends first (archive spec 4.3).
    for token in tokens(|leg| leg.account == account && leg.captions.active() && leg.captions.named(&conversation)) {
        if let Some(mut leg) = take(&token) {
            end(&mut leg, s, Reason::MeetingEnded, now);
            put(leg, now);
        }
    }
    let active = |leg: &Leg| leg.captions.active();
    let (mine, all) = (tokens(|leg| active(leg) && leg.account == account), tokens(active).len());
    if mine.len() >= MAX_ACCOUNT_SESSIONS || all >= MAX_SESSIONS {
        return Err(ModuleError::Platform(format!(
            "connection_limit: at most {MAX_ACCOUNT_SESSIONS} live-captions sessions an account and {MAX_SESSIONS} at once"
        )));
    }
    let used: Vec<String> = LEGS.with(|legs| mine.iter().filter_map(|token| legs.borrow().get(token).map(|leg| leg.endpoint.clone())).collect());
    let endpoint = (0..MAX_ACCOUNT_SESSIONS).map(|slot| endpoint(&account, slot)).find(|e| !used.contains(e)).unwrap_or_else(api::uuid);
    // The roster before joining: nothing reaches the call unless the user is in it on another
    // device and the call allows captions (archive spec 4.2).
    let resource = captions::resource(&read_call(s, &live)?, &s.me(), &endpoint).map_err(api::proto)?;
    if resource.allow_transcription == Some(false) {
        return Err(ModuleError::Platform("policy: this call does not allow captions".to_string()));
    }
    if !resource.roster.as_ref().is_some_and(|roster| roster.user_elsewhere) {
        return Err(ModuleError::Platform(
            "user_not_in_meeting: Teams does not show you in this call on another device; join it in Teams first".to_string(),
        ));
    }
    let token = format!("{:016x}{:016x}", api::random(), api::random());
    let mut session = Captions::new(&conversation, &call, &token, &s.me(), now);
    if let Some(chat) = resource.chat {
        live.chat = Some(chat);
    }
    if let Some((thread, _)) = &live.chat
        && !session.thread(thread)
    {
        return Err(ModuleError::Incompatible("unexpected server response at activeModalities.groupChat".to_string()));
    }
    if want {
        session.want_captions();
    }
    let mut trouter = Trouter::new(&endpoint, &api::uuid());
    let ws = websocket::open(&trouter.url(&api::uuid()), &[])
        .map_err(|e| ModuleError::Platform(format!("Teams could not be reached: {}", describe(&e))))?;
    let leg = Leg {
        account,
        captions: session,
        trouter,
        ws: Some(ws),
        rtc: None,
        live,
        add: resource.add,
        add_with_chat: resource.add_with_chat,
        language,
        endpoint,
        participant: api::uuid(),
        callbacks: api::uuid(),
        tag: u32::try_from(api::random() >> 32).unwrap_or(0),
        chain: api::uuid(),
        media_leg: api::uuid().replace('-', ""),
        // The join's answer names the `leave` link of open_mess's leg.
        leave: None,
        joined: false,
        registered: false,
    };
    put(leg, now);
    Ok(json!({"state": "joining"}))
}

/// The account's sessions named `conversation`, the newest first.
fn named(account: &str, conversation: &str) -> Vec<String> {
    LEGS.with(|legs| {
        let legs = legs.borrow();
        let mut found: Vec<(&String, &Leg)> = legs.iter().filter(|(_, leg)| leg.account == account && leg.captions.named(conversation)).collect();
        found.sort_by_key(|(_, leg)| std::cmp::Reverse(leg.captions.started()));
        found.into_iter().map(|(token, _)| token.clone()).collect()
    })
}

/// `teams_read_captions` (spec 4.2, archive spec 4.3): the newest session of the conversation, or
/// the one of `session` - an ended session stays readable by its token after a new one started.
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
    let found = named(&s.account, conversation);
    let token = match session {
        _ if found.is_empty() => None,
        Some(token) => Some(found.into_iter().find(|t| t == token).ok_or_else(|| ModuleError::InvalidArgs(CHANGED.to_string()))?),
        None => found.into_iter().next(),
    };
    let now = api::now_ms();
    let read = token.and_then(|token| {
        LEGS.with(|legs| {
            legs.borrow_mut().get_mut(&token).map(|leg| leg.captions.read(session, cursor, usize::try_from(max_bytes).unwrap_or(65536), now))
        })
    });
    match read {
        Some(Ok(page)) => {
            schedule(&s.account, now);
            Ok(page)
        }
        Some(Err(_)) => Err(ModuleError::InvalidArgs(CHANGED.to_string())),
        None => Ok(json!({
            "state": "not_joined", "session": null, "call": null, "captions_on": null, "captions_seen": false, "next_cursor": 0, "missed": 0,
            "skipped": 0, "language": null, "reason": null, "untrusted": {"cues": []},
        })),
    }
}

/// `teams_leave_captions` (spec 4.3): leaves the call of this conversation's session.
pub fn leave(s: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let conversation = conversation_arg(args)?;
    let now = api::now_ms();
    let found = named(&s.account, conversation);
    let active = LEGS.with(|legs| found.iter().find(|t| legs.borrow().get(*t).is_some_and(|leg| leg.captions.active())).cloned());
    let Some(mut leg) = active.or_else(|| found.into_iter().next()).and_then(|token| take(&token)) else {
        return Ok(json!({"state": "not_joined"}));
    };
    end(&mut leg, s, Reason::Left, now);
    let state = leg.captions.state().name();
    put(leg, now);
    Ok(json!({"state": state}))
}

/// The approval's target of a call without a chat, `call:<id>` (archive spec 4.3): from the
/// call's conversation, while the call store has it.
pub fn describe_call(s: &mut Session, call: &str) -> Result<Target, ModuleError> {
    let live = calls(s)?.into_iter().find(|c| c.call == call).ok_or_else(|| ModuleError::Platform(GONE.to_string()))?;
    let (title, people) = captions::call_target(&read_call(s, &live)?).map_err(api::proto)?;
    let participants = people.into_iter().map(|(id, display_name)| Participant { id, display_name }).collect();
    Ok(Target { conversation_title: title, participants, target_text: None })
}

/// Ends the session: the connections go, the registration is removed, and the call is left
/// (spec 4.3). A `leave` that fails is tried again later from the record.
fn end(leg: &mut Leg, s: &mut Session, reason: Reason, now: i64) {
    if !leg.captions.active() {
        return;
    }
    host::log(2, &format!("live captions: the session ends ({})", reason.name()));
    leg.captions.end(reason, now);
    close(leg);
    // `leave` first: what others see goes before what only Trouter knows (Codex review of 7385fdd).
    let record = Record { controller: leg.live.controller.clone(), endpoint: leg.endpoint.clone(), participant: leg.participant.clone(), at_ms: now };
    if !leg.joined || leave_one(s, &record, leg.leave.as_deref()) {
        forget(&leg.account, &leg.participant);
    } else {
        retry_from(&leg.account, now);
    }
    if leg.registered && api::has_time() && api::send(s, Service::Chat, &leg.trouter.unregistration(), false).is_err() {
        host::log(1, "the live-captions registration was not removed; it lapses by itself");
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
            Ok(r) if (200..=299).contains(&r.status) => match captions::resource(&r.body, &mri, &record.endpoint) {
                Ok(Resource { leave: Some(link), .. }) => link,
                _ => return false,
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
    let held: Vec<String> = LEGS.with(|legs| {
        legs.borrow().values().filter(|leg| leg.account == s.account && leg.captions.active()).map(|leg| leg.participant.clone()).collect()
    });
    let (mut kept, mut waiting) = (Vec::new(), false);
    for record in records(&s.account) {
        let stale = now.saturating_sub(record.at_ms) > captions::STALE_RECORD_MS;
        if held.contains(&record.participant) {
            kept.push(record);
        } else if !api::has_time() || (!leave_one(s, &record, None) && !stale) {
            waiting = true;
            kept.push(record);
        }
    }
    store_records(&s.account, &kept);
    waiting
}

/// After `restore` (daemon start, or a new instance after a crash): the calls of legs that were
/// never left are left now (spec 4.3).
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

/// Before the daemon stops, an update or a module scan: the account's sessions end and leave
/// (spec 4.3).
pub fn end_sessions(account: &str) {
    for token in tokens(|leg| leg.account == account) {
        event(&token, |leg, s, now| end(leg, s, Reason::Update, now));
    }
}

/// Before the account goes: its sessions end and leave, and every leg still on record gets one
/// more try (spec 4.3), whether a session holds it or not (Codex review of 7385fdd).
pub fn sign_out(account: &str) {
    for token in tokens(|leg| leg.account == account) {
        event(&token, |leg, s, now| end(leg, s, Reason::Left, now));
    }
    if !records(account).is_empty() {
        let now = api::now_ms();
        if api::with_session(account, |s| Ok(leave_records(s, now))).is_err() {
            host::log(1, "the meetings of live-captions sessions were not left before sign-out");
        }
    }
    RETRIES.with(|r| r.borrow_mut().remove(account));
    LEGS.with(|legs| legs.borrow_mut().retain(|_, leg| leg.account != account));
}

pub fn ws_message(conn: u32, text: &str) {
    if let Some(token) = token_of(|leg| leg.ws == Some(conn)) {
        event(&token, |leg, s, now| frame(leg, s, text, now));
    }
}

pub fn ws_closed(conn: u32) {
    if let Some(token) = token_of(|leg| leg.ws == Some(conn)) {
        event(&token, |leg, _, now| {
            leg.ws = None;
            leg.trouter.lost(now);
            leg.captions.down(now);
        });
    }
}

pub fn rtc_data(conn: u32, messages: &[Vec<u8>], dropped: u32) {
    if let Some(token) = token_of(|leg| leg.rtc == Some(conn)) {
        event(&token, |leg, s, now| {
            if let Some(reason) = leg.captions.data(messages, dropped, now) {
                end(leg, s, reason, now);
            }
        });
    }
}

pub fn rtc_state(conn: u32, state: &str) {
    if let Some(token) = token_of(|leg| leg.rtc == Some(conn)) {
        event(&token, |leg, s, now| match state {
            "connected" => {
                host::log(2, "live captions: the data channel is open");
                leg.captions.channel_open(now);
            }
            "failed" | "closed" => {
                leg.rtc = None;
                end(leg, s, Reason::MediaFailed, now);
            }
            _ => {}
        });
    }
}

/// An account's timer: Trouter's reconnections, pings and renewals, the sessions' deadlines and
/// the account's `leave` retries.
pub fn timer_fired(id: u32) -> bool {
    let Some(account) = id.checked_sub(TIMER_BASE).and_then(|at| SLOTS.with(|slots| slots.borrow().get(usize::try_from(at).ok()?).cloned())) else {
        return false;
    };
    let retry_due = RETRIES.with(|r| r.borrow().get(&account).copied());
    let now = api::now_ms();
    if let Some((next, until)) = retry_due
        && now >= next
    {
        // Without a session (WAM gives no token now) the records wait, and the retries go on.
        let waiting = api::with_session(&account, |s| Ok(leave_records(s, now))).unwrap_or(true);
        RETRIES.with(|r| {
            let mut r = r.borrow_mut();
            if waiting && now < until {
                r.insert(account.clone(), (now.saturating_add(captions::LEAVE_RETRY_MS), until));
            } else {
                r.remove(&account);
            }
        });
    }
    for token in tokens(|leg| leg.account == account) {
        event(&token, tick);
    }
    schedule(&account, now);
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
        Some(Due::AddBot) => add_bot(leg, s),
        Some(Due::Start(url)) => start(leg, s, &url, now),
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
    let status = api::send(s, Service::Chat, &request, false).map(|r| r.status);
    host::log(2, &format!("live captions: registration HTTP {}", status.as_ref().map_or_else(|_| "failed".to_string(), u16::to_string)));
    let ok = matches!(status, Ok(200..=299));
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
        Incoming::Connected { same_surl: false, .. } => end(leg, s, Reason::Protocol, now),
        // Up again, also when the first socket fell before its `trouter.connected` (Codex review of
        // 263a526).
        Incoming::Connected { first, .. } => {
            leg.captions.up();
            if first {
                register(leg, s, now);
                call(leg, s, now);
            } else {
                roster(leg, s, now);
            }
        }
        Incoming::Callback(callback) => match captions::callback(&callback.url, &callback.body, &leg.callbacks) {
            Ok(Call::Acceptance { answer, acknowledgement }) => accept(leg, s, &answer, acknowledgement.as_deref(), now),
            Ok(Call::Lobby) => end(leg, s, Reason::Lobby, now),
            Ok(Call::Roster) => roster(leg, s, now),
            Ok(Call::Ended { meeting, code, sub_code }) => {
                let what = if meeting { "meeting" } else { "call" };
                let shown = |c: Option<i64>| c.map_or_else(|| "none".to_string(), |c| c.to_string());
                host::log(1, &format!("live captions: Teams ended the {what} (code {}, sub-code {})", shown(code), shown(sub_code)));
                end(leg, s, if meeting { Reason::MeetingEnded } else { captions::refusal(code, sub_code) }, now);
            }
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
    // No join without its record: a crash could not leave it then (Codex review of 7385fdd).
    let mut all = records(&leg.account);
    all.push(Record { controller: leg.live.controller.clone(), endpoint: leg.endpoint.clone(), participant: leg.participant.clone(), at_ms: now });
    if !store_records(&leg.account, &all) {
        return end(leg, s, Reason::Protocol, now);
    }
    let mri = s.me();
    let links = Links { surl: &surl, session: &leg.callbacks, tag: leg.tag };
    let body = captions::join(&me(s, leg, &mri), &links, &leg.live, &offer, &leg.media_leg);
    leg.joined = true;
    let controller = leg.live.controller.clone();
    let joined = service(s, leg, "POST", &controller, Some(&body), true);
    host::log(2, &format!("live captions: join HTTP {}", joined.as_ref().map_or_else(|_| "failed".to_string(), |r| r.status.to_string())));
    match joined {
        Ok(r) if (200..=299).contains(&r.status) => match captions::resource(&r.body, &mri, &leg.endpoint) {
            Ok(resource) if resource.leave.is_some() => learn(leg, s, resource, now),
            _ => end(leg, s, Reason::Protocol, now),
        },
        // Refused: the service did not take it.
        Ok(r) => {
            leg.joined = false;
            end(leg, s, captions::refused(r.status, &r.body), now);
        }
        Err(_) => end(leg, s, Reason::Protocol, now),
    }
}

/// What a read of the conversation tells the session (archive spec 4.2-4.3): its links, its chat
/// thread - a 1:1 call that people are added to gets one - and the roster.
fn learn(leg: &mut Leg, s: &mut Session, resource: Resource, now: i64) {
    if leg.leave.is_none() {
        leg.leave = resource.leave;
    }
    if resource.add.is_some() {
        leg.add = resource.add;
    }
    if resource.add_with_chat.is_some() {
        leg.add_with_chat = resource.add_with_chat;
    }
    if let Some(chat) = resource.chat {
        if !leg.captions.thread(&chat.0) {
            return end(leg, s, Reason::Protocol, now);
        }
        leg.live.chat = Some(chat);
    }
    if let Some(roster) = resource.roster {
        host::log(2, &format!("live captions: roster: the user elsewhere {}, captions {}", roster.user_elsewhere, roster.captions));
        let seen = leg.captions.captions_seen();
        let ended = leg.captions.roster(&roster, now);
        if !seen && leg.captions.captions_seen() {
            note_seen(&leg.account, &leg.captions.call, now);
        }
        if let Some(reason) = ended {
            end(leg, s, reason, now);
        }
    }
}

/// Turns captions on by adding the captions bot (archive spec 4.3, docs 10.8): through
/// `addParticipantAndModality` when the call has a chat thread, else `addParticipant`, as the
/// official client does. A try that fails is the session's to repeat.
fn add_bot(leg: &mut Leg, s: &mut Session) {
    let link = if leg.live.chat.is_some() { leg.add_with_chat.clone() } else { leg.add.clone() };
    let (Some(link), Some(surl)) = (link, leg.trouter.surl().map(str::to_string)) else {
        return host::log(1, "live captions: the call gave no link to add the captions bot");
    };
    let Ok(token) = api::recording_token(s) else { return host::log(1, "live captions: Windows gave no token to turn captions on") };
    let mri = s.me();
    let links = Links { surl: &surl, session: &leg.callbacks, tag: leg.tag };
    let body = captions::add_bot(&me(s, leg, &mri), &links, &leg.live, &leg.language, &token);
    let added = service(s, leg, "POST", &link, Some(&body), true);
    host::log(2, &format!("live captions: adding the captions bot HTTP {}", added.map_or_else(|_| "failed".to_string(), |r| r.status.to_string())));
}

/// Turns captions on in the bot that is in the call: `start` on its command URL (archive spec 4.3).
fn start(leg: &mut Leg, s: &mut Session, url: &str, now: i64) {
    let request = captions::start(url, &s.me(), &leg.participant, &leg.live.call, &api::uuid(), now);
    let started = api::bot_command(s, &request);
    host::log(2, &format!("live captions: starting captions HTTP {}", started.map_or_else(|_| "failed".to_string(), |r| r.status.to_string())));
}

/// The SDP answer of `/call/acceptance/`: to the host in standard SDP, then the acknowledgement.
fn accept(leg: &mut Leg, s: &mut Session, answer: &str, acknowledgement: Option<&str>, now: i64) {
    let Some(conn) = leg.rtc else { return };
    let plain = match sdp::from_ms(answer) {
        Ok(plain) => plain,
        Err(e) => {
            host::log(1, &format!("live captions: the SDP answer was not taken: {e:?}"));
            return end(leg, s, Reason::Protocol, now);
        }
    };
    match rtc::accept(conn, &plain) {
        Ok(()) => {
            host::log(2, "live captions: the SDP answer was accepted");
            leg.captions.answered();
        }
        Err(HttpError::Blocked(why)) if why.starts_with("proxy:") => return end(leg, s, Reason::Proxy, now),
        Err(e) => {
            host::log(1, &format!("live captions: the media connection refused the answer: {}", describe(&e)));
            return end(leg, s, Reason::MediaFailed, now);
        }
    }
    if let Some(url) = acknowledgement
        && service(s, leg, "POST", url, Some(&captions::acknowledgement()), false).is_err()
    {
        host::log(1, "the call acceptance was not acknowledged");
    }
}

/// Reads the roster (`GET <controller>`): whether the user is still in the call, captions are on
/// and the call has a new chat thread (spec 4.1, 5.3; archive spec 4.2).
fn roster(leg: &mut Leg, s: &mut Session, now: i64) {
    let controller = leg.live.controller.clone();
    let read = service(s, leg, "GET", &controller, None, false).ok().filter(|r| (200..=299).contains(&r.status));
    let mri = s.me();
    match read.map(|r| captions::resource(&r.body, &mri, &leg.endpoint)) {
        Some(Ok(resource)) => {
            learn(leg, s, resource, now);
            if !leg.captions.active() {
                return;
            }
        }
        Some(Err(_)) | None => host::log(1, "the meeting's roster could not be read"),
    }
    leg.captions.roster_polled(now);
}
