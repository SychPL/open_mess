//! Live captions of a Teams call (spec 2026-10-29-live-captions, 4 and 5.3, and
//! 2026-11-04-captions-archive, 4.2-4.3; docs/protocols/teams.md 10.7, 10.8 and "Live results"
//! 11-20): a session's state and deadlines, the caption messages of the call's data channel, the
//! buffer agents read, turning captions on, and the requests and answers of the call store and the
//! conversation service. No network and no clock: the glue (`live.rs`) does the I/O and passes the
//! time in.

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};
use url::Url;

use crate::http::HttpRequest;
use crate::meetings::{cut, language_tag};
use crate::{ProtoError, incompatible, parse_json};

/// The host of the call store and of every conversation URL the module sends a token to (spec
/// 4.1, R16).
pub const SERVICE_HOST: &str = "api.flightproxy.teams.microsoft.com";
/// The client the probe named in `X-Microsoft-Skype-Client` and `pstnContent` ("Live results"
/// 14 and 18).
const CLIENT: &str = "SkypeSpaces/1415/26260.1717.5229.7049/os=windows; osVer=NT 10.0; deviceType=computer/TsCallingVersion=2026.37.01.11";
/// The web client's endpoint capabilities without the broker and reinviteless bits, which the
/// join at the call store's URL needs ("Live results" 14 and 18). Bit 128 asks for compressed
/// callbacks, which `trouter.rs` inflates ("Live results" 19).
const ENDPOINT_CAPABILITIES: u64 = 72935;
/// ponytail: the probe's `clientLocation`; the client sends the user's country. Take it from the
/// account if Teams turns out to care (step 5 of the spec).
const CLIENT_LOCATION: &str = "PL";

/// Deadlines of a session (spec 4.1-4.3).
pub const JOIN_MS: i64 = 60_000;
pub const IDLE_MS: i64 = 10 * 60_000;
pub const TIME_LIMIT_MS: i64 = 3 * 3_600_000;
/// How long the cues of an ended session can still be read.
pub const KEEP_MS: i64 = 15 * 60_000;
/// The roster is read this often while Trouter is down (spec 5.3, Codex R24).
pub const ROSTER_POLL_MS: i64 = 10_000;
/// A failed `leave` is sent again this often, for this long (spec 4.3, Codex R21).
pub const LEAVE_RETRY_MS: i64 = 30_000;
pub const LEAVE_RETRIES_FOR_MS: i64 = 10 * 60_000;
/// A session record older than this gets one `leave` and goes, whatever the answer (spec 4.3).
pub const STALE_RECORD_MS: i64 = 4 * 3_600_000;
/// Turning captions on (archive spec 4.3): the first try this long after the data channel opened,
/// a failed one again after [`ENABLE_RETRY_MS`], at most [`ENABLE_TRIES`] tries in all.
pub const ENABLE_AFTER_MS: i64 = 10_000;
pub const ENABLE_RETRY_MS: i64 = 5 * 60_000;
const ENABLE_TRIES: u8 = 4;

/// The combined recording and transcription bot, the one the official client adds for captions
/// with its shipped configuration, and the captions-only bot (docs 10.8, "Live results" 20). The
/// captions bot is told by these MRIs, not by `28:`: a meeting can have another `28:` participant.
pub const COMBINED_BOT: &str = "28:bdd75849-e0a6-4cce-8fc1-d7c0d4da43e5";
const BOTS: [&str; 2] = [COMBINED_BOT, "28:b1902c3e-b9f7-4650-9b23-5772bd429747"];

/// `teams_list_calls` lists at most this many calls (archive spec 4.2).
pub const MAX_CALLS: usize = 20;
/// Chat threads one session can have (a 1:1 call that people are added to); past them it ends.
const MAX_THREADS: usize = 8;
/// Calls whose captions were on, remembered in the module's storage for this long, at most this
/// many (archive spec 4.3).
const SEEN_FOR_MS: i64 = 24 * 3_600_000;
const MAX_SEEN: usize = 200;
/// A call's subject in an approval, and the people listed there.
const MAX_SUBJECT: usize = 200;
const MAX_PEOPLE: usize = 500;

/// The buffer of one session (spec 5.3).
const MAX_CUES: usize = 20_000;
const MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const MAX_SPEAKER: usize = 200;
const MAX_TEXT: usize = 4000;
const MAX_PAGE_CUES: usize = 2000;
/// A message reassembled from fragments is cut off past this; the host passes at most 64 KiB a
/// fragment.
const MAX_MESSAGE: usize = 1024 * 1024;
/// Unreadable messages in a row that end a session (spec 5.3).
const MAX_BAD_IN_A_ROW: u32 = 100;
/// The data id of caption messages on `main-channel` (docs 10.2).
const CAPTIONS_DATA_ID: u8 = 3;
/// Session records kept per account; a record past them is the oldest and goes.
const MAX_RECORDS: usize = 8;

/// Why a session ended (spec 4.2; `lobby`, `e2ee` and `policy` from the codes of docs 10.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    MeetingEnded,
    Update,
    UserLeft,
    UserNotInMeeting,
    Idle,
    TimeLimit,
    Left,
    Refused,
    Lobby,
    E2ee,
    Policy,
    MediaFailed,
    Proxy,
    ConnectionLimit,
    Protocol,
}

impl Reason {
    pub fn name(self) -> &'static str {
        match self {
            Reason::MeetingEnded => "meeting_ended",
            Reason::Update => "update",
            Reason::UserLeft => "user_left",
            Reason::UserNotInMeeting => "user_not_in_meeting",
            Reason::Idle => "idle",
            Reason::TimeLimit => "time_limit",
            Reason::Left => "left",
            Reason::Refused => "refused",
            Reason::Lobby => "lobby",
            Reason::E2ee => "e2ee",
            Reason::Policy => "policy",
            Reason::MediaFailed => "media_failed",
            Reason::Proxy => "proxy",
            Reason::ConnectionLimit => "connection_limit",
            Reason::Protocol => "protocol",
        }
    }

    /// Whether the session failed rather than ended in the normal way.
    fn failed(self) -> bool {
        !matches!(self, Reason::MeetingEnded | Reason::Update | Reason::UserLeft | Reason::Idle | Reason::TimeLimit | Reason::Left)
    }
}

/// Why the service refused the join or ended open_mess's leg, from its code and sub-code (docs
/// 10.8, the client's end-screen and refusal tables): an end-to-end encrypted meeting, the lobby
/// and policy are told apart; anything else is `Refused`.
pub fn refusal(code: Option<i64>, sub_code: Option<i64>) -> Reason {
    match (code, sub_code) {
        (Some(403), Some(4158)) => Reason::E2ee,
        (Some(0), Some(5854 | 5855)) | (Some(413), Some(5807)) | (Some(403), Some(5354)) => Reason::Lobby,
        (Some(403), Some(5448 | 5723 | 5724 | 5810 | 10005 | 10193)) | (Some(412), Some(5204)) => Reason::Policy,
        _ => Reason::Refused,
    }
}

/// The refusal of a join the service answered with `status` and `body` (a `subCode` at the top,
/// if any; whether E2EE comes this way or as the leg's end is open, docs step 0b(11)).
pub fn refused(status: u16, body: &[u8]) -> Reason {
    let sub_code = serde_json::from_slice::<Value>(body).ok().and_then(|v| v.get("subCode").and_then(Value::as_i64));
    refusal(Some(i64::from(status)), sub_code)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Joining,
    Live,
    Ended,
    Failed,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Joining => "joining",
            State::Live => "live",
            State::Ended => "ended",
            State::Failed => "failed",
        }
    }
}

/// What is due in a session ([`Session::due`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Due {
    /// End the session (and leave the meeting).
    End(Reason),
    /// Read the roster again (Trouter is down).
    Roster,
    /// Turn captions on: add the captions bot to the call.
    AddBot,
    /// Turn captions on: `start` to the bot at this command URL.
    Start(String),
    /// The ended session's cues are gone: forget it.
    Forget,
}

/// A refused read ([`Session::read`]).
#[derive(Debug, PartialEq, Eq)]
pub struct SessionChanged;

/// One final caption.
struct Cue {
    at_ms: i64,
    speaker: Option<String>,
    /// The user said it.
    own: bool,
    text: String,
    language: Option<String>,
    /// Its share of the buffer's byte limit.
    bytes: usize,
}

/// The cues of a session, numbered from 0; the oldest go past the limits.
#[derive(Default)]
struct Buffer {
    cues: VecDeque<Cue>,
    /// The number of the oldest cue kept.
    first: u64,
    bytes: usize,
    /// Cues up to this number have been on a page.
    read_to: u64,
    /// Cues lost: dropped by the host or the module, or gone from the buffer unread.
    missed: u64,
}

/// A page of cues ([`Buffer::page`]).
struct Page {
    cues: Vec<Value>,
    next: u64,
    /// The last cue's language.
    language: Option<String>,
    /// Cues between the cursor and the page that were gone from the buffer already.
    skipped: u64,
}

impl Buffer {
    fn next(&self) -> u64 {
        self.first + self.cues.len() as u64
    }

    fn push(&mut self, cue: Cue) {
        while !self.cues.is_empty() && (self.cues.len() >= MAX_CUES || self.bytes + cue.bytes > MAX_BUFFER_BYTES) {
            if let Some(gone) = self.cues.pop_front() {
                self.bytes -= gone.bytes;
                if self.first >= self.read_to {
                    self.missed += 1;
                }
                self.first += 1;
            }
        }
        self.bytes += cue.bytes;
        self.cues.push_back(cue);
    }

    /// The cues from `cursor` (from the oldest kept when it is older) that fit in `max_bytes` of
    /// JSON - at least one, at most [`MAX_PAGE_CUES`].
    fn page(&mut self, cursor: u64, max_bytes: usize) -> Page {
        let start = cursor.clamp(self.first, self.next());
        let skip = usize::try_from(start - self.first).unwrap_or(usize::MAX);
        let (mut cues, mut used, mut language) = (Vec::new(), 0, None);
        for cue in self.cues.iter().skip(skip) {
            let item = json!({"at": crate::time::rfc3339_millis(cue.at_ms), "speaker": cue.speaker, "own": cue.own, "text": cue.text});
            let size = item.to_string().len();
            if !cues.is_empty() && (used + size > max_bytes || cues.len() == MAX_PAGE_CUES) {
                break;
            }
            used += size;
            language = cue.language.clone();
            cues.push(item);
        }
        let next = start + cues.len() as u64;
        self.read_to = self.read_to.max(next);
        Page { cues, next, language, skipped: self.first.saturating_sub(cursor) }
    }
}

/// The data channel's messages (docs 10.7): an 11-byte big-endian header and 4 bytes a recipient,
/// then the payload; a message in several fragments is put back together by sequence number.
#[derive(Default)]
struct Feed {
    /// Data id -> sequence number, fragments still to come and the payload so far.
    pending: HashMap<u8, (u16, u8, Vec<u8>)>,
    bad_in_a_row: u32,
}

struct Fragment<'a> {
    start: bool,
    data_id: u8,
    seq: u16,
    remaining: u8,
    payload: &'a [u8],
}

fn fragment(bytes: &[u8]) -> Option<Fragment<'_>> {
    let (first, second, flags) = (*bytes.first()?, *bytes.get(1)?, *bytes.get(2)?);
    if first >> 4 != 1 {
        return None;
    }
    let header = (usize::from(first & 15) << 8) + usize::from(second);
    let recipients = usize::from(*bytes.get(10)?);
    if header < 11 + 4 * recipients {
        return None;
    }
    Some(Fragment {
        start: flags & 0x80 != 0,
        data_id: flags & 63,
        seq: u16::from_be_bytes([*bytes.get(3)?, *bytes.get(4)?]),
        remaining: *bytes.get(5)?,
        payload: bytes.get(header..)?,
    })
}

impl Feed {
    /// A whole message once its last fragment came: its data id and payload. `Err` for a fragment
    /// that cannot be read or does not follow the one before.
    fn reassemble(&mut self, bytes: &[u8]) -> Result<Option<(u8, Vec<u8>)>, ()> {
        let f = fragment(bytes).ok_or(())?;
        if f.start {
            if f.remaining == 0 {
                return Ok(Some((f.data_id, f.payload.to_vec())));
            }
            self.pending.insert(f.data_id, (f.seq, f.remaining, f.payload.to_vec()));
            return Ok(None);
        }
        let (seq, remaining, mut payload) = self.pending.remove(&f.data_id).ok_or(())?;
        if f.seq != seq.wrapping_add(1) || payload.len() + f.payload.len() > MAX_MESSAGE {
            return Err(());
        }
        payload.extend_from_slice(f.payload);
        if remaining <= 1 {
            return Ok(Some((f.data_id, payload)));
        }
        self.pending.insert(f.data_id, (f.seq, remaining - 1, payload));
        Ok(None)
    }

    /// The final captions of one data channel message, arrived at `now`; `me` is the user's MRI.
    /// `None` when it cannot be read; then [`Feed::broken`] says whether that ends the session.
    fn push(&mut self, bytes: &[u8], now: i64, me: &str) -> Option<Vec<Cue>> {
        let cues = match self.reassemble(bytes) {
            Ok(Some((CAPTIONS_DATA_ID, payload))) => captions(&payload, now, me),
            // Another data id, or a fragment of a message still coming.
            Ok(_) => Some(Vec::new()),
            Err(()) => None,
        };
        self.bad_in_a_row = if cues.is_some() { 0 } else { self.bad_in_a_row + 1 };
        cues
    }

    fn broken(&self) -> bool {
        self.bad_in_a_row >= MAX_BAD_IN_A_ROW
    }
}

/// The final results of a caption message (keys of "Live results" 16): `recognitionResults`, each
/// with `isFinal`, `text`, `displayName`, `spokenLanguage` and `userId`, the speaker's MRI, which
/// tells the user's own (`me`) and goes no further. `None` when it is not that shape.
fn captions(payload: &[u8], now: i64, me: &str) -> Option<Vec<Cue>> {
    let message: Value = serde_json::from_slice(payload).ok()?;
    let results = match message.as_object()?.get("recognitionResults") {
        None => return Some(Vec::new()),
        Some(results) => results.as_array()?,
    };
    let mut cues = Vec::new();
    for result in results {
        let result = result.as_object()?;
        if result.get("isFinal").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let text = cut(result.get("text")?.as_str()?, MAX_TEXT);
        let speaker = result.get("displayName").and_then(Value::as_str).map(|name| cut(name, MAX_SPEAKER));
        let own = result.get("userId").and_then(Value::as_str).is_some_and(|id| id.eq_ignore_ascii_case(me));
        let language = result.get("spokenLanguage").and_then(Value::as_str).filter(|tag| language_tag(tag)).map(str::to_string);
        let bytes = text.len() + speaker.as_ref().map_or(0, String::len) + 64;
        cues.push(Cue { at_ms: now, speaker, own, text, language, bytes });
    }
    Some(cues)
}

/// What a roster says for the session (spec 4.1, 4.2; archive spec 4.2-4.3).
#[derive(Debug, PartialEq, Eq)]
pub struct Roster {
    /// Another device of the user is in the call: the user is in the meeting there.
    pub user_elsewhere: bool,
    /// The captions bot is in the call with `closedCaptions` active.
    pub captions: bool,
    /// The captions bot is in the call.
    pub bot: bool,
    /// The bot's command URL, if it gave one on the service's host.
    pub command: Option<String>,
}

/// Turning captions on in a session (archive spec 4.3).
#[derive(Default)]
struct Enable {
    /// The caller asked for it, and no participant turned captions off in this call before.
    wanted: bool,
    /// Captions were on at some roster read of the session; never false again.
    seen: bool,
    tries: u8,
    /// When the next try is due.
    at: Option<i64>,
    /// This try added the bot; it gets `start` once it is in the call.
    added: bool,
    started: bool,
}

/// One live-captions session of an account (spec 4).
pub struct Session {
    /// The conversation the session was joined with: a chat thread or `call:<id>`.
    pub conversation: String,
    /// The call store's `correlationId` of the call.
    pub call: String,
    /// The `session` agents pass back (base64url, random).
    pub token: String,
    /// The user's MRI: whose cues are `own`.
    me: String,
    /// Every chat thread of the call, in order (archive spec 4.2).
    threads: Vec<String>,
    state: State,
    reason: Option<Reason>,
    started: i64,
    ended: Option<i64>,
    last_read: i64,
    /// The service's SDP answer was accepted.
    answered: bool,
    /// A roster showed the user's other device.
    roster_seen: bool,
    captions_on: Option<bool>,
    /// The captions bot of the last roster, and its command URL.
    bot: bool,
    command: Option<String>,
    enable: Enable,
    /// Since Trouter went down or lost a message: cues are dropped until a roster is read again.
    paused: bool,
    /// Trouter is down: the roster is read every 10 s until it is back (spec 5.3, Codex R24).
    down: bool,
    roster_at: Option<i64>,
    buffer: Buffer,
    feed: Feed,
}

impl Session {
    /// A session joining `call`, named `conversation`; `me` is the user's MRI.
    pub fn new(conversation: &str, call: &str, token: &str, me: &str, now: i64) -> Session {
        Session {
            conversation: conversation.to_string(),
            call: call.to_string(),
            token: token.to_string(),
            me: me.to_string(),
            threads: if is_thread(conversation) { vec![conversation.to_string()] } else { Vec::new() },
            state: State::Joining,
            reason: None,
            started: now,
            ended: None,
            last_read: now,
            answered: false,
            roster_seen: false,
            captions_on: None,
            bot: false,
            command: None,
            enable: Enable::default(),
            paused: false,
            down: false,
            roster_at: None,
            buffer: Buffer::default(),
            feed: Feed::default(),
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn active(&self) -> bool {
        matches!(self.state, State::Joining | State::Live)
    }

    pub fn started(&self) -> i64 {
        self.started
    }

    pub fn ended(&self) -> Option<i64> {
        self.ended
    }

    /// Whether `name` is the session's conversation or one of its threads.
    pub fn named(&self, name: &str) -> bool {
        self.conversation == name || self.threads.iter().any(|t| t == name)
    }

    /// A chat thread of the call, the newest last; anything else is ignored. False when the
    /// session would have more than [`MAX_THREADS`]: it should end.
    pub fn thread(&mut self, thread: &str) -> bool {
        if !is_thread(thread) || self.threads.iter().any(|t| t == thread) {
            return true;
        }
        if self.threads.len() >= MAX_THREADS {
            return false;
        }
        self.threads.push(thread.to_string());
        true
    }

    /// Turning captions on is asked for (archive spec 4.3): tried while captions were not on yet.
    pub fn want_captions(&mut self) {
        self.enable.wanted = true;
    }

    pub fn captions_seen(&self) -> bool {
        self.enable.seen
    }

    /// Ends the session, once; the cues stay readable for [`KEEP_MS`].
    pub fn end(&mut self, reason: Reason, now: i64) {
        if self.active() {
            self.state = if reason.failed() { State::Failed } else { State::Ended };
            self.reason = Some(reason);
            self.ended = Some(now);
        }
    }

    /// The service's SDP answer was accepted.
    pub fn answered(&mut self) {
        self.answered = true;
    }

    /// The data channel opened: the session is live, and captions are turned on in a while if no
    /// one has.
    pub fn channel_open(&mut self, now: i64) {
        if self.state == State::Joining {
            self.state = State::Live;
            self.enable.at = Some(now.saturating_add(ENABLE_AFTER_MS));
        }
    }

    /// Trouter lost a message: the roster may have changed unseen (spec 5.3).
    pub fn pause(&mut self, now: i64) {
        self.paused = true;
        self.captions_on = None;
        self.roster_at.get_or_insert(now.saturating_add(ROSTER_POLL_MS));
    }

    /// Trouter went down: no callback arrives until it is back.
    pub fn down(&mut self, now: i64) {
        self.down = true;
        self.pause(now);
    }

    /// Trouter is back; the next roster read lets captions in again.
    pub fn up(&mut self) {
        self.down = false;
    }

    /// A roster read: the reason to end the session, if the user is not in the meeting. While
    /// Trouter is down it only checks that; captions wait for Trouter (spec 5.3).
    pub fn roster(&mut self, roster: &Roster, now: i64) -> Option<Reason> {
        if !roster.user_elsewhere {
            return Some(if self.roster_seen { Reason::UserLeft } else { Reason::UserNotInMeeting });
        }
        self.roster_seen = true;
        // Whether captions are on counts even while Trouter is down (Codex review of 263a526).
        self.enable.seen |= roster.captions;
        self.bot = roster.bot;
        self.command.clone_from(&roster.command);
        if self.down {
            self.roster_at = Some(now.saturating_add(ROSTER_POLL_MS));
            return None;
        }
        self.paused = false;
        self.roster_at = None;
        self.captions_on = Some(roster.captions);
        None
    }

    /// Data channel messages and how many the host dropped: the reason to end the session when
    /// too many in a row cannot be read.
    pub fn data(&mut self, messages: &[Vec<u8>], dropped: u32, now: i64) -> Option<Reason> {
        self.buffer.missed += u64::from(dropped);
        if self.paused || !self.active() {
            self.buffer.missed += messages.len() as u64;
            return None;
        }
        for message in messages {
            match self.feed.push(message, now, &self.me) {
                Some(cues) => cues.into_iter().for_each(|cue| self.buffer.push(cue)),
                None => self.buffer.missed += 1,
            }
            if self.feed.broken() {
                return Some(Reason::Protocol);
            }
        }
        None
    }

    /// Whether turning captions on is still to do: asked for, live, no gap, and captions not seen
    /// on in this session yet - once they were, a participant who turns them off decides.
    fn enabling(&self) -> bool {
        self.state == State::Live && !self.paused && self.enable.wanted && !self.enable.seen
    }

    /// The bot this session added is in the call without captions: it gets `start`.
    fn start_due(&self) -> Option<&String> {
        (self.enable.added && !self.enable.started && self.bot).then_some(self.command.as_ref()).flatten()
    }

    /// The next step of turning captions on, due at `now` (archive spec 4.3): the bot this
    /// session added gets `start` once it is in the call; a try adds the bot when it is not in the
    /// call, or sends it `start` when it is (only to a command URL on the service's host).
    fn enable_due(&mut self, now: i64) -> Option<Due> {
        if !self.enabling() {
            return None;
        }
        if let Some(url) = self.start_due().cloned() {
            self.enable.started = true;
            return Some(Due::Start(url));
        }
        if self.enable.tries >= ENABLE_TRIES || self.enable.at.is_none_or(|at| now < at) {
            return None;
        }
        self.enable.tries += 1;
        self.enable.at = Some(now.saturating_add(ENABLE_RETRY_MS));
        self.enable.added = !self.bot;
        self.enable.started = self.bot;
        match (self.bot, &self.command) {
            (false, _) => Some(Due::AddBot),
            (true, Some(url)) => Some(Due::Start(url.clone())),
            (true, None) => None,
        }
    }

    /// What is due at `now`, the most urgent first.
    pub fn due(&mut self, now: i64) -> Option<Due> {
        if !self.active() {
            return self.ended.is_some_and(|at| now >= at.saturating_add(KEEP_MS)).then_some(Due::Forget);
        }
        let past = |at: i64, wait: i64| now >= at.saturating_add(wait);
        if past(self.started, TIME_LIMIT_MS) {
            return Some(Due::End(Reason::TimeLimit));
        }
        if past(self.last_read, IDLE_MS) {
            return Some(Due::End(Reason::Idle));
        }
        if self.state == State::Joining && past(self.started, JOIN_MS) {
            return Some(Due::End(if self.answered { Reason::MediaFailed } else { Reason::Protocol }));
        }
        if let Some(due) = self.enable_due(now) {
            return Some(due);
        }
        if self.roster_at.is_some_and(|at| now >= at) {
            return Some(Due::Roster);
        }
        None
    }

    /// Whether an ended session's cues are gone.
    pub fn gone(&self, now: i64) -> bool {
        self.ended.is_some_and(|at| now >= at.saturating_add(KEEP_MS))
    }

    /// A roster read is done (or failed): the next one during a gap.
    pub fn roster_polled(&mut self, now: i64) {
        if self.paused {
            self.roster_at = Some(now.saturating_add(ROSTER_POLL_MS));
        }
    }

    /// When something is due next.
    pub fn next_at(&self) -> Option<i64> {
        if !self.active() {
            return self.ended.map(|at| at.saturating_add(KEEP_MS));
        }
        let mut times = vec![self.started.saturating_add(TIME_LIMIT_MS), self.last_read.saturating_add(IDLE_MS)];
        if self.state == State::Joining {
            times.push(self.started.saturating_add(JOIN_MS));
        }
        if self.enabling() {
            // A `start` waiting is due now: any time past is.
            times.extend(self.start_due().map(|_| self.started));
            times.extend(self.enable.at.filter(|_| self.enable.tries < ENABLE_TRIES));
        }
        times.extend(self.roster_at);
        times.into_iter().min()
    }

    /// The result of `teams_read_captions` (spec 4.2, archive spec 4.3). `session` from the agent
    /// must be this one's; a read counts against the idle limit.
    pub fn read(&mut self, session: Option<&str>, cursor: Option<u64>, max_bytes: usize, now: i64) -> Result<Value, SessionChanged> {
        if session.is_some_and(|s| s != self.token) {
            return Err(SessionChanged);
        }
        let page = self.buffer.page(cursor.unwrap_or(0), max_bytes);
        self.last_read = now;
        let captions_on = if self.active() { self.captions_on } else { None };
        let mut result = json!({
            "state": self.state.name(),
            "session": self.token,
            "call": self.call,
            "captions_on": captions_on,
            "captions_seen": self.enable.seen,
            "next_cursor": page.next,
            "missed": self.buffer.missed,
            "skipped": page.skipped,
            "language": page.language,
            "reason": self.reason.map(Reason::name),
            "untrusted": {"cues": page.cues},
        });
        // The threads, when there is more to them than the conversation joined (archive spec 4.2).
        let more = self.threads.len() > 1 || self.threads.first().is_some_and(|t| *t != self.conversation);
        if more && let Some(map) = result.as_object_mut() {
            map.insert("threads".to_string(), json!(self.threads));
        }
        Ok(result)
    }
}

/// Whether `url` is one the module sends the skype token to: https on [`SERVICE_HOST`], port
/// 443, no user info (spec 4.1, R16).
pub fn on_service(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.username().is_empty()
            && u.password().is_none()
            && u.port_or_known_default() == Some(443)
            && u.host_str() == Some(SERVICE_HOST)
    })
}

/// A chat thread id as the call store and a conversation name it: `19:`, an `@`, at most 256
/// letters, digits and `-_.@:=+` (archive spec 4.2).
pub fn is_thread(id: &str) -> bool {
    id.len() <= 256 && id.starts_with("19:") && id.contains('@') && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.@:=+".contains(&b))
}

/// A call id, `^[A-Za-z0-9-]{1,64}$` (archive spec 4.2).
pub fn is_call(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A call of the user's call store (docs 10.8, "Live results" 18 and 20).
#[derive(Clone, Debug, PartialEq)]
pub struct Live {
    /// `correlationId`: the call's id, the same when someone is added to a 1:1 call.
    pub call: String,
    /// Where to join and read the call.
    pub controller: String,
    /// The call's chat thread and its message id: `groupContext`, or the conversation's
    /// `activeModalities.groupChat` once it has one. None in a 1:1 call.
    pub chat: Option<(String, String)>,
    pub meeting_info: Value,
    /// The other person's object id, when `participants` are the user and one other.
    pub other: Option<String>,
}

impl Live {
    /// Whether `conversation` names this call: its chat thread, `call:<id>`, or the 1:1 chat of
    /// the user (object id `me`) and the other person, in either order of ids.
    pub fn named(&self, conversation: &str, me: &str) -> bool {
        let direct = |other: &String| {
            [format!("19:{me}_{other}@unq.gbl.spaces"), format!("19:{other}_{me}@unq.gbl.spaces")].contains(&conversation.to_string())
        };
        self.chat.as_ref().is_some_and(|(thread, _)| thread == conversation)
            || conversation.strip_prefix("call:") == Some(self.call.as_str())
            || self.other.as_ref().is_some_and(direct)
    }
}

/// The call store's request (docs 10.7, "Live results" 11): the user's calls.
pub fn call_store(mri: &str) -> HttpRequest {
    let url = format!("https://{SERVICE_HOST}/api/v2/ep/api.userstore.skype.com/api/v1/user/{mri}/conversations");
    HttpRequest { method: "GET".to_string(), url, headers: Vec::new(), body: Vec::new() }
}

/// The user's calls in the call store (`me` is the user's MRI), once each, and how many items
/// were left out: no call id of the pattern, or a conversation URL the module would not send a
/// token to (archive spec 4.2, R6).
pub fn calls(body: &[u8], me: &str) -> Result<(Vec<Live>, usize), ProtoError> {
    let v = parse_json(body)?;
    let items = v.as_array().ok_or_else(|| incompatible("$"))?;
    let (mut calls, mut skipped): (Vec<Live>, usize) = (Vec::new(), 0);
    for item in items {
        let call = item.get("correlationId").and_then(Value::as_str).filter(|id| is_call(id));
        let controller = item.get("conversationController").and_then(Value::as_str).filter(|url| on_service(url));
        let (Some(call), Some(controller)) = (call, controller) else {
            skipped += 1;
            continue;
        };
        if calls.iter().any(|c| c.call == call) {
            continue;
        }
        calls.push(Live {
            call: call.to_string(),
            controller: controller.to_string(),
            chat: chat(item.get("groupContext")),
            meeting_info: item.get("meetingInfo").filter(|info| info.is_object()).cloned().unwrap_or(Value::Null),
            other: other(item, me),
        });
    }
    Ok((calls, skipped))
}

/// A `{threadId, messageId}` object's chat thread, and its message id (`"0"` without one, as the
/// web client sends it).
fn chat(v: Option<&Value>) -> Option<(String, String)> {
    let v = v?;
    let thread = v.get("threadId").and_then(Value::as_str).filter(|t| is_thread(t))?;
    let message = v.get("messageId").and_then(Value::as_str).filter(|m| (1..=32).contains(&m.len()) && m.bytes().all(|b| b.is_ascii_digit()));
    Some((thread.to_string(), message.unwrap_or("0").to_string()))
}

/// The other person's object id of a call whose `participants` are the user (`me`) and one other
/// person, MRIs as plain strings ("Live results" 20).
fn other(item: &Value, me: &str) -> Option<String> {
    let [a, b] = item.get("participants")?.as_array()?.as_slice() else { return None };
    let (a, b) = (a.as_str()?, b.as_str()?);
    let other = match (a.eq_ignore_ascii_case(me), b.eq_ignore_ascii_case(me)) {
        (true, false) => b,
        (false, true) => a,
        _ => return None,
    };
    let oid = other.strip_prefix("8:orgid:")?;
    ((1..=64).contains(&oid.len()) && oid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')).then(|| oid.to_string())
}

/// open_mess's endpoint in the call, as `participants.from` names it.
pub struct Me<'a> {
    pub mri: &'a str,
    pub name: &'a str,
    pub endpoint: &'a str,
    pub participant: &'a str,
}

impl Me<'_> {
    fn json(&self) -> Value {
        json!({"id": self.mri, "displayName": self.name, "endpointId": self.endpoint, "participantId": self.participant, "languageId": "en-US"})
    }
}

/// The callback URLs of a session: `<surl>callAgent/<session>/<tag><path>` (as the probe made
/// them, "Live results" 15), a tag a link.
pub struct Links<'a> {
    pub surl: &'a str,
    pub session: &'a str,
    pub tag: u32,
}

impl Links<'_> {
    fn link(&self, n: u32, path: &str) -> String {
        let slash = if self.surl.ends_with('/') { "" } else { "/" };
        format!("{}{slash}callAgent/{}/{:08x}{path}", self.surl, self.session, self.tag.wrapping_add(n))
    }
}

/// `groupChat` of a request: the call's chat thread, or null in a 1:1 call (docs 10.8: a second
/// device joins a 1:1 call without a thread).
fn group_chat(live: &Live) -> Value {
    live.chat.as_ref().map_or(Value::Null, |(thread, message)| json!({"threadId": thread, "messageId": message}))
}

/// The join (`POST <controller>`): the probe's body that gave captions with a data-only offer
/// ("Live results" 15 and 18) - no `payload` envelope, muted, no call modalities.
pub fn join(me: &Me<'_>, links: &Links<'_>, live: &Live, offer: &str, media_leg: &str) -> Value {
    let l = |n: u32, path: &str| links.link(n, path);
    json!({
        "conversationRequest": {
            "conversationType": null,
            "subject": null,
            "suppressDialout": true,
            "roster": {"type": "Delta", "rosterUpdate": l(0, "/conversation/rosterUpdate/")},
            "properties": {
                "allowConversationWithoutHost": true,
                "enableGroupCallEventMessages": true,
                "enableGroupCallUpgradeMessage": false,
                "enableGroupCallMeetupGeneration": false,
            },
            "links": {
                "conversationEnd": l(1, "/conversation/conversationEnd/"),
                "conversationUpdate": l(2, "/conversation/conversationUpdate/"),
                "localParticipantUpdate": l(3, "/conversation/localParticipantUpdate/"),
                "addParticipantSuccess": l(4, "/conversation/addParticipantSuccess/"),
                "addParticipantFailure": l(5, "/conversation/addParticipantFailure/"),
                "addModalitySuccess": l(6, "/conversation/addModalitySuccess/"),
                "addModalityFailure": l(7, "/conversation/addModalityFailure/"),
                "confirmUnmute": l(8, "/conversation/confirmUnmute/"),
                "receiveMessage": l(9, "/conversation/receiveMessage/"),
            },
        },
        "groupContext": null,
        "groupChat": group_chat(live),
        "contentSharing": null,
        "participants": {"from": me.json(), "to": []},
        "meetingData": null,
        "meetingPreferences": {"shouldResurrect": "Resurrect"},
        "endpointState": {"endpointStateSequenceNumber": 0, "state": {"isMuted": true}},
        "capabilities": null,
        "endpointCapabilities": ENDPOINT_CAPABILITIES,
        "clientEndpointCapabilities": null,
        "endpointMetadata": {},
        "meetingInfo": live.meeting_info,
        "callInvitation": {
            "callModalities": [],
            "replaces": null,
            "transferor": null,
            "clientTransferContext": null,
            "customContext": null,
            "emergencyContent": null,
            "locationContent": null,
            "networkContent": null,
            "areaContent": null,
            "pstnContent": {"emergencyCallCountry": "", "platformName": CLIENT, "publicApiCall": false},
            "links": {
                "progress": l(10, "/call/progress/"),
                "mediaAnswer": l(11, "/call/mediaAnswer/"),
                "acceptance": l(12, "/call/acceptance/"),
                "redirection": l(13, "/call/redirection/"),
                "end": l(14, "/call/end/"),
            },
            "clientContentForMediaController": {"controlVideoStreaming": l(15, "/call/controlVideoStreaming/"), "csrcInfo": l(16, "/call/csrcInfo/")},
            "mediaContent": {"contentType": "application/sdp-ngc-1.0", "blob": offer, "clientLocation": CLIENT_LOCATION, "mediaLegId": media_leg},
        },
    })
}

/// Adding the captions bot (docs 10.8, `addParticipantsToCall` and `createBotPayload`): the
/// combined bot, as the official client adds it with its shipped configuration, `botData` with
/// the captions language and `token`, the user's token for the recording resource. The values the
/// static analysis did not show (`disableUnmute`, the target's ids, `mode`, `consumerType`) are the
/// client's names for the combined bot, to be checked live (step 0b(3) of the archive spec).
pub fn add_bot(me: &Me<'_>, links: &Links<'_>, live: &Live, language: &str, token: &str) -> Value {
    json!({
        "disableUnmute": false,
        "participants": {"from": me.json(), "to": [{"id": COMBINED_BOT, "assertedId": null, "displayName": null, "participantId": null}]},
        "participantInvitationData": {"botData": {
            "meetingTitle": null,
            "clientInfo": "Teams-general",
            "callId": live.call,
            "threadId": live.chat.as_ref().map(|(thread, _)| thread),
            "recorderFeatures": null,
            "mode": "RecordingAndTranscription",
            "iCalUid": null,
            "consumerType": "CombinedClosedCaptions",
            "spokenLanguage": language,
            "correlationId": live.call,
            "initiatorUserToken": token,
            "exchangeId": null,
            "meetingOrganizer": live.meeting_info.get("organizerId"),
        }},
        "replacementDetails": null,
        "groupContext": null,
        "groupChat": group_chat(live),
        "links": {
            "addParticipantSuccess": links.link(4, "/conversation/addParticipantSuccess/"),
            "addParticipantFailure": links.link(5, "/conversation/addParticipantFailure/"),
        },
    })
}

/// The `start` command to the captions bot (docs 10.8, `CallingBotRestClient.sendCommand`): only
/// captions, no language. `participant` is open_mess's leg (its `participantId`; which id the
/// client puts in `participantLegId` is to be checked live), `call` the chain id. The tokens are
/// the caller's to add.
pub fn start(url: &str, me: &str, participant: &str, call: &str, request: &str, now_ms: i64) -> HttpRequest {
    let body = json!({
        "timestamp": crate::time::rfc3339_millis(now_ms),
        "participantMri": me,
        "participantLegId": participant,
        "action": "start",
        "mode": "transcription",
        "processingModes": ["closedCaptions"],
        "participantSkypeToken": "",
    });
    let headers = [("Content-Type", "application/json"), ("requestId", request), ("X-Microsoft-Skype-Chain-ID", call)];
    HttpRequest {
        method: "POST".to_string(),
        url: url.to_string(),
        headers: headers.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect(),
        body: body.to_string().into_bytes(),
    }
}

/// What the module reads of a conversation (the join's answer or `GET <controller>`; docs 10.7,
/// 10.8). Links are kept only on the service's host.
#[derive(Debug, Default, PartialEq)]
pub struct Resource {
    pub leave: Option<String>,
    /// `links.addParticipantAndModality` and `links.addParticipant`: where the bot is added.
    pub add_with_chat: Option<String>,
    pub add: Option<String>,
    pub roster: Option<Roster>,
    /// `activeModalities.groupChat`: the call's chat thread and message id, once it has one.
    pub chat: Option<(String, String)>,
    /// `meetingDetails.meetingCapability.allowTranscription`; null is no refusal ("Live results" 20).
    pub allow_transcription: Option<bool>,
}

/// A conversation as [`Resource`]; `me` is the user's MRI and `endpoint` open_mess's own device.
pub fn resource(body: &[u8], me: &str, endpoint: &str) -> Result<Resource, ProtoError> {
    let v = parse_json(body)?;
    let link =
        |name: &str| v.get("links").and_then(|links| links.get(name)).and_then(Value::as_str).filter(|url| on_service(url)).map(str::to_string);
    let roster = match v.get("roster") {
        Some(roster) => Some(read_roster(roster, me, endpoint)?),
        None => None,
    };
    Ok(Resource {
        leave: link("leave"),
        add_with_chat: link("addParticipantAndModality"),
        add: link("addParticipant"),
        roster,
        chat: chat(v.pointer("/activeModalities/groupChat")),
        allow_transcription: v.pointer("/meetingDetails/meetingCapability/allowTranscription").and_then(Value::as_bool),
    })
}

/// The roster of a conversation (`roster.participants`, keyed by MRI; "Live results" 15-20): the
/// user's entry lists its devices by id, and the captions bot, one of [`BOTS`], has
/// `processingModes.closedCaptions.state` `Active` while captions are on (docs 10.7) and its
/// `commandUrl`. Another device of the user counts only with a media stream other than `data`:
/// a leg of open_mess, here or on another computer, has only `data` (archive spec 4.2, docs 10.8).
fn read_roster(roster: &Value, me: &str, endpoint: &str) -> Result<Roster, ProtoError> {
    let participants = roster.get("participants").and_then(Value::as_object).ok_or_else(|| incompatible("roster.participants"))?;
    let mut found = Roster { user_elsewhere: false, captions: false, bot: false, command: None };
    for (mri, entry) in participants {
        if mri.eq_ignore_ascii_case(me) {
            let endpoints = entry.get("endpoints").and_then(Value::as_object).ok_or_else(|| incompatible("roster.participants.endpoints"))?;
            found.user_elsewhere |= endpoints.iter().any(|(id, device)| !id.eq_ignore_ascii_case(endpoint) && media(device));
        } else if BOTS.iter().any(|bot| mri.eq_ignore_ascii_case(bot)) {
            found.bot = true;
            found.captions |= captions_active(entry, 0);
            let devices = entry.get("endpoints").and_then(Value::as_object).into_iter().flat_map(|endpoints| endpoints.values());
            let command =
                devices.filter_map(|device| device.pointer("/endpointMetadata/commandUrl").and_then(Value::as_str)).find(|url| on_service(url));
            found.command = found.command.take().or_else(|| command.map(str::to_string));
        }
    }
    Ok(found)
}

/// Whether a device is in the call with a media stream other than `data` (`call.mediaStreams`).
fn media(device: &Value) -> bool {
    let streams = device.pointer("/call/mediaStreams").and_then(Value::as_array);
    streams.is_some_and(|streams| streams.iter().any(|s| s.get("type").and_then(Value::as_str).is_some_and(|t| t != "data")))
}

/// A `processingModes.closedCaptions.state` of `Active` anywhere in a bot's entry, as the probe
/// found it ("Live results" 15), within a few levels.
fn captions_active(v: &Value, depth: u8) -> bool {
    if depth > 8 {
        return false;
    }
    match v {
        Value::Object(map) => {
            map.get("processingModes").and_then(|modes| modes.pointer("/closedCaptions/state")).and_then(Value::as_str) == Some("Active")
                || map.values().any(|v| captions_active(v, depth + 1))
        }
        Value::Array(items) => items.iter().any(|v| captions_active(v, depth + 1)),
        _ => false,
    }
}

/// What an approval shows of a call without a chat (archive spec 4.3): the conversation's
/// `subject` (at most 200 characters, "Teams call without a chat" when it has none) with its
/// `conversationStartTime`, and the people of its roster, MRI and name (the MRI without one).
pub fn call_target(body: &[u8]) -> Result<(String, Vec<(String, String)>), ProtoError> {
    let v = parse_json(body)?;
    let subject = v.get("subject").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let mut title = subject.map_or_else(|| "Teams call without a chat".to_string(), |s| cut(s, MAX_SUBJECT));
    if let Some(at) = v.get("conversationStartTime").and_then(Value::as_str).and_then(crate::time::rfc3339_ms) {
        title = format!("{title}, started {}", crate::time::rfc3339(at));
    }
    let participants = v.pointer("/roster/participants").and_then(Value::as_object).ok_or_else(|| incompatible("roster.participants"))?;
    let people = participants
        .iter()
        .filter(|(mri, _)| mri.len() <= 256)
        .take(MAX_PEOPLE)
        .map(|(mri, entry)| {
            let name = entry.get("displayName").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty());
            (mri.clone(), cut(name.unwrap_or(mri), MAX_SPEAKER))
        })
        .collect();
    Ok((title, people))
}

/// The body of `leave` (docs 10.7): open_mess's endpoint, ended by the user.
pub fn leave(me: &Me<'_>) -> Value {
    json!({"participants": {"from": me.json()}, "callTransactionEnd": {"code": 0, "subCode": 0, "phrase": "LOCAL_USER_INITIATED"}})
}

/// The acknowledgement of the call's acceptance ("Live results" 15).
pub fn acknowledgement() -> Value {
    json!({"callAcceptanceAcknowledgement": {"links": {}}})
}

/// A call callback, by its path after `callAgent/<session>/<tag>`.
#[derive(Debug, PartialEq, Eq)]
pub enum Call {
    /// `/call/acceptance/`: the SDP answer, and where to acknowledge it.
    Acceptance { answer: String, acknowledgement: Option<String> },
    /// `/call/acceptance/` into the lobby (`controllerName` `lobby`, docs 10.8).
    Lobby,
    /// `/conversation/rosterUpdate/`: the roster changed; the module reads it whole.
    Roster,
    /// `/conversation/conversationEnd/` (`meeting`, the meeting is over) or `/call/end/` (the
    /// service ended open_mess's leg), with the end's code and sub-code (docs 10.7, 10.8).
    Ended { meeting: bool, code: Option<i64>, sub_code: Option<i64> },
    /// Anything else, or another session's: not read.
    Other,
}

/// What a callback of the session `session` is (docs 10.7).
pub fn callback(url: &str, body: &str, session: &str) -> Result<Call, ProtoError> {
    let Some(rest) = url.split_once("/callAgent/").map(|(_, rest)| rest) else { return Ok(Call::Other) };
    let mut parts = rest.splitn(3, '/');
    if parts.next() != Some(session) {
        return Ok(Call::Other);
    }
    let path = parts.nth(1).map(|path| format!("/{path}")).unwrap_or_default();
    match path.as_str() {
        "/call/acceptance/" => {
            let v: Value = serde_json::from_str(body).map_err(|_| incompatible("callAcceptance"))?;
            if v.pointer("/callAcceptance/controllerName").and_then(Value::as_str) == Some("lobby") {
                return Ok(Call::Lobby);
            }
            let answer = v
                .pointer("/callAcceptance/mediaContent/blob")
                .and_then(Value::as_str)
                .ok_or_else(|| incompatible("callAcceptance.mediaContent.blob"))?;
            let acknowledgement =
                v.pointer("/callAcceptance/links/acknowledgement").and_then(Value::as_str).filter(|url| on_service(url)).map(str::to_string);
            Ok(Call::Acceptance { answer: answer.to_string(), acknowledgement })
        }
        "/conversation/rosterUpdate/" => Ok(Call::Roster),
        "/call/end/" | "/conversation/conversationEnd/" => {
            let meeting = path == "/conversation/conversationEnd/";
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let end = v.get(if meeting { "conversationEnd" } else { "callEnd" });
            // The client's end screen reads `callControllerCode` and `callControllerSubCode` (docs
            // 10.8); the ends seen live had `code` and `subCode` ("Live results" 19).
            let number = |keys: [&str; 2]| keys.iter().find_map(|key| end.and_then(|end| end.get(*key)).and_then(Value::as_i64));
            Ok(Call::Ended { meeting, code: number(["callControllerCode", "code"]), sub_code: number(["callControllerSubCode", "subCode"]) })
        }
        _ => Ok(Call::Other),
    }
}

/// A request to the conversation service with the web client's headers ("Live results" 14), less
/// the skype token, which the caller adds. `chain` is one id for the whole call.
pub fn service(method: &str, url: &str, body: Option<&Value>, chain: &str, message: &str, region: Option<(&str, &str)>) -> HttpRequest {
    let mut headers = vec![
        ("Content-Type", "application/json".to_string()),
        ("X-Microsoft-Skype-Chain-ID", chain.to_string()),
        ("X-Microsoft-Skype-Message-ID", message.to_string()),
        ("X-Microsoft-Skype-Client", CLIENT.to_string()),
        ("MS-Teams-Ring", "general".to_string()),
    ];
    if let Some((region, partition)) = region {
        headers.extend([("MS-Teams-Region", region.to_string()), ("MS-Teams-Partition", partition.to_string())]);
    }
    HttpRequest {
        method: method.to_string(),
        url: url.to_string(),
        headers: headers.into_iter().map(|(name, value)| (name.to_string(), value)).collect(),
        body: body.map(|b| b.to_string().into_bytes()).unwrap_or_default(),
    }
}

/// What the module stores of a leg before it joins, to leave it from another instance (spec 4.3):
/// no content, no token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub controller: String,
    pub endpoint: String,
    pub participant: String,
    pub at_ms: i64,
}

/// The records of an account, as stored.
pub fn encode_records(records: &[Record]) -> Vec<u8> {
    let start = records.len().saturating_sub(MAX_RECORDS);
    let items: Vec<Value> = records
        .iter()
        .skip(start)
        .map(|r| json!({"controller": r.controller, "endpoint": r.endpoint, "participant": r.participant, "at_ms": r.at_ms}))
        .collect();
    Value::Array(items).to_string().into_bytes()
}

/// Stored records; ones that cannot be read (or name a URL the module would not send a token to)
/// are left out.
pub fn decode_records(bytes: &[u8]) -> Vec<Record> {
    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(bytes) else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_string);
            Some(Record {
                controller: text("controller").filter(|u| on_service(u))?,
                endpoint: text("endpoint")?,
                participant: text("participant")?,
                at_ms: item.get("at_ms")?.as_i64()?,
            })
        })
        .collect()
}

/// The calls whose captions were on (archive spec 4.3), as stored: `[account, call, at_ms]`, the
/// newest last; ones older than a day or that cannot be read are left out.
fn seen_calls(stored: Option<&[u8]>, now: i64) -> Vec<(String, String, i64)> {
    let items = stored.and_then(|bytes| serde_json::from_slice::<Vec<(String, String, i64)>>(bytes).ok()).unwrap_or_default();
    items.into_iter().filter(|(_, _, at)| now.saturating_sub(*at) < SEEN_FOR_MS).collect()
}

/// Whether `account` saw captions on in `call` within the day: then open_mess never turns them on
/// there, whatever the caller asks (archive spec 4.3, R7).
pub fn seen(stored: Option<&[u8]>, account: &str, call: &str, now: i64) -> bool {
    seen_calls(stored, now).iter().any(|(a, c, _)| a == account && c == call)
}

/// The stored list with `account` and `call` added at `now`, at most [`MAX_SEEN`], the oldest out.
pub fn note_seen(stored: Option<&[u8]>, account: &str, call: &str, now: i64) -> Vec<u8> {
    let mut items = seen_calls(stored, now);
    items.retain(|(a, c, _)| !(a == account && c == call));
    items.push((account.to_string(), call.to_string(), now));
    let start = items.len().saturating_sub(MAX_SEEN);
    serde_json::to_vec(items.get(start..).unwrap_or_default()).unwrap_or_default()
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "8:orgid:00000000-0000-0000-0000-000000000001";
    const ME_OID: &str = "00000000-0000-0000-0000-000000000001";
    const OTHER_OID: &str = "00000000-0000-0000-0000-000000000002";
    const ENDPOINT: &str = "11111111-1111-4111-8111-111111111111";
    const THREAD: &str = "19:meeting_c3ludGhldGlj@thread.v2";
    const GROUP: &str = "19:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@thread.v2";
    const CALL: &str = "5a3e1b9c-0000-4000-8000-00000000000c";
    const CONTROLLER: &str = "https://api.flightproxy.teams.microsoft.com/api/v2/ep/conv.example/conv/abc";
    const COMMAND: &str = "https://api.flightproxy.teams.microsoft.com/api/v2/bot/command";

    /// A data channel fragment as docs 10.7 frames it.
    fn packet(start: bool, id: u8, seq: u16, remaining: u8, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0x10, 11, if start { 0x80 } else { 0 } | id];
        b.extend(seq.to_be_bytes());
        b.push(remaining);
        b.extend(7u32.to_be_bytes());
        b.push(0);
        b.extend(payload);
        b
    }

    fn said(text: &str, last: bool, by: &str) -> Vec<u8> {
        let results = json!({"recognitionResults": [
            {"isFinal": false, "text": "partial", "displayName": "Speaker"},
            {"isFinal": last, "text": text, "displayName": "Speaker", "spokenLanguage": "pl-pl", "userId": by},
        ], "events": [], "version": 1});
        packet(true, 3, 1, 0, results.to_string().as_bytes())
    }

    fn caption(text: &str, last: bool) -> Vec<u8> {
        said(text, last, "8:orgid:x")
    }

    /// A roster with the user's other device, and the captions bot in the state given.
    fn roster(user_elsewhere: bool, captions: bool) -> Roster {
        Roster { user_elsewhere, captions, bot: captions, command: captions.then(|| COMMAND.to_string()) }
    }

    fn joining(now: i64) -> Session {
        Session::new(THREAD, CALL, "dG9rZW4", ME, now)
    }

    fn live(now: i64) -> Session {
        let mut s = joining(now);
        s.answered();
        s.channel_open(now);
        assert_eq!(s.roster(&roster(true, true), now), None);
        assert_eq!(s.due(now + ENABLE_AFTER_MS), None);
        s
    }

    #[test]
    fn final_captions_are_paged_with_cursor_language_and_own() {
        let mut s = live(0);
        let messages = [caption("one", true), caption("not yet", false), said("two", true, &ME.to_uppercase())];
        assert_eq!(s.data(&messages, 0, 1_800_000_000_123), None);
        let page = s.read(None, None, 65536, 1).unwrap();
        assert_eq!(page["state"], "live");
        assert_eq!((page["call"].as_str(), page["captions_on"].as_bool(), page["captions_seen"].as_bool()), (Some(CALL), Some(true), Some(true)));
        assert_eq!((page["next_cursor"].as_u64(), page["skipped"].as_u64()), (Some(2), Some(0)));
        assert_eq!(page["language"], "pl-pl");
        assert_eq!(page["reason"], Value::Null);
        // One thread, the conversation joined: no `threads`.
        assert!(page.get("threads").is_none());
        assert_eq!(
            page["untrusted"]["cues"],
            json!([
                {"at": "2027-01-15T08:00:00.123Z", "speaker": "Speaker", "own": false, "text": "one"},
                {"at": "2027-01-15T08:00:00.123Z", "speaker": "Speaker", "own": true, "text": "two"},
            ])
        );
        let next = s.read(Some("dG9rZW4"), Some(2), 65536, 2).unwrap();
        assert_eq!((next["next_cursor"].as_u64(), next["untrusted"]["cues"].as_array().map(Vec::len)), (Some(2), Some(0)));
        assert_eq!(next["language"], Value::Null);
        // Another session's token: refused, so two sessions never mix.
        assert_eq!(s.read(Some("b3RoZXI"), Some(0), 65536, 3), Err(SessionChanged));
    }

    #[test]
    fn a_page_holds_at_least_one_cue_and_stays_in_its_budget() {
        let mut s = live(0);
        let long = "x".repeat(5000);
        s.data(&[caption(&long, true), caption("b", true), caption("c", true)], 0, 0);
        let one = s.read(None, Some(0), 4096, 0).unwrap();
        let text = one["untrusted"]["cues"][0]["text"].as_str().unwrap();
        assert_eq!((text.chars().count(), text.ends_with('\u{2026}')), (MAX_TEXT, true));
        assert_eq!(one["next_cursor"], 1);
        let rest = s.read(None, Some(1), 4096, 0).unwrap();
        assert_eq!(rest["next_cursor"], 3);
    }

    #[test]
    fn fragments_are_put_together_and_bad_frames_counted() {
        let mut s = live(0);
        let payload = json!({"recognitionResults": [{"isFinal": true, "text": "joined", "displayName": "A"}]}).to_string();
        let (a, b) = payload.split_at(10);
        let messages = [packet(true, 3, 5, 1, a.as_bytes()), packet(false, 3, 6, 0, b.as_bytes()), vec![0x20, 0], packet(true, 24, 1, 0, b"rtt")];
        assert_eq!(s.data(&messages, 2, 0), None);
        let page = s.read(None, None, 65536, 0).unwrap();
        assert_eq!((page["untrusted"]["cues"][0]["text"].as_str(), page["untrusted"]["cues"][0]["own"].as_bool()), (Some("joined"), Some(false)));
        // Two dropped by the host and one frame that could not be read.
        assert_eq!(page["missed"], 3);
        // A fragment out of sequence breaks its message.
        let mut s = live(0);
        s.data(&[packet(true, 3, 5, 1, b"{"), packet(false, 3, 9, 0, b"}")], 0, 0);
        assert_eq!(s.read(None, None, 65536, 0).unwrap()["missed"], 1);
    }

    #[test]
    fn a_hundred_unreadable_messages_in_a_row_end_the_session() {
        let mut s = live(0);
        let junk: Vec<Vec<u8>> = (0..99).map(|_| packet(true, 3, 1, 0, b"not json")).collect();
        assert_eq!(s.data(&junk, 0, 0), None);
        assert_eq!(s.data(&[caption("fine", true)], 0, 0), None);
        assert_eq!(s.data(&junk, 0, 0), None);
        assert_eq!(s.data(&[packet(true, 3, 1, 0, b"[1]")], 0, 0), Some(Reason::Protocol));
    }

    #[test]
    fn the_oldest_cues_go_unread_ones_count_as_missed_and_a_late_cursor_as_skipped() {
        let mut buffer = Buffer::default();
        let cue = |n: usize| Cue { at_ms: 0, speaker: None, own: false, text: n.to_string(), language: None, bytes: 1 };
        for n in 0..MAX_CUES {
            buffer.push(cue(n));
        }
        buffer.page(0, 100);
        let read = buffer.read_to;
        for n in 0..10 {
            buffer.push(cue(MAX_CUES + n));
        }
        assert_eq!(buffer.first, 10);
        // The ones read before they went are not missed.
        assert_eq!(buffer.missed, 10 - read);
        // A reader still at 4 lost 6 cues, though someone else read some of them (archive spec 4.3).
        let page = buffer.page(4, 100);
        assert_eq!((page.cues[0]["text"].as_str(), page.next, page.skipped), (Some("10"), 10 + page.cues.len() as u64, 6));
        assert_eq!(buffer.page(page.next, 100).skipped, 0);
        // Bytes too: a cue over the space left pushes out old ones.
        let mut buffer = Buffer::default();
        buffer.push(Cue { bytes: MAX_BUFFER_BYTES - 10, ..cue(0) });
        buffer.push(Cue { bytes: 20, ..cue(1) });
        assert_eq!((buffer.first, buffer.bytes), (1, 20));
    }

    #[test]
    fn deadlines_end_the_session_with_their_reasons() {
        // No answer within the join budget, then no data channel after it.
        let mut s = joining(0);
        assert_eq!((s.due(JOIN_MS - 1), s.due(JOIN_MS)), (None, Some(Due::End(Reason::Protocol))));
        let mut s = joining(0);
        s.answered();
        assert_eq!(s.due(JOIN_MS), Some(Due::End(Reason::MediaFailed)));
        // Captions off for good keep the session: the archive notes the gap (archive spec 4.3).
        let mut s = joining(0);
        s.channel_open(1000);
        s.roster(&roster(true, false), 1000);
        assert_eq!(s.read(None, None, 4096, 3_600_000).unwrap()["captions_on"], false);
        assert_eq!(s.due(3_600_000), None);
        // Silence keeps the session; not reading it for 10 minutes ends it, and 3 hours do.
        let mut s = live(0);
        s.read(None, None, 4096, 9 * 60_000).unwrap();
        assert_eq!(s.due(15 * 60_000), None);
        assert_eq!(s.due(19 * 60_000), Some(Due::End(Reason::Idle)));
        let mut s = live(0);
        for minute in (5..=180).step_by(5) {
            s.read(None, None, 4096, minute * 60_000).unwrap();
        }
        assert_eq!(s.due(TIME_LIMIT_MS), Some(Due::End(Reason::TimeLimit)));
        // An ended session is read for 15 minutes, then forgotten.
        let mut s = live(0);
        s.end(Reason::Left, 1000);
        assert_eq!((s.state(), s.read(None, None, 4096, 2000).unwrap()["reason"].as_str()), (State::Ended, Some("left")));
        assert_eq!((s.due(1000 + KEEP_MS - 1), s.due(1000 + KEEP_MS)), (None, Some(Due::Forget)));
        s.end(Reason::Protocol, 3000);
        assert_eq!(s.state(), State::Ended);
    }

    #[test]
    fn captions_are_turned_on_when_no_one_has() {
        // No bot 10 s after the data channel opened: the bot is added, and gets `start` once it is
        // in the call without captions on; a try that fails is repeated every 5 minutes, 4 in all.
        let mut s = joining(0);
        s.want_captions();
        s.channel_open(1000);
        s.roster(&roster(true, false), 1000);
        assert_eq!(s.next_at(), Some(1000 + ENABLE_AFTER_MS));
        assert_eq!(s.due(1000 + ENABLE_AFTER_MS - 1), None);
        assert_eq!(s.due(1000 + ENABLE_AFTER_MS), Some(Due::AddBot));
        assert_eq!(s.due(1000 + ENABLE_AFTER_MS), None);
        let in_call = Roster { user_elsewhere: true, captions: false, bot: true, command: Some(COMMAND.to_string()) };
        s.roster(&in_call, 20_000);
        assert_eq!(s.next_at(), Some(0));
        assert_eq!(s.due(20_000), Some(Due::Start(COMMAND.to_string())));
        assert_eq!(s.due(20_000), None);
        // Still off: tried again with `start` to the bot that is there now.
        let again = 1000 + ENABLE_AFTER_MS + ENABLE_RETRY_MS;
        assert_eq!(s.next_at(), Some(again));
        // Read as the archive reads, so the session is not idle.
        fn at(s: &mut Session, t: i64) -> Option<Due> {
            s.read(None, None, 4096, t).unwrap();
            s.due(t)
        }
        assert_eq!(at(&mut s, again), Some(Due::Start(COMMAND.to_string())));
        assert_eq!(at(&mut s, again + ENABLE_RETRY_MS), Some(Due::Start(COMMAND.to_string())));
        assert_eq!(at(&mut s, again + 2 * ENABLE_RETRY_MS), Some(Due::Start(COMMAND.to_string())));
        assert_eq!(at(&mut s, again + 3 * ENABLE_RETRY_MS), None);
        assert_eq!(s.next_at(), Some(again + 3 * ENABLE_RETRY_MS + IDLE_MS));
        // A bot whose command URL is not on the service's host gets nothing.
        let mut s = joining(0);
        s.want_captions();
        s.channel_open(0);
        s.roster(&Roster { user_elsewhere: true, captions: false, bot: true, command: None }, 0);
        assert_eq!(s.due(ENABLE_AFTER_MS), None);
        // Not asked for (`start_captions: false`): nothing, until a later join asks.
        let mut s = joining(0);
        s.channel_open(0);
        s.roster(&roster(true, false), 0);
        assert_eq!(s.due(ENABLE_AFTER_MS), None);
        s.want_captions();
        assert_eq!(s.due(ENABLE_AFTER_MS + 1), Some(Due::AddBot));
        // On once, then off: a participant decided; open_mess never turns them on again.
        let mut s = joining(0);
        s.want_captions();
        s.channel_open(0);
        s.roster(&roster(true, true), 1);
        s.roster(&roster(true, false), 2);
        assert_eq!(s.due(ENABLE_AFTER_MS), None);
        let page = s.read(None, None, 4096, 3).unwrap();
        assert_eq!((page["captions_on"].as_bool(), page["captions_seen"].as_bool()), (Some(false), Some(true)));
        // No try during a gap: the roster may be stale.
        let mut s = joining(0);
        s.want_captions();
        s.channel_open(0);
        s.roster(&roster(true, false), 0);
        s.pause(1);
        assert_eq!(s.due(ENABLE_AFTER_MS), None);
    }

    #[test]
    fn captions_seen_holds_from_the_first_roster_with_captions_on_even_in_a_gap() {
        let mut s = joining(0);
        s.channel_open(0);
        assert!(!s.captions_seen());
        s.down(1);
        s.roster(&roster(true, true), 2);
        assert!(s.captions_seen());
        s.roster(&roster(true, false), 3);
        assert!(s.captions_seen());
    }

    #[test]
    fn the_user_must_be_in_the_meeting_on_another_endpoint() {
        let mut s = joining(0);
        assert_eq!(s.roster(&roster(false, true), 0), Some(Reason::UserNotInMeeting));
        let mut s = live(0);
        assert_eq!(s.roster(&roster(false, true), 5), Some(Reason::UserLeft));
        s.end(Reason::UserLeft, 5);
        assert_eq!(s.state(), State::Ended);
        s.end(Reason::Protocol, 6);
        let mut failed = joining(0);
        failed.end(Reason::UserNotInMeeting, 1);
        assert_eq!(failed.state(), State::Failed);
    }

    #[test]
    fn while_trouter_is_down_the_roster_is_read_every_10_s() {
        let mut s = live(0);
        s.down(1000);
        // A read that finds the user keeps the reads going and the captions out (Codex review of 96070fd).
        assert_eq!(s.roster(&roster(true, true), 11_000), None);
        assert_eq!(s.due(21_000), Some(Due::Roster));
        s.data(&[caption("lost", true)], 0, 21_000);
        assert_eq!(s.read(None, None, 4096, 21_000).unwrap()["missed"], 1);
        assert_eq!(s.roster(&roster(false, true), 21_000), Some(Reason::UserLeft));
        let mut s = live(0);
        s.down(50_000);
        s.roster(&roster(true, true), 60_000);
        assert_eq!(s.read(None, None, 4096, 60_000).unwrap()["captions_on"], Value::Null);
        // Back up: the next read lets captions in again.
        let mut s = live(0);
        s.down(1000);
        s.up();
        assert_eq!(s.roster(&roster(true, true), 2000), None);
        s.data(&[caption("kept", true)], 0, 2000);
        assert_eq!(s.read(None, None, 4096, 2000).unwrap()["untrusted"]["cues"][0]["text"], "kept");
        assert_eq!(s.next_at(), Some(2000 + IDLE_MS));
    }

    #[test]
    fn a_gap_drops_captions_until_the_roster_is_read_again() {
        let mut s = live(0);
        s.pause(1000);
        assert_eq!(s.read(None, None, 4096, 1000).unwrap()["captions_on"], Value::Null);
        s.data(&[caption("lost", true)], 0, 1000);
        assert_eq!(s.read(None, None, 4096, 1000).unwrap()["missed"], 1);
        assert_eq!(s.due(1000 + ROSTER_POLL_MS), Some(Due::Roster));
        s.roster_polled(1000 + ROSTER_POLL_MS);
        assert_eq!(s.next_at(), Some(1000 + 2 * ROSTER_POLL_MS));
        // The user left meanwhile: the session ends at the next read of the roster.
        assert_eq!(s.roster(&roster(false, true), 21_000), Some(Reason::UserLeft));
        let mut s = live(0);
        s.pause(1000);
        s.roster(&roster(true, true), 2000);
        s.data(&[caption("kept", true)], 0, 2000);
        assert_eq!(s.read(None, None, 4096, 2000).unwrap()["untrusted"]["cues"][0]["text"], "kept");
        assert_eq!(s.due(60_000), None);
    }

    #[test]
    fn a_session_keeps_every_thread_of_its_call() {
        // A 1:1 call named by its chat, then a person added: the group chat is the second thread.
        let direct = format!("19:{ME_OID}_{OTHER_OID}@unq.gbl.spaces");
        let mut s = Session::new(&direct, CALL, "t", ME, 0);
        assert!(s.thread(&direct) && s.thread("not a thread") && s.thread(GROUP));
        assert!(s.named(&direct) && s.named(GROUP) && !s.named(THREAD));
        assert_eq!(s.read(None, None, 4096, 0).unwrap()["threads"], json!([direct, GROUP]));
        // A call without a chat learns its thread after the join.
        let mut s = Session::new(&format!("call:{CALL}"), CALL, "t", ME, 0);
        assert!(s.read(None, None, 4096, 0).unwrap().get("threads").is_none());
        assert!(s.thread(GROUP));
        assert_eq!(s.read(None, None, 4096, 0).unwrap()["threads"], json!([GROUP]));
        // A call store that keeps changing threads ends the session.
        let mut s = joining(0);
        let added: Vec<bool> = (0..10).map(|n| s.thread(&format!("19:t{n}@thread.v2"))).collect();
        assert_eq!(added.iter().filter(|added| **added).count(), MAX_THREADS - 1);
        assert!(!added[9]);
    }

    #[test]
    fn the_call_store_lists_calls_named_and_checked() {
        let item =
            |call: &str, url: &str| json!({"correlationId": call, "conversationController": url, "userJoinTime": "2027-01-15T08:00:00.1234567Z"});
        let meeting = || {
            let mut m = item(CALL, CONTROLLER);
            m["groupContext"] = json!({"messageId": "1700000000000", "threadId": THREAD});
            m["meetingInfo"] = json!({"organizerId": "o", "tenantId": "t"});
            m
        };
        let direct = |call: &str, participants: Value| {
            let mut d = item(call, CONTROLLER);
            d["participants"] = participants;
            d["additionalData"] = json!({"participantList": [{"id": "x", "endpointType": null}]});
            d
        };
        let other = format!("8:orgid:{OTHER_OID}");
        let body = json!([
            meeting(),
            meeting(),
            direct("d1", json!([other, ME.to_uppercase()])),
            direct("d2", json!([other, "8:orgid:00000000-0000-0000-0000-000000000003"])),
            direct("d3", json!([other])),
            {"correlationId": "e1", "conversationController": CONTROLLER, "groupContext": {"threadId": "not a thread"}},
            item("bad id!", CONTROLLER),
            item(&"x".repeat(65), CONTROLLER),
            item("f1", "https://evil.example/conv"),
            {"conversationController": CONTROLLER},
        ]);
        let (calls, skipped) = calls(body.to_string().as_bytes(), ME).unwrap();
        assert_eq!(skipped, 4);
        let ids: Vec<&str> = calls.iter().map(|c| c.call.as_str()).collect();
        assert_eq!(ids, [CALL, "d1", "d2", "d3", "e1"]);
        let [m, d1, d2, d3, e1] = calls.as_slice() else { panic!() };
        assert_eq!(
            (m.controller.as_str(), &m.chat, &m.meeting_info),
            (CONTROLLER, &Some((THREAD.to_string(), "1700000000000".to_string())), &json!({"organizerId": "o", "tenantId": "t"}))
        );
        assert_eq!((&d1.chat, d1.other.as_deref()), (&None, Some(OTHER_OID)));
        assert_eq!((d2.other.as_deref(), d3.other.as_deref(), &e1.chat), (None, None, &None));
        // What a conversation may name a call: its thread, `call:<id>`, its 1:1 chat either way round.
        assert!(m.named(THREAD, ME_OID) && m.named(&format!("call:{CALL}"), ME_OID) && !m.named(GROUP, ME_OID));
        assert!(
            d1.named(&format!("19:{ME_OID}_{OTHER_OID}@unq.gbl.spaces"), ME_OID)
                && d1.named(&format!("19:{OTHER_OID}_{ME_OID}@unq.gbl.spaces"), ME_OID)
        );
        assert!(!d1.named(&format!("19:{ME_OID}_{ME_OID}@unq.gbl.spaces"), ME_OID) && !d1.named("call:d2", ME_OID));
        assert_eq!(super::calls(b"[]", ME), Ok((Vec::new(), 0)));
        assert_eq!(super::calls(br#"{"conversations": []}"#, ME), Err(incompatible("$")));
        assert!(is_call(CALL) && !is_call("") && !is_call("a b") && is_thread(THREAD) && !is_thread("19:no-at") && !is_thread("19:a/b@thread.v2"));
    }

    fn me() -> Me<'static> {
        Me { mri: ME, name: "Synthetic Person One", endpoint: ENDPOINT, participant: "p" }
    }

    fn meeting_call() -> Live {
        Live {
            call: CALL.to_string(),
            controller: CONTROLLER.to_string(),
            chat: Some((THREAD.to_string(), "0".to_string())),
            meeting_info: json!({"organizerId": "o"}),
            other: None,
        }
    }

    #[test]
    fn the_join_is_the_probes_data_only_body() {
        let live = meeting_call();
        let links = Links { surl: "https://trouter.example/v4/f/abc/", session: "s1", tag: 0xffff_fffe };
        let body = join(&me(), &links, &live, "v=0\r\n", "leg");
        assert_eq!(
            body["conversationRequest"]["roster"]["rosterUpdate"],
            "https://trouter.example/v4/f/abc/callAgent/s1/fffffffe/conversation/rosterUpdate/"
        );
        assert_eq!(body["callInvitation"]["links"]["acceptance"], "https://trouter.example/v4/f/abc/callAgent/s1/0000000a/call/acceptance/");
        assert_eq!(body["callInvitation"]["callModalities"], json!([]));
        assert_eq!(body["callInvitation"]["mediaContent"]["blob"], "v=0\r\n");
        assert_eq!(body["participants"]["from"]["endpointId"], ENDPOINT);
        assert_eq!(body["endpointState"]["state"]["isMuted"], true);
        assert_eq!((body["groupChat"]["threadId"].as_str(), body["meetingInfo"]["organizerId"].as_str()), (Some(THREAD), Some("o")));
        assert!(body.get("payload").is_none());
        let no_slash = Links { surl: "https://trouter.example/v4/f/abc", ..links };
        assert!(
            join(&me(), &no_slash, &live, "", "leg")["conversationRequest"]["links"]["conversationEnd"]
                .as_str()
                .is_some_and(|l| l.starts_with("https://trouter.example/v4/f/abc/callAgent/s1/"))
        );
        // A 1:1 call: no thread, as the client's second device joins it.
        let direct = Live { chat: None, ..live };
        assert_eq!(join(&me(), &no_slash, &direct, "", "leg")["groupChat"], Value::Null);
    }

    #[test]
    fn the_bot_is_added_and_started_as_the_client_does() {
        let links = Links { surl: "https://trouter.example/v4/f/abc/", session: "s1", tag: 0 };
        let body = add_bot(&me(), &links, &meeting_call(), "pl-pl", "aad-token");
        assert_eq!(body["participants"]["to"][0]["id"], COMBINED_BOT);
        let bot = &body["participantInvitationData"]["botData"];
        assert_eq!(
            (bot["spokenLanguage"].as_str(), bot["initiatorUserToken"].as_str(), bot["callId"].as_str()),
            (Some("pl-pl"), Some("aad-token"), Some(CALL))
        );
        assert_eq!((bot["threadId"].as_str(), bot["meetingOrganizer"].as_str()), (Some(THREAD), Some("o")));
        assert_eq!(body["groupChat"]["threadId"], THREAD);
        assert_eq!(
            body["links"]["addParticipantFailure"],
            "https://trouter.example/v4/f/abc/callAgent/s1/00000005/conversation/addParticipantFailure/"
        );
        let direct = add_bot(&me(), &links, &Live { chat: None, ..meeting_call() }, "en-us", "t");
        assert_eq!((&direct["groupChat"], &direct["participantInvitationData"]["botData"]["threadId"]), (&Value::Null, &Value::Null));
        // `start`: only captions, no language, the chain id the call's.
        let request = start(COMMAND, ME, "p", CALL, "r1", 1_800_000_000_123);
        let header = |name: &str| request.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        assert_eq!((request.method.as_str(), header("X-Microsoft-Skype-Chain-ID"), header("requestId")), ("POST", Some(CALL), Some("r1")));
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body,
            json!({"timestamp": "2027-01-15T08:00:00.123Z", "participantMri": ME, "participantLegId": "p", "action": "start",
                   "mode": "transcription", "processingModes": ["closedCaptions"], "participantSkypeToken": ""})
        );
    }

    #[test]
    fn the_roster_tells_the_users_other_device_and_the_captions_bot() {
        let device = |streams: &[&str]| json!({"call": {"mediaStreams": streams.iter().map(|t| json!({"type": t, "direction": "sendrecv"})).collect::<Vec<_>>()}});
        let bot = |state: &str, command: &str| json!({"endpoints": {"b": {"endpointMetadata": {"commandUrl": command, "processingModes": {"closedCaptions": {"state": state}}}}}});
        let conversation = |endpoints: Value, bots: Value| {
            let mut participants = json!({
                ME.to_uppercase(): {"state": "active", "endpoints": endpoints},
                "8:orgid:someone-else": {"endpoints": {"x": device(&["audio"])}},
                // Another `28:` participant, not a captions bot ("Live results" 20).
                "28:00000000-0000-0000-0000-0000000000cc": {"endpoints": {"a": device(&["audio"])}},
            });
            for (mri, entry) in bots.as_object().unwrap() {
                participants[mri] = entry.clone();
            }
            json!({"links": {"leave": format!("{CONTROLLER}/leave"), "addParticipant": format!("{CONTROLLER}/add"),
                             "addParticipantAndModality": "https://evil.example/add"},
                   "roster": {"participants": participants},
                   "activeModalities": {"groupChat": {"threadId": GROUP, "messageId": "1"}},
                   "meetingDetails": {"meetingCapability": {"allowTranscription": false}}})
            .to_string()
        };
        let official = json!({ENDPOINT: device(&["data"]), "official": device(&["audio", "video", "data"])});
        let both = conversation(official.clone(), json!({COMBINED_BOT: bot("Active", COMMAND)}));
        let r = resource(both.as_bytes(), ME, ENDPOINT).unwrap();
        assert_eq!((r.leave, r.add, r.add_with_chat), (Some(format!("{CONTROLLER}/leave")), Some(format!("{CONTROLLER}/add")), None));
        assert_eq!(r.roster, Some(Roster { user_elsewhere: true, captions: true, bot: true, command: Some(COMMAND.to_string()) }));
        assert_eq!((r.chat, r.allow_transcription), (Some((GROUP.to_string(), "1".to_string())), Some(false)));
        // The captions-only bot, off, with a command URL elsewhere: no URL.
        let off = conversation(official, json!({"28:b1902c3e-b9f7-4650-9b23-5772bd429747": bot("Inactive", "https://evil.example/c")}));
        assert_eq!(
            resource(off.as_bytes(), ME, ENDPOINT).unwrap().roster,
            Some(Roster { user_elsewhere: true, captions: false, bot: true, command: None })
        );
        // Only open_mess's devices - its own, another instance's data-only leg, one in the lobby -
        // and another `28:` that is not the bot: the user is not there, and no captions.
        let alone =
            json!({ENDPOINT: device(&["audio"]), "other-open-mess": device(&["data"]), "lobby": {"lobby": {"mediaStreams": [{"type": "audio"}]}}});
        let alone = conversation(alone, json!({}));
        assert_eq!(
            resource(alone.as_bytes(), ME, ENDPOINT).unwrap().roster,
            Some(Roster { user_elsewhere: false, captions: false, bot: false, command: None })
        );
        let bare = json!({"links": {"leave": "https://evil.example/leave"}, "meetingDetails": {"meetingCapability": {"allowTranscription": null}}});
        assert_eq!(resource(bare.to_string().as_bytes(), ME, ENDPOINT), Ok(Resource::default()));
        let listed = conversation(json!([ENDPOINT]), json!({}));
        assert!(resource(listed.as_bytes(), ME, ENDPOINT).is_err());
    }

    #[test]
    fn a_call_without_a_chat_is_described_from_its_conversation() {
        let body = json!({"subject": "  Weekly sync  ", "conversationStartTime": "2027-01-15T08:00:00.1234567Z", "roster": {"participants": {
            ME: {"displayName": "Synthetic Person One", "endpoints": {}},
            "8:orgid:nameless": {"endpoints": {}},
        }}});
        let (title, people) = call_target(body.to_string().as_bytes()).unwrap();
        assert_eq!(title, "Weekly sync, started 2027-01-15T08:00:00Z");
        assert_eq!(people, [(ME.to_string(), "Synthetic Person One".to_string()), ("8:orgid:nameless".to_string(), "8:orgid:nameless".to_string())]);
        let long = json!({"subject": "x".repeat(300), "roster": {"participants": {}}});
        assert_eq!(call_target(long.to_string().as_bytes()).unwrap().0.chars().count(), MAX_SUBJECT);
        let bare = json!({"subject": "", "conversationStartTime": "soon", "roster": {"participants": {}}});
        assert_eq!(call_target(bare.to_string().as_bytes()).unwrap().0, "Teams call without a chat");
        assert_eq!(call_target(b"{}"), Err(incompatible("roster.participants")));
    }

    #[test]
    fn refusals_are_told_apart_by_code() {
        assert_eq!(refusal(Some(403), Some(4158)), Reason::E2ee);
        for (code, sub) in [(0, 5854), (0, 5855), (413, 5807), (403, 5354)] {
            assert_eq!(refusal(Some(code), Some(sub)), Reason::Lobby, "{code}/{sub}");
        }
        for (code, sub) in [(403, 5448), (403, 5723), (403, 5724), (403, 5810), (403, 10005), (403, 10193), (412, 5204)] {
            assert_eq!(refusal(Some(code), Some(sub)), Reason::Policy, "{code}/{sub}");
        }
        for (code, sub) in [(Some(430), Some(10067)), (Some(403), None), (None, Some(4158)), (Some(410), Some(301_999))] {
            assert_eq!(refusal(code, sub), Reason::Refused, "{code:?}/{sub:?}");
        }
        assert_eq!(refused(403, br#"{"subCode": 4158}"#), Reason::E2ee);
        assert_eq!(refused(403, b"not json"), Reason::Refused);
        assert_eq!(Reason::E2ee.name(), "e2ee");
    }

    #[test]
    fn callbacks_are_told_apart_by_path_and_session() {
        let base = "https://trouter.example/v4/f/abc/callAgent/s1/0000000c";
        let acceptance =
            json!({"callAcceptance": {"mediaContent": {"blob": "v=0"}, "links": {"acknowledgement": format!("{CONTROLLER}/ack")}}}).to_string();
        assert_eq!(
            callback(&format!("{base}/call/acceptance/"), &acceptance, "s1"),
            Ok(Call::Acceptance { answer: "v=0".to_string(), acknowledgement: Some(format!("{CONTROLLER}/ack")) })
        );
        let foreign_ack =
            json!({"callAcceptance": {"mediaContent": {"blob": "v=0"}, "links": {"acknowledgement": "https://evil.example/ack"}}}).to_string();
        assert_eq!(
            callback(&format!("{base}/call/acceptance/"), &foreign_ack, "s1"),
            Ok(Call::Acceptance { answer: "v=0".to_string(), acknowledgement: None })
        );
        let lobby = json!({"callAcceptance": {"controllerName": "lobby"}}).to_string();
        assert_eq!(callback(&format!("{base}/call/acceptance/"), &lobby, "s1"), Ok(Call::Lobby));
        assert!(callback(&format!("{base}/call/acceptance/"), "{}", "s1").is_err());
        assert_eq!(callback(&format!("{base}/conversation/rosterUpdate/"), "{}", "s1"), Ok(Call::Roster));
        let ended = json!({"callEnd": {"code": 410, "subCode": 5000, "phrase": "x"}}).to_string();
        assert_eq!(callback(&format!("{base}/call/end/"), &ended, "s1"), Ok(Call::Ended { meeting: false, code: Some(410), sub_code: Some(5000) }));
        let diagnosed = json!({"callEnd": {"code": 0, "subCode": 0, "callControllerCode": 403, "callControllerSubCode": 4158}}).to_string();
        assert_eq!(
            callback(&format!("{base}/call/end/"), &diagnosed, "s1"),
            Ok(Call::Ended { meeting: false, code: Some(403), sub_code: Some(4158) })
        );
        assert_eq!(
            callback(&format!("{base}/conversation/conversationEnd/"), "", "s1"),
            Ok(Call::Ended { meeting: true, code: None, sub_code: None })
        );
        assert_eq!(callback(&format!("{base}/conversation/receiveMessage/"), "{}", "s1"), Ok(Call::Other));
        assert_eq!(callback(&format!("{base}/call/end/"), "", "s2"), Ok(Call::Other));
        assert_eq!(callback("https://trouter.example/messaging", "{}", "s1"), Ok(Call::Other));
    }

    #[test]
    fn records_round_trip_and_keep_the_newest() {
        let record = |n: i64| Record { controller: CONTROLLER.to_string(), endpoint: ENDPOINT.to_string(), participant: format!("p{n}"), at_ms: n };
        let records: Vec<Record> = (0..10).map(record).collect();
        let back = decode_records(&encode_records(&records));
        assert_eq!(back, records[2..].to_vec());
        let foreign = br#"[{"controller": "https://evil.example/x", "endpoint": "e", "participant": "p", "at_ms": 1}, {"endpoint": "e"}]"#;
        assert_eq!(decode_records(foreign), Vec::new());
        assert_eq!(decode_records(b"junk"), Vec::new());
    }

    #[test]
    fn calls_with_captions_seen_are_remembered_for_a_day_at_most_200() {
        let day = SEEN_FOR_MS;
        let stored = note_seen(None, "a", CALL, 1000);
        assert!(seen(Some(&stored), "a", CALL, 1000 + day - 1));
        assert!(!seen(Some(&stored), "a", CALL, 1000 + day) && !seen(Some(&stored), "b", CALL, 1000) && !seen(Some(&stored), "a", "x", 1000));
        let mut stored = stored;
        for n in 0..MAX_SEEN {
            stored = note_seen(Some(&stored), "a", &format!("c{n}"), 2000);
        }
        // The oldest went for the 200 newer ones.
        assert!(!seen(Some(&stored), "a", CALL, 2000) && seen(Some(&stored), "a", "c0", 2000));
        assert!(!seen(Some(b"junk"), "a", CALL, 0) && !seen(None, "a", CALL, 0));
        assert_eq!(seen_calls(Some(&note_seen(Some(b"junk"), "a", CALL, 0)), 0).len(), 1);
    }

    #[test]
    fn service_requests_carry_the_clients_headers() {
        let request = service("POST", CONTROLLER, Some(&json!({"a": 1})), "chain", "message", Some(("emea", "emea01")));
        let header = |name: &str| request.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        assert_eq!((header("X-Microsoft-Skype-Chain-ID"), header("MS-Teams-Partition")), (Some("chain"), Some("emea01")));
        assert_eq!(header("X-Microsoft-Skype-Client"), Some(CLIENT));
        assert_eq!(request.body, br#"{"a":1}"#);
        assert!(service("GET", CONTROLLER, None, "c", "m", None).headers.iter().all(|(n, _)| n != "MS-Teams-Region"));
        assert_eq!(call_store(ME).url, format!("https://{SERVICE_HOST}/api/v2/ep/api.userstore.skype.com/api/v1/user/{ME}/conversations"));
    }
}
