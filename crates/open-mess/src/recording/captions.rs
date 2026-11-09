//! The archive of Teams calls from their live captions (spec 2026-11-04-captions-archive, 4.1-4.7).
//! While the window's switch is on, the archiver lists the calls of every account whose module
//! offers the captions tools (the Teams module's `list_calls`, `join_captions`, `read_captions` and
//! `leave_captions`) every 20 s, joins each call the user is in as the core, reads its captions
//! every 5 s into a recording of source `captions` and leaves it when the owner stops it. It calls
//! the module the way an approved action does (pending.rs `module_tool`), under the switch's
//! standing consent instead of an approval, and trusts the module's answers no more than an
//! agent's call does. Logs carry states, counts and reasons only: never text, names or threads.
//!
//! Everything runs under one lock, a pass at a time: turning the switch off, "Zatrzymaj" and the
//! last read before the modules' sessions end wait for a join running now, and so leave no call
//! joined behind them (4.1, R7).
// ponytail: one lock for every call of every account; a stop waits for the pass in progress (at
// most a few module calls). Per-call locks if passes ever grow long.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, SystemTime};

use chrono::DateTime;
use open_mess_host::limits::CALL_TIMEOUT;
use open_mess_host::{HostError, ModuleError, ModuleHandle};
use serde_json::{Value, json};

use super::archive::{self, Call, CallState, Cue, GapReason, Gaps};
use super::{MAX_TRANSCRIPTS_TOTAL, MIN_FREE_DISK, Meta, Reason, Recordings, Source, State, millis};
use crate::accounts::{Account, Accounts};
use crate::tools::{Gate, extra};
use crate::update::probation;

/// The window's switch, the language captions are turned on in, and when the switch was last
/// turned on (4.1, 4.4).
pub const AUTO: &str = "captions.auto";
pub const LANGUAGE: &str = "captions.language";
const SINCE: &str = "captions.auto_since";
pub const DEFAULT_LANGUAGE: &str = "pl-pl";
/// How often the calls are listed and the captions read.
pub const LIST_EVERY: Duration = Duration::from_secs(20);
pub const PASS_EVERY: Duration = Duration::from_secs(5);
/// A join that failed is tried again this much later (4.4).
const RETRY_MS: u64 = 2 * 60 * 1000;
const MAX_ATTEMPTS: u32 = 15;
const MAX_REFUSED: u32 = 3;
/// One recording's longest time and most cues; the call goes on in a new one (4.4).
const LONGEST_MS: u64 = 12 * 60 * 60 * 1000;
const MAX_CUES: u64 = 200_000;
/// The calls one list takes (4.2).
const MAX_LISTED: usize = 20;
/// A page of captions, and the most pages of one session in a pass.
const READ_BYTES: u64 = 262_144;
const MAX_PAGES: usize = 50;
/// The module's tools, without its name.
const LIST: &str = "list_calls";
const JOIN: &str = "join_captions";
const READ: &str = "read_captions";
const LEAVE: &str = "leave_captions";

/// A captions language as BCP 47 writes it: `pl-pl`, `en-us`.
pub fn valid_language(code: &str) -> bool {
    let mut parts = code.split('-');
    parts.next().is_some_and(|first| (2..=3).contains(&first.len()) && first.bytes().all(|b| b.is_ascii_lowercase()))
        && parts.all(|part| (2..=8).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
        && code.len() <= 16
}

/// What the owner is shown of the archive (4.7).
pub trait Notices: Send + Sync {
    /// A recording started: "open_mess zapisuje napisy tej rozmowy", whose button stops it.
    fn started(&self, id: &str);
    /// The notices Windows refused, for `status`.
    fn failed(&self) -> usize {
        0
    }
}

/// Shows in the tray how many calls are kept.
pub type Indicator = Box<dyn Fn(usize) + Send + Sync>;

/// An account id and a `call` of `list_calls`.
type Key = (String, String);

/// A recording kept now.
struct Rec {
    id: String,
    meta: Meta,
    gaps: Gaps,
}

impl Rec {
    /// `ms` from the recording's start.
    fn at(&self, ms: u64) -> u64 {
        ms.saturating_sub(self.meta.start_ms)
    }
}

/// The module's session of a call, as the archiver reads it.
#[derive(Default)]
struct Session {
    token: Option<String>,
    cursor: u64,
    /// `missed` of the last page; None before the first.
    missed: Option<u64>,
}

/// A call the archiver knows of, in memory.
struct Live {
    conversation: String,
    joined_at: Option<u64>,
    /// When it was first listed, the start of its recording without `joined_at` (4.4).
    first_seen: u64,
    /// In the last list of its account; None before one after a restart.
    listed: Option<bool>,
    next_join: u64,
    /// Why the last join failed, for a recording `failed` when the call goes unjoined.
    last_reason: Option<Reason>,
    session: Option<Session>,
    rec: Option<Rec>,
    /// Its recording was left by a daemon that stopped: it ends "interrupted" if the call is gone.
    resumed: bool,
}

impl Live {
    fn new(conversation: String, now: u64) -> Live {
        Live {
            conversation,
            joined_at: None,
            first_seen: now,
            listed: Some(true),
            next_join: 0,
            last_reason: None,
            session: None,
            rec: None,
            resumed: false,
        }
    }
}

#[derive(Default)]
struct Inner {
    /// The calls' state was read from the disk, and the recordings left were dealt with.
    loaded: bool,
    /// `captions-calls.json`, by account and call.
    calls: BTreeMap<Key, Call>,
    changed: bool,
    /// The calls' state could not be read at the start: calls listed then are never joined (4.4).
    lost: bool,
    listed_once: HashSet<String>,
    skip: HashSet<Key>,
    live: HashMap<Key, Live>,
    last_list: Option<u64>,
}

impl Inner {
    fn state(&self, key: &Key) -> Option<CallState> {
        self.calls.get(key).map(|call| call.state)
    }

    /// The call's state, made `active` when it has none.
    fn entry(&mut self, key: &Key, now: u64) -> &mut Call {
        self.changed = true;
        self.calls.entry(key.clone()).or_insert_with(|| Call {
            account: key.0.clone(),
            call: key.1.clone(),
            recording: None,
            state: CallState::Active,
            seen_ms: now,
            captions_seen: false,
            attempts: 0,
        })
    }
}

/// How a join or a session ended (4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// The call ended, or the user left it.
    Over,
    /// The user is not in the call (now).
    NotInMeeting,
    /// That call is no longer running.
    Gone,
    /// open_mess's own limit of sessions: tried again without a limit.
    Limit,
    /// E2EE, lobby, policy: never tried again.
    Permanent(Reason),
    /// Tried again every 2 minutes, `refused` 3 times and the rest 15.
    Transient(Reason),
    /// Ended from outside (an update, a time limit, idle, an agent's leave): joined again at once.
    Rejoin,
}

fn outcome(code: &str) -> Outcome {
    match code {
        "meeting_ended" | "user_left" => Outcome::Over,
        "user_not_in_meeting" => Outcome::NotInMeeting,
        "connection_limit" => Outcome::Limit,
        "e2ee" => Outcome::Permanent(Reason::E2ee),
        "lobby" => Outcome::Permanent(Reason::Lobby),
        "policy" => Outcome::Permanent(Reason::Policy),
        "media_failed" => Outcome::Transient(Reason::MediaFailed),
        "proxy" => Outcome::Transient(Reason::Proxy),
        "protocol" => Outcome::Transient(Reason::Protocol),
        "refused" => Outcome::Transient(Reason::Refused),
        _ => Outcome::Rejoin,
    }
}

/// A failed join: the module's error starts with its reason (`connection_limit: ...`).
fn join_outcome(err: &HostError) -> Outcome {
    match err {
        HostError::Module(ModuleError::Platform(why)) if why.contains("no longer running") => Outcome::Gone,
        HostError::Module(ModuleError::Platform(why)) => {
            let code = why.split(|c: char| c == ':' || c.is_whitespace()).next().unwrap_or_default();
            match outcome(code) {
                Outcome::Rejoin | Outcome::Over => Outcome::Transient(Reason::ModuleFailed),
                known => known,
            }
        }
        _ => Outcome::Transient(Reason::ModuleFailed),
    }
}

/// A call of `list_calls`, checked (4.2).
struct Listed {
    call: String,
    conversation: String,
    joined_at: Option<u64>,
}

/// A conversation as a captions tool takes it: a chat thread, or `call:<id>` for a call without one.
fn valid_conversation(conversation: &str) -> bool {
    match conversation.strip_prefix("call:") {
        Some(call) => archive::valid_call(call),
        None => valid_thread(conversation),
    }
}

fn valid_thread(thread: &str) -> bool {
    thread.starts_with("19:") && thread.len() <= 256 && thread.bytes().all(|b| b.is_ascii_graphic())
}

/// RFC 3339 as milliseconds since 1970, never later than `now`.
fn time(text: &str, now: u64) -> Option<u64> {
    let ms = DateTime::parse_from_rfc3339(text).ok()?.timestamp_millis();
    u64::try_from(ms).ok().map(|ms| ms.min(now))
}

fn parse_list(value: &Value, now: u64) -> Option<Vec<Listed>> {
    let items = value.get("items")?.as_array()?;
    let listed = items.iter().take(MAX_LISTED).filter_map(|item| {
        let call = item.get("call")?.as_str().filter(|call| archive::valid_call(call))?;
        let conversation = item.get("conversation")?.as_str().filter(|conversation| valid_conversation(conversation))?;
        let joined_at = item.get("joined_at").and_then(Value::as_str).and_then(|at| time(at, now));
        Some(Listed { call: call.to_owned(), conversation: conversation.to_owned(), joined_at })
    });
    Some(listed.collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageState {
    Joining,
    Live,
    Ended,
    Failed,
    NotJoined,
}

/// A page of `read_captions`, checked: what is not as the contract says is no page.
struct Page {
    state: PageState,
    session: Option<String>,
    call: Option<String>,
    next_cursor: u64,
    missed: u64,
    skipped: u64,
    captions_on: Option<bool>,
    captions_seen: bool,
    reason: Option<String>,
    threads: Vec<String>,
    /// Milliseconds since 1970, speaker, own, text.
    cues: Vec<(u64, String, bool, String)>,
}

fn parse_page(value: &Value, now: u64) -> Option<Page> {
    let state = match value.get("state")?.as_str()? {
        "joining" => PageState::Joining,
        "live" => PageState::Live,
        "ended" => PageState::Ended,
        "failed" => PageState::Failed,
        "not_joined" => PageState::NotJoined,
        _ => return None,
    };
    let text = |key: &str, max: usize| value.get(key).and_then(Value::as_str).filter(|text| text.len() <= max).map(str::to_owned);
    let number = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    let threads = value.get("threads").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    let cues = value.get("untrusted").and_then(|untrusted| untrusted.get("cues")).and_then(Value::as_array).into_iter().flatten();
    let cues = cues.filter_map(|cue| {
        let at = cue.get("at").and_then(Value::as_str).and_then(|at| time(at, now)).unwrap_or(now);
        let speaker = cue.get("speaker").and_then(Value::as_str).unwrap_or_default();
        let text = cue.get("text").and_then(Value::as_str)?;
        Some((at, speaker.to_owned(), cue.get("own").and_then(Value::as_bool).unwrap_or(false), text.to_owned()))
    });
    Some(Page {
        state,
        session: text("session", 128),
        call: text("call", 64),
        next_cursor: number("next_cursor"),
        missed: number("missed"),
        skipped: number("skipped"),
        captions_on: value.get("captions_on").and_then(Value::as_bool),
        captions_seen: value.get("captions_seen").and_then(Value::as_bool).unwrap_or(false),
        reason: text("reason", 64),
        threads: threads.filter(|thread| valid_thread(thread)).take(16).map(str::to_owned).collect(),
        cues: cues.collect(),
    })
}

/// Calls `tool` of the module for `account` under the call limit; the answer is a JSON object
/// within the limit of a module tool's result (4.4, R8).
async fn call(handle: &ModuleHandle, account: &Account, tool: &str, args: Value) -> Result<Value, HostError> {
    let json =
        tokio::time::timeout(CALL_TIMEOUT, handle.call(&account.module_account, tool, &args.to_string())).await.map_err(|_| HostError::Timeout)??;
    let invalid = || HostError::Module(ModuleError::Incompatible("not a result of the captions tools".into()));
    if json.len() > extra::MAX_RESULT {
        return Err(invalid());
    }
    serde_json::from_str::<Value>(&json).ok().filter(Value::is_object).ok_or_else(invalid)
}

/// Whether the module offers the captions tools.
fn offers(handle: &ModuleHandle) -> bool {
    handle.manifest().is_ok_and(|manifest| [LIST, JOIN, READ, LEAVE].iter().all(|tool| manifest.extra_tools.iter().any(|decl| decl.name == *tool)))
}

/// What the archiver works with besides the recordings.
pub struct Setup {
    pub accounts: Arc<Accounts>,
    /// The tools' gate and the probation of an update: no join while writes are held (4.4, R10).
    pub gate: Gate,
    pub probation: probation::Flag,
    /// The tray is up: nothing is kept without its indicator (4.7, R6).
    pub ui_ready: Arc<AtomicBool>,
    pub notices: Arc<dyn Notices>,
    /// Free bytes on the disk of the recordings.
    pub free_disk: Box<dyn Fn() -> u64 + Send + Sync>,
}

pub struct Archiver {
    recordings: Recordings,
    setup: Setup,
    inner: tokio::sync::Mutex<Inner>,
    /// Recordings kept now, why no call is joined now, and whether the calls' state was lost: for
    /// `status`, the tray and the window without the lock.
    active: AtomicUsize,
    reason: StdMutex<Option<&'static str>>,
    lost: AtomicBool,
    indicator: StdMutex<Option<Indicator>>,
    /// Where a toast's "Zatrzymaj" runs.
    runtime: Option<tokio::runtime::Handle>,
}

/// Milliseconds since 1970, now.
pub fn now_ms() -> u64 {
    millis(SystemTime::now())
}

impl Archiver {
    /// The archiver of the recordings in `recordings`; it registers the last read before the
    /// modules' sessions end. Inside the runtime.
    pub fn new(recordings: Recordings, setup: Setup) -> Arc<Archiver> {
        let archiver = Arc::new(Archiver {
            recordings,
            setup,
            inner: tokio::sync::Mutex::default(),
            active: AtomicUsize::new(0),
            reason: StdMutex::default(),
            lost: AtomicBool::new(false),
            indicator: StdMutex::default(),
            runtime: tokio::runtime::Handle::try_current().ok(),
        });
        let weak = Arc::downgrade(&archiver);
        archiver.setup.accounts.before_end_sessions(Box::new(move |handles| {
            let weak = weak.clone();
            Box::pin(async move {
                if let Some(archiver) = weak.upgrade() {
                    archiver.last_read(&handles).await;
                }
            })
        }));
        archiver
    }

    /// Passes every 5 s, for the daemon's lifetime.
    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(PASS_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.pass(now_ms()).await;
        }
    }

    fn setting(&self, key: &str) -> Option<String> {
        self.setup.accounts.store().get_config(key).ok().flatten()
    }

    /// The switch: off unless the window turned it on (4.1).
    pub fn auto(&self) -> bool {
        self.setting(AUTO).as_deref() == Some("true")
    }

    pub fn language(&self) -> String {
        self.setting(LANGUAGE).filter(|code| valid_language(code)).unwrap_or_else(|| DEFAULT_LANGUAGE.into())
    }

    fn since(&self) -> u64 {
        self.setting(SINCE).and_then(|since| since.parse().ok()).unwrap_or(0)
    }

    /// The window's settings (4.1). Turning the switch on records when; turning it off ends every
    /// call kept, "auto_off", after a join running now.
    pub async fn set(&self, auto: bool, language: &str, now: u64) -> Result<(), String> {
        if !valid_language(language) {
            return Err("unknown language".into());
        }
        let was = self.auto();
        {
            let store = self.setup.accounts.store();
            store.set_config(LANGUAGE, language).map_err(|err| err.to_string())?;
            if auto && !was {
                store.set_config(SINCE, &now.to_string()).map_err(|err| err.to_string())?;
            }
            store.set_config(AUTO, if auto { "true" } else { "false" }).map_err(|err| err.to_string())?;
        }
        if was && !auto {
            let mut inner = self.inner.lock().await;
            self.close_all(&mut inner, now, Reason::AutoOff).await;
            self.save(&mut inner, now);
            self.indicate(&inner);
        }
        Ok(())
    }

    /// The tray's indicator, shown at once as things stand.
    pub fn set_indicator(&self, show: Indicator) {
        show(self.active.load(Ordering::SeqCst));
        *self.indicator.lock().unwrap_or_else(PoisonError::into_inner) = Some(show);
    }

    fn indicate(&self, inner: &Inner) {
        let count = inner.live.values().filter(|live| live.rec.is_some()).count();
        if self.active.swap(count, Ordering::SeqCst) != count
            && let Some(show) = &*self.indicator.lock().unwrap_or_else(PoisonError::into_inner)
        {
            show(count);
        }
    }

    /// The recordings kept now.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// The `captions` object of `status`.
    pub fn status(&self) -> Value {
        json!({
            "auto": self.auto(),
            "active": self.active(),
            "reason": *self.reason.lock().unwrap_or_else(PoisonError::into_inner),
            "state_lost": self.lost.load(Ordering::SeqCst),
            "notices_failed": self.setup.notices.failed(),
        })
    }

    fn set_reason(&self, reason: Option<&'static str>) {
        *self.reason.lock().unwrap_or_else(PoisonError::into_inner) = reason;
    }

    /// Why no call may be joined now (4.4, 4.7): no tray, writes held, or the disk.
    fn held(&self) -> Option<&'static str> {
        if !self.setup.ui_ready.load(Ordering::SeqCst) {
            return Some("no tray icon");
        }
        if self.setup.gate.writes_blocked() || self.setup.probation.get() || self.setup.accounts.module_writes().blocked() {
            return Some("update in progress");
        }
        self.full()
    }

    /// The disk or the text limit, which end the recordings kept and start none (4.4).
    fn full(&self) -> Option<&'static str> {
        if (self.setup.free_disk)() < MIN_FREE_DISK {
            Some("low disk")
        } else if self.recordings.usage().1 >= MAX_TRANSCRIPTS_TOTAL {
            Some("text limit")
        } else {
            None
        }
    }

    /// The account with its running module, when it offers the captions tools and is online.
    fn module_of(&self, id: &str) -> Option<(Account, ModuleHandle)> {
        let account = self.setup.accounts.get(id).ok().flatten().filter(|account| account.state == "online")?;
        let handle = self.setup.accounts.modules().handle(&account.module).filter(offers)?;
        Some((account, handle))
    }

    /// One pass at `now`: the calls listed when due, each call joined, read or ended.
    pub async fn pass(&self, now: u64) {
        let mut inner = self.inner.lock().await;
        self.load(&mut inner, now);
        if self.auto() {
            if inner.last_list.is_none_or(|at| now >= at.saturating_add(LIST_EVERY.as_millis() as u64)) {
                inner.last_list = Some(now);
                self.list(&mut inner, now).await;
            }
            let full = self.full();
            let keys: Vec<Key> = inner.live.keys().cloned().collect();
            for key in keys {
                if let Some(mut live) = inner.live.remove(&key)
                    && self.step(&mut inner, &key, &mut live, now, full).await
                {
                    inner.live.insert(key, live);
                }
            }
        } else {
            // Turned off without the window (another daemon's setting, a test): as the window does.
            self.close_all(&mut inner, now, Reason::AutoOff).await;
        }
        self.save(&mut inner, now);
        self.indicate(&inner);
    }

    /// At the first pass: the calls' state, and the recordings a stopped daemon left (4.4). One
    /// whose call can be joined again waits for it with a `rejoin` gap; any other ends now,
    /// "interrupted", with what was not read lost.
    fn load(&self, inner: &mut Inner, now: u64) {
        if inner.loaded {
            return;
        }
        inner.loaded = true;
        let (calls, lost) = archive::load_calls(self.recordings.dir());
        inner.calls = calls.into_iter().map(|call| ((call.account.clone(), call.call.clone()), call)).collect();
        inner.lost = lost;
        self.lost.store(lost, Ordering::SeqCst);
        let entries: Vec<_> = self.recordings.list().into_iter().filter(|entry| entry.meta.source == Source::Captions).collect();
        if lost {
            // The previous version may lack the last stop: every call stopped in the last day stays so.
            for entry in entries.iter().filter(|entry| entry.meta.reason == Some(Reason::Stopped)) {
                let (Some(account), Some(id)) = (&entry.meta.account, &entry.meta.call) else { continue };
                if entry.meta.end_ms.unwrap_or(entry.meta.start_ms).saturating_add(archive::CALL_TTL_MS) > now {
                    inner.entry(&(account.clone(), id.clone()), now).state = CallState::Blocked;
                }
            }
            inner.changed = true;
        }
        for entry in entries.into_iter().filter(|entry| entry.meta.state == State::Recording) {
            let Some(dir) = self.recordings.path(&entry.id) else { continue };
            let gaps = Gaps::load(&dir, true).map(|(gaps, _)| gaps).unwrap_or_default();
            let mut rec = Rec { id: entry.id, meta: entry.meta, gaps };
            let key = match (&rec.meta.account, &rec.meta.call) {
                (Some(account), Some(call)) => (account.clone(), call.clone()),
                _ => (String::new(), String::new()),
            };
            let read = rec.meta.read_ms.unwrap_or(rec.meta.start_ms);
            let ours =
                inner.calls.get(&key).is_some_and(|call| call.state == CallState::Active && call.recording.as_deref() == Some(rec.id.as_str()));
            if lost || !ours || inner.live.contains_key(&key) {
                self.close(inner, &key, rec, now, Some(Reason::Interrupted), Some(read));
                continue;
            }
            let at = rec.at(read);
            rec.gaps.add(at, None, GapReason::Rejoin);
            let mut live = Live::new(rec.meta.conversation.clone().unwrap_or_default(), now);
            (live.listed, live.joined_at, live.resumed, live.rec) = (None, rec.meta.joined_at, true, Some(rec));
            inner.live.insert(key, live);
        }
    }

    /// Lists the calls of every account (4.2). An account that is gone, signed out or whose module
    /// stopped ends its recordings now, "interrupted" (4.4).
    async fn list(&self, inner: &mut Inner, now: u64) {
        let mut present = HashSet::new();
        for account in self.setup.accounts.list().unwrap_or_default() {
            let Some((account, handle)) = self.module_of(&account.id) else { continue };
            present.insert(account.id.clone());
            // A list that fails changes nothing: the calls are not taken for gone.
            let Some(items) = call(&handle, &account, LIST, json!({})).await.ok().and_then(|page| parse_list(&page, now)) else { continue };
            let first = inner.lost && inner.listed_once.insert(account.id.clone());
            for (_, live) in inner.live.iter_mut().filter(|(key, _)| key.0 == account.id) {
                live.listed = Some(false);
            }
            for item in items {
                let key = (account.id.clone(), item.call);
                // Its state was lost: it may be one the owner stopped (4.4, R7).
                if first {
                    inner.skip.insert(key.clone());
                }
                if let Some(call) = inner.calls.get_mut(&key) {
                    call.seen_ms = now;
                    inner.changed = true;
                }
                let live = inner.live.entry(key).or_insert_with(|| Live::new(item.conversation.clone(), now));
                (live.listed, live.conversation) = (Some(true), item.conversation);
                if item.joined_at.is_some() {
                    live.joined_at = item.joined_at;
                }
            }
        }
        let gone: Vec<Key> = inner.live.keys().filter(|key| !present.contains(&key.0)).cloned().collect();
        for key in gone {
            if let Some(mut live) = inner.live.remove(&key) {
                self.end_presence(inner, &key, &mut live, now).await;
                if let Some(rec) = live.rec.take() {
                    let read = rec.meta.read_ms;
                    self.close(inner, &key, rec, now, Some(Reason::Interrupted), read);
                }
            }
        }
    }

    /// One call in a pass; false forgets it.
    async fn step(&self, inner: &mut Inner, key: &Key, live: &mut Live, now: u64, full: Option<&'static str>) -> bool {
        if let Some(why) = full.filter(|_| live.rec.is_some() || live.session.is_some()) {
            // The disk or the text limit: the call is left and its recording ends (4.4).
            self.end_presence(inner, key, live, now).await;
            if let Some(rec) = live.rec.take() {
                let reason = if why == "low disk" { Reason::LowDisk } else { Reason::TextLimit };
                self.close(inner, key, rec, now, Some(reason), None);
            }
            self.set_reason(Some(why));
            live.next_join = now.saturating_add(RETRY_MS);
            return true;
        }
        if live.session.is_some() {
            self.read(inner, key, live, now).await;
        }
        if let Some(rec) = &mut live.rec {
            // Its coverage keeps up with the time (4.5).
            self.save_rec(rec, now);
        }
        if live.session.is_some() {
            return true;
        }
        if matches!(inner.state(key), Some(CallState::Blocked | CallState::GaveUp)) {
            return live.listed != Some(false);
        }
        if live.listed == Some(false) {
            match live.rec.take() {
                Some(rec) => {
                    let reason = live.resumed.then_some(Reason::Interrupted);
                    self.close(inner, key, rec, now, reason, None);
                }
                // Never joined: the owner sees why (4.4).
                None => {
                    if let Some(reason) = live.last_reason {
                        self.give_up(inner, key, live, reason, now);
                    }
                }
            }
            return false;
        }
        if live.listed == Some(true) && !inner.skip.contains(key) && now >= live.next_join {
            self.join(inner, key, live, now).await;
        }
        true
    }

    /// Joins the call (4.4): under the module write guard of approved actions, as the core, with
    /// the switch checked under the archiver's lock just before.
    async fn join(&self, inner: &mut Inner, key: &Key, live: &mut Live, now: u64) {
        if let Some(why) = self.held() {
            self.set_reason(Some(why));
            return;
        }
        if !self.auto() {
            return;
        }
        let Some((account, _)) = self.module_of(&key.0) else { return };
        // Written before the join: a crash during it never loses the call's state.
        let entry = inner.entry(key, now);
        entry.recording = live.rec.as_ref().map(|rec| rec.id.clone());
        let captions_seen = entry.captions_seen;
        self.save(inner, now);
        let Some(running) = self.setup.accounts.module_writes().start() else { return };
        // Found again under the guard: no swap of the module comes between this and the call.
        let Some(handle) = self.setup.accounts.modules().handle(&account.module).filter(offers) else { return };
        let args = json!({"conversation": live.conversation, "call": key.1, "language": self.language(), "start_captions": !captions_seen});
        let joined = call(&handle, &account, JOIN, args).await;
        drop(running);
        match joined {
            Ok(_) => {
                tracing::info!(target: "open_mess::captions", "joined a call");
                self.set_reason(None);
                live.session = Some(Session::default());
            }
            Err(err) => {
                let outcome = join_outcome(&err);
                tracing::info!(target: "open_mess::captions", outcome = ?outcome, "a join failed");
                self.failed(inner, key, live, outcome, now);
            }
        }
    }

    /// A join that failed, or a session that ended without a recording or before it went on.
    fn failed(&self, inner: &mut Inner, key: &Key, live: &mut Live, outcome: Outcome, now: u64) {
        let retry = now.saturating_add(RETRY_MS);
        match outcome {
            Outcome::Over | Outcome::NotInMeeting | Outcome::Gone => live.next_join = retry,
            Outcome::Rejoin => live.next_join = now,
            Outcome::Limit => (live.last_reason, live.next_join) = (Some(Reason::ConnectionLimit), retry),
            Outcome::Permanent(reason) => self.give_up(inner, key, live, reason, now),
            Outcome::Transient(reason) => {
                let entry = inner.entry(key, now);
                entry.attempts = entry.attempts.saturating_add(1);
                let most = if reason == Reason::Refused { MAX_REFUSED } else { MAX_ATTEMPTS };
                live.last_reason = Some(reason);
                if entry.attempts >= most {
                    self.give_up(inner, key, live, reason, now);
                } else {
                    live.next_join = retry;
                }
            }
        }
    }

    /// The call is not tried again; its recording ends with `reason`, or a `failed` one says why
    /// it was never kept (4.4).
    fn give_up(&self, inner: &mut Inner, key: &Key, live: &mut Live, reason: Reason, now: u64) {
        inner.entry(key, now).state = CallState::GaveUp;
        tracing::info!(target: "open_mess::captions", reason = ?reason, "a call is given up");
        if let Some(rec) = live.rec.take() {
            let read = rec.meta.read_ms;
            self.close(inner, key, rec, now, Some(reason), read);
            return;
        }
        let meta =
            Meta { state: State::Failed, start_ms: self.start_of(live, now), end_ms: Some(now), reason: Some(reason), ..self.meta_of(key, live) };
        // ponytail: a failed recording that cannot be written leaves the call given up unseen.
        let _ = self.recordings.create_with(&meta);
    }

    /// A captions recording's meta for the call.
    fn meta_of(&self, key: &Key, live: &Live) -> Meta {
        let threads = if valid_thread(&live.conversation) { vec![live.conversation.clone()] } else { Vec::new() };
        Meta {
            source: Source::Captions,
            account: Some(key.0.clone()),
            conversation: Some(live.conversation.clone()),
            threads,
            call: Some(key.1.clone()),
            joined_at: live.joined_at,
            coverage: Some(100),
            gaps: Some(0),
            cues: Some(0),
            ..Meta::default()
        }
    }

    /// When the call's recording starts: when the user joined it, else when it was first listed,
    /// never before the switch was turned on (4.4, R10).
    fn start_of(&self, live: &Live, now: u64) -> u64 {
        live.joined_at.unwrap_or(live.first_seen).max(self.since()).min(now)
    }

    /// The recording of the call, at its session's first live state (4.4): its `late_start` gap
    /// and the owner's notice. False when it could not be made.
    fn start_recording(&self, inner: &mut Inner, key: &Key, live: &mut Live, now: u64) -> bool {
        let start = self.start_of(live, now);
        let meta = Meta { state: State::Recording, start_ms: start, read_ms: Some(now), ..self.meta_of(key, live) };
        let Ok(id) = self.recordings.create_with(&meta) else {
            self.set_reason(Some("cannot create the recording"));
            return false;
        };
        let mut rec = Rec { id: id.clone(), meta, gaps: Gaps::default() };
        if now > start {
            rec.gaps.add(0, Some(now - start), GapReason::LateStart);
        }
        self.save_rec(&mut rec, now);
        inner.entry(key, now).recording = Some(id.clone());
        live.rec = Some(rec);
        live.resumed = false;
        tracing::info!(target: "open_mess::captions", "a recording started");
        self.setup.notices.started(&id);
        true
    }

    /// Reads the call's session page by page until one brings nothing new, then deals with its end.
    async fn read(&self, inner: &mut Inner, key: &Key, live: &mut Live, now: u64) {
        let Some((account, handle)) = self.module_of(&key.0) else { return };
        let Some((state, reason)) = self.read_pages(inner, key, live, &account, &handle, now, false).await else { return };
        let outcome = match state {
            PageState::Ended | PageState::Failed => outcome(reason.as_deref().unwrap_or_default()),
            // The module lost the session (reloaded, crashed), or it is another call's.
            PageState::NotJoined => Outcome::Rejoin,
            PageState::Joining | PageState::Live => return,
        };
        live.session = None;
        let Some(rec) = &mut live.rec else {
            self.failed(inner, key, live, outcome, now);
            return;
        };
        match outcome {
            // The user may come back while the call is listed: a new recording then (4.2, R10).
            Outcome::Over => {
                if let Some(rec) = live.rec.take() {
                    self.close(inner, key, rec, now, None, None);
                }
                live.next_join = now.saturating_add(RETRY_MS);
            }
            Outcome::Permanent(reason) => self.give_up(inner, key, live, reason, now),
            _ => {
                // From the last successful read until the next session is live (4.4, 4.5).
                let from = rec.at(rec.meta.read_ms.unwrap_or(now));
                rec.gaps.add(from, None, GapReason::Rejoin);
                self.save_rec(rec, now);
                match outcome {
                    Outcome::Transient(_) | Outcome::Limit => self.failed(inner, key, live, outcome, now),
                    _ => live.next_join = now,
                }
            }
        }
    }

    /// Reads pages of the session into the recording, the first live one starting it; returns the
    /// last page's state and reason, None when a read failed. While `draining` (a stop) nothing
    /// starts and the end is left to the caller.
    #[allow(clippy::too_many_arguments)]
    async fn read_pages(
        &self,
        inner: &mut Inner,
        key: &Key,
        live: &mut Live,
        account: &Account,
        handle: &ModuleHandle,
        now: u64,
        draining: bool,
    ) -> Option<(PageState, Option<String>)> {
        for _ in 0..MAX_PAGES {
            let session = live.session.as_ref()?;
            let mut args = json!({"conversation": live.conversation, "max_bytes": READ_BYTES});
            if let (Some(token), Some(args)) = (&session.token, args.as_object_mut()) {
                args.insert("session".into(), token.clone().into());
                args.insert("cursor".into(), session.cursor.into());
            }
            let page = call(handle, account, READ, args).await.ok().and_then(|page| parse_page(&page, now))?;
            // Only this call's session is ever written to its recording (4.3, R2).
            if page.state == PageState::NotJoined || page.call.as_deref().is_some_and(|call| call != key.1) || page.session.is_none() {
                return Some((PageState::NotJoined, None));
            }
            let first = session.missed.is_none();
            if page.captions_seen {
                inner.entry(key, now).captions_seen = true;
            }
            if page.state == PageState::Joining {
                return Some((page.state, page.reason));
            }
            if live.rec.is_none() && page.state == PageState::Live && !draining && !self.start_recording(inner, key, live, now) {
                return None;
            }
            let session = live.session.get_or_insert_with(Session::default);
            let lost = !first && (page.skipped > 0 || session.missed.is_some_and(|missed| page.missed > missed));
            (session.token, session.cursor, session.missed) = (page.session.clone(), page.next_cursor, Some(page.missed));
            if let Some(rec) = &mut live.rec {
                // A call a person joined has the new chat's thread from then on (4.2).
                if valid_thread(&live.conversation) && !rec.meta.threads.contains(&live.conversation) {
                    rec.meta.threads.push(live.conversation.clone());
                }
                self.write_page(rec, &page, lost, now);
                if rec.meta.cues.unwrap_or(0) >= MAX_CUES || now.saturating_sub(rec.meta.start_ms) >= LONGEST_MS {
                    self.continue_in_new(inner, key, live, now);
                }
            }
            if page.cues.is_empty() {
                return Some((page.state, page.reason));
            }
        }
        None
    }

    /// A page into the recording: its cues word for word (none from before the start), and the
    /// gaps it shows (4.4, 4.5).
    fn write_page(&self, rec: &mut Rec, page: &Page, lost: bool, now: u64) {
        let from = rec.at(rec.meta.read_ms.unwrap_or(now));
        let cues: Vec<Cue> = page
            .cues
            .iter()
            .filter(|(at, ..)| *at >= rec.meta.start_ms)
            .map(|(at, speaker, own, text)| Cue::capped(rec.at(*at), speaker, *own, text))
            .collect();
        if let Some(dir) = self.recordings.path(&rec.id) {
            // ponytail: cues that cannot be written are lost without a gap; the next read tries the disk again.
            if archive::append_cues(&dir, &cues).is_ok() {
                rec.meta.cues = Some(rec.meta.cues.unwrap_or(0).saturating_add(cues.len() as u64));
            }
        }
        if page.state == PageState::Live {
            rec.gaps.end(Some(GapReason::Rejoin), rec.at(now));
        }
        if lost {
            rec.gaps.add(from, Some(rec.at(now)), GapReason::Lost);
        }
        if page.state == PageState::Live && page.captions_on == Some(false) {
            rec.gaps.add(from, Some(rec.at(now)), GapReason::CaptionsOff);
        }
        for thread in &page.threads {
            if !rec.meta.threads.contains(thread) {
                rec.meta.threads.push(thread.clone());
            }
        }
        rec.meta.read_ms = Some(now);
        self.save_rec(rec, now);
    }

    /// 12 hours or 200 000 cues: the recording ends "too long" and the call goes on in a new one,
    /// the same session (4.4).
    fn continue_in_new(&self, inner: &mut Inner, key: &Key, live: &mut Live, now: u64) {
        let Some(old) = live.rec.take() else { return };
        let meta = Meta {
            state: State::Recording,
            start_ms: now,
            end_ms: None,
            reason: None,
            coverage: Some(100),
            gaps: Some(0),
            cues: Some(0),
            read_ms: Some(now),
            continues: Some(old.id.clone()),
            ..old.meta.clone()
        };
        self.close(inner, key, old, now, Some(Reason::TooLong), None);
        if let Ok(id) = self.recordings.create_with(&meta) {
            inner.entry(key, now).recording = Some(id.clone());
            live.rec = Some(Rec { id, meta, gaps: Gaps::default() });
        }
    }

    /// The recording's meta with its coverage now, and its gaps.
    fn save_rec(&self, rec: &mut Rec, now: u64) {
        let end = rec.meta.end_ms.unwrap_or(now);
        rec.meta.coverage = Some(rec.gaps.coverage(end.saturating_sub(rec.meta.start_ms)));
        rec.meta.gaps = Some(u32::try_from(rec.gaps.0.len()).unwrap_or(u32::MAX));
        if let Some(dir) = self.recordings.path(&rec.id) {
            // ponytail: a meta.json or gaps.jsonl that cannot be written now is written at the next read.
            let _ = rec.gaps.save(&dir);
            let _ = self.recordings.set_meta(&rec.id, &rec.meta);
        }
    }

    /// Ends a recording `done` with `reason`; what was not read since `lost_from` is a `lost` gap
    /// to its end (4.4).
    fn close(&self, inner: &mut Inner, key: &Key, mut rec: Rec, now: u64, reason: Option<Reason>, lost_from: Option<u64>) {
        let end = rec.at(now);
        if let Some(from) = lost_from {
            rec.gaps.add(rec.at(from), Some(end), GapReason::Lost);
        }
        rec.gaps.end(None, end);
        (rec.meta.state, rec.meta.end_ms, rec.meta.reason) = (State::Done, Some(now.max(rec.meta.start_ms)), reason);
        self.save_rec(&mut rec, now);
        if let Some(call) = inner.calls.get_mut(key).filter(|call| call.recording.as_deref() == Some(rec.id.as_str())) {
            call.recording = None;
            inner.changed = true;
        }
        tracing::info!(target: "open_mess::captions", reason = ?reason, cues = rec.meta.cues.unwrap_or(0), "a recording ended");
    }

    /// Ends open_mess's presence in the call: a last read, the leave, the rest of the buffer read
    /// (4.7). Nothing starts meanwhile.
    async fn end_presence(&self, inner: &mut Inner, key: &Key, live: &mut Live, now: u64) {
        if live.session.is_none() {
            return;
        }
        if let Some((account, handle)) = self.module_of(&key.0) {
            let _ = self.read_pages(inner, key, live, &account, &handle, now, true).await;
            let _ = call(&handle, &account, LEAVE, json!({"conversation": live.conversation})).await;
            let rest = self.read_pages(inner, key, live, &account, &handle, now, true).await;
            if rest.is_none()
                && let Some(rec) = &mut live.rec
            {
                // The rest of the buffer could not be read: lost from the last read (4.4).
                let from = rec.at(rec.meta.read_ms.unwrap_or(now));
                rec.gaps.add(from, Some(rec.at(now)), GapReason::Lost);
            }
        }
        live.session = None;
    }

    /// Every call kept or joined ends with `reason` and is forgotten, not blocked (4.1).
    async fn close_all(&self, inner: &mut Inner, now: u64, reason: Reason) {
        let keys: Vec<Key> = inner.live.keys().cloned().collect();
        for key in keys {
            let Some(mut live) = inner.live.remove(&key) else { continue };
            self.end_presence(inner, &key, &mut live, now).await;
            if let Some(rec) = live.rec.take() {
                self.close(inner, &key, rec, now, Some(reason), None);
            }
        }
    }

    /// Stops the recordings going on that `which` picks (4.7): each call is blocked first, then read,
    /// left and read to its end, and its recording ends "stopped". Returns their ids.
    pub async fn stop(&self, which: impl Fn(&str, &Meta) -> bool) -> Vec<String> {
        let now = now_ms();
        let mut inner = self.inner.lock().await;
        let keys: Vec<Key> = inner
            .live
            .iter()
            .filter(|(_, live)| live.rec.as_ref().is_some_and(|rec| which(&rec.id, &rec.meta)))
            .map(|(key, _)| key.clone())
            .collect();
        let mut stopped = Vec::new();
        for key in keys {
            let Some(mut live) = inner.live.remove(&key) else { continue };
            // Blocked and written first: a crash from here on never brings the call back (4.4, R3).
            inner.entry(&key, now).state = CallState::Blocked;
            self.save(&mut inner, now);
            self.end_presence(&mut inner, &key, &mut live, now).await;
            if let Some(rec) = live.rec.take() {
                stopped.push(rec.id.clone());
                self.close(&mut inner, &key, rec, now, Some(Reason::Stopped), None);
            }
            // Kept while listed, so it is not taken for a new call.
            inner.live.insert(key, live);
        }
        self.save(&mut inner, now);
        self.indicate(&inner);
        stopped
    }

    /// "Zatrzymaj" of a toast: in the background, the toast's thread never waits.
    pub fn stop_soon(self: &Arc<Self>, id: String) {
        let archiver = Arc::clone(self);
        if let Some(runtime) = &self.runtime {
            runtime.spawn(async move {
                archiver.stop(|recording, _| recording == id).await;
            });
        }
    }

    /// "Zatrzymaj wszystkie" of the tray: in the background, as [`Archiver::stop_soon`].
    pub fn stop_all_soon(self: &Arc<Self>) {
        let archiver = Arc::clone(self);
        if let Some(runtime) = &self.runtime {
            runtime.spawn(async move {
                archiver.stop(|_, _| true).await;
            });
        }
    }

    /// Deletes a recording from the window; one going on is stopped first, its call blocked (4.7).
    pub async fn delete(&self, id: &str) -> std::io::Result<()> {
        self.stop(|recording, _| recording == id).await;
        self.recordings.delete(id)
    }

    /// The last read of every session before the modules in `handles` end their sessions (an
    /// update, a scan of the modules, the daemon's stop): what comes after it until the leave is
    /// lost, and counted from it (4.4).
    pub async fn last_read(&self, handles: &[ModuleHandle]) {
        let now = now_ms();
        let mut inner = self.inner.lock().await;
        let keys: Vec<Key> = inner.live.iter().filter(|(_, live)| live.session.is_some()).map(|(key, _)| key.clone()).collect();
        for key in keys {
            let Some(account) = self.setup.accounts.get(&key.0).ok().flatten() else { continue };
            let module = handles.iter().find(|handle| handle.manifest().is_ok_and(|manifest| manifest.name == account.module));
            let Some(mut live) = inner.live.remove(&key) else { continue };
            if let Some(handle) = module {
                let _ = self.read_pages(&mut inner, &key, &mut live, &account, handle, now, true).await;
            }
            inner.live.insert(key, live);
        }
        self.save(&mut inner, now);
    }

    /// Writes the calls' state when it changed.
    fn save(&self, inner: &mut Inner, now: u64) {
        if !inner.changed {
            return;
        }
        let mut calls: Vec<Call> = inner.calls.values().cloned().collect();
        // ponytail: a state that cannot be written is tried again at the next change.
        if archive::save_calls(self.recordings.dir(), &mut calls, now).is_ok() {
            inner.calls = calls.into_iter().map(|call| ((call.account.clone(), call.call.clone()), call)).collect();
            inner.changed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_are_bcp_47() {
        for good in ["pl-pl", "en-us", "de", "zh-hans-cn"] {
            assert!(valid_language(good), "{good}");
        }
        for bad in ["", "PL-pl", "p", "pl_pl", "pl-", "pl-p", "../x", "abcd-ef", "pl-plplplplpl"] {
            assert!(!valid_language(bad), "{bad}");
        }
    }

    #[test]
    fn join_errors_are_told_apart_by_their_reason() {
        let platform = |why: &str| HostError::Module(ModuleError::Platform(why.into()));
        assert_eq!(join_outcome(&platform("connection_limit: at most 4")), Outcome::Limit);
        assert_eq!(join_outcome(&platform("that call is no longer running")), Outcome::Gone);
        assert_eq!(join_outcome(&platform("user_not_in_meeting")), Outcome::NotInMeeting);
        assert_eq!(join_outcome(&platform("e2ee: end-to-end encrypted")), Outcome::Permanent(Reason::E2ee));
        assert_eq!(join_outcome(&platform("refused")), Outcome::Transient(Reason::Refused));
        assert_eq!(join_outcome(&platform("something else")), Outcome::Transient(Reason::ModuleFailed));
        assert_eq!(join_outcome(&HostError::Timeout), Outcome::Transient(Reason::ModuleFailed));
    }

    #[test]
    fn hostile_lists_and_pages_are_cut_down_never_a_panic() {
        let now = 1_700_000_000_000;
        let mut items: Vec<Value> = vec![
            json!({"call": "../x", "conversation": "19:a@thread.v2"}),
            json!({"call": "ok", "conversation": "not a thread"}),
            json!({"call": "ok2", "conversation": "call:bad id"}),
            json!({"call": "ok3", "conversation": "19:a@thread.v2", "joined_at": "yesterday"}),
            json!({"call": "ok4", "conversation": "call:ok4", "joined_at": "2999-01-01T00:00:00.1234567Z"}),
            json!({"call": 5}),
            json!("text"),
        ];
        items.extend((0..100).map(|n| json!({"call": format!("c{n}"), "conversation": "19:b@thread.v2"})));
        let listed = parse_list(&json!({ "items": items }), now).unwrap();
        assert_eq!(listed.iter().map(|item| item.call.as_str()).take(2).collect::<Vec<_>>(), ["ok3", "ok4"]);
        assert!(listed.len() <= MAX_LISTED);
        assert_eq!((listed[0].joined_at, listed[1].joined_at), (None, Some(now)));
        assert!(parse_list(&json!({"items": "x"}), now).is_none());

        assert!(parse_page(&json!({"state": "exploded"}), now).is_none());
        let page = parse_page(
            &json!({
                "state": "live", "session": "s".repeat(500), "next_cursor": -1, "threads": ["19:a@thread.v2", "bad", 7],
                "untrusted": {"cues": [{"at": "x", "text": "t"}, {"speaker": "no text"}, {"at": "2020-01-01T00:00:00Z", "speaker": "A", "own": true, "text": "y"}]},
            }),
            now,
        )
        .unwrap();
        assert_eq!((page.session, page.next_cursor, page.threads.len(), page.cues.len()), (None, 0, 1, 2));
        assert_eq!(page.cues[0].0, now);
    }
}
