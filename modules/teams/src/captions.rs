//! Live captions of a Teams meeting (spec 2026-10-29-live-captions, 4 and 5.3;
//! docs/protocols/teams.md 10.7 and "Live results" 11-18): a session's state and deadlines, the
//! caption messages of the call's data channel, the buffer agents read, and the requests and
//! answers of the conversation service. No network and no clock: the glue (`live.rs`) does the
//! I/O and passes the time in.

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
/// join at the call store's URL needs ("Live results" 14 and 18).
const ENDPOINT_CAPABILITIES: u64 = 72935;
/// ponytail: the probe's `clientLocation`; the client sends the user's country. Take it from the
/// account if Teams turns out to care (step 5 of the spec).
const CLIENT_LOCATION: &str = "PL";

/// Deadlines of a session (spec 4.1-4.3).
pub const JOIN_MS: i64 = 60_000;
pub const CAPTIONS_WAIT_MS: i64 = 30_000;
pub const CAPTIONS_OFF_MS: i64 = 5 * 60_000;
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

/// Why a session ended (spec 4.2). Lobby, end-to-end encryption and policy refusals have no
/// signal of their own yet; the service's refusal of the join is `Refused`.
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
    NoCaptions,
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
            Reason::NoCaptions => "no_captions",
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
    /// JSON - at least one, at most [`MAX_PAGE_CUES`] - the next cursor, and the last one's language.
    fn page(&mut self, cursor: u64, max_bytes: usize) -> (Vec<Value>, u64, Option<String>) {
        let start = cursor.clamp(self.first, self.next());
        let skip = usize::try_from(start - self.first).unwrap_or(usize::MAX);
        let (mut items, mut used, mut language) = (Vec::new(), 0, None);
        for cue in self.cues.iter().skip(skip) {
            let item = json!({"at": crate::time::rfc3339(cue.at_ms), "speaker": cue.speaker, "text": cue.text});
            let size = item.to_string().len();
            if !items.is_empty() && (used + size > max_bytes || items.len() == MAX_PAGE_CUES) {
                break;
            }
            used += size;
            language = cue.language.clone();
            items.push(item);
        }
        let next = start + items.len() as u64;
        self.read_to = self.read_to.max(next);
        (items, next, language)
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

    /// The final captions of one data channel message, arrived at `now`. `None` when it cannot be
    /// read; then [`Feed::broken`] says whether that ends the session.
    fn push(&mut self, bytes: &[u8], now: i64) -> Option<Vec<Cue>> {
        let cues = match self.reassemble(bytes) {
            Ok(Some((CAPTIONS_DATA_ID, payload))) => captions(&payload, now),
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
/// with `isFinal`, `text`, `displayName` and `spokenLanguage`. `None` when it is not that shape.
fn captions(payload: &[u8], now: i64) -> Option<Vec<Cue>> {
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
        let language = result.get("spokenLanguage").and_then(Value::as_str).filter(|tag| language_tag(tag)).map(str::to_string);
        let bytes = text.len() + speaker.as_ref().map_or(0, String::len) + 64;
        cues.push(Cue { at_ms: now, speaker, text, language, bytes });
    }
    Some(cues)
}

/// What a roster says for the session (spec 4.1, 4.2).
#[derive(Debug, PartialEq, Eq)]
pub struct Roster {
    /// Another endpoint of the user is in the call: the user is in the meeting there.
    pub user_elsewhere: bool,
    /// The captions bot is in the call with `closedCaptions` active.
    pub captions: bool,
}

/// One live-captions session of an account (spec 4).
pub struct Session {
    pub conversation: String,
    /// The `session` agents pass back (base64url, random).
    pub token: String,
    state: State,
    reason: Option<Reason>,
    started: i64,
    ended: Option<i64>,
    last_read: i64,
    /// The service's SDP answer was accepted.
    answered: bool,
    /// When the data channel opened.
    channel_at: Option<i64>,
    /// A roster showed the user's other endpoint.
    roster_seen: bool,
    captions_on: Option<bool>,
    /// Captions were on 30 s after the data channel opened (spec 4.1), and since when they are off.
    checked: bool,
    off_since: Option<i64>,
    /// Since Trouter went down or lost a message: cues are dropped until a roster is read again.
    paused: bool,
    /// Trouter is down: the roster is read every 10 s until it is back (spec 5.3, Codex R24).
    down: bool,
    roster_at: Option<i64>,
    buffer: Buffer,
    feed: Feed,
}

impl Session {
    pub fn new(conversation: &str, token: &str, now: i64) -> Session {
        Session {
            conversation: conversation.to_string(),
            token: token.to_string(),
            state: State::Joining,
            reason: None,
            started: now,
            ended: None,
            last_read: now,
            answered: false,
            channel_at: None,
            roster_seen: false,
            captions_on: None,
            checked: false,
            off_since: None,
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

    /// The data channel opened: the session is live.
    pub fn channel_open(&mut self, now: i64) {
        if self.state == State::Joining {
            self.state = State::Live;
            self.channel_at = Some(now);
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
        if roster.captions {
            self.off_since = None;
        } else {
            self.off_since.get_or_insert(now);
        }
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
            match self.feed.push(message, now) {
                Some(cues) => cues.into_iter().for_each(|cue| self.buffer.push(cue)),
                None => self.buffer.missed += 1,
            }
            if self.feed.broken() {
                return Some(Reason::Protocol);
            }
        }
        None
    }

    /// What is due at `now`, the most urgent first. Notes that captions were on 30 s after the data
    /// channel opened, once they were.
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
        // Nobody had captions on 30 s after the channel opened; a gap decides at the next roster read.
        if !self.checked && !self.paused && self.channel_at.is_some_and(|at| past(at, CAPTIONS_WAIT_MS)) {
            if self.captions_on != Some(true) {
                return Some(Due::End(Reason::NoCaptions));
            }
            self.checked = true;
        }
        if self.checked && self.off_since.is_some_and(|at| past(at, CAPTIONS_OFF_MS)) {
            return Some(Due::End(Reason::NoCaptions));
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
        if !self.checked && !self.paused {
            times.extend(self.channel_at.map(|at| at.saturating_add(CAPTIONS_WAIT_MS)));
        }
        if self.checked {
            times.extend(self.off_since.map(|at| at.saturating_add(CAPTIONS_OFF_MS)));
        }
        times.extend(self.roster_at);
        times.into_iter().min()
    }

    /// The result of `teams_read_captions` (spec 4.2). `session` from the agent must be this one's;
    /// a read counts against the idle limit.
    pub fn read(&mut self, session: Option<&str>, cursor: Option<u64>, max_bytes: usize, now: i64) -> Result<Value, SessionChanged> {
        if session.is_some_and(|s| s != self.token) {
            return Err(SessionChanged);
        }
        let (cues, next, language) = self.buffer.page(cursor.unwrap_or(0), max_bytes);
        self.last_read = now;
        let captions_on = if self.active() { self.captions_on } else { None };
        Ok(json!({
            "state": self.state.name(),
            "session": self.token,
            "captions_on": captions_on,
            "next_cursor": next,
            "missed": self.buffer.missed,
            "language": language,
            "reason": self.reason.map(Reason::name),
            "untrusted": {"cues": cues},
        }))
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

/// The user's live call in the meeting, from the call store (spec 4.1).
#[derive(Debug, PartialEq)]
pub struct Live {
    /// Where to join and read the roster.
    pub controller: String,
    pub meeting_info: Value,
    pub message: String,
}

/// The call store's request (docs 10.7, "Live results" 11): the user's calls.
pub fn call_store(mri: &str) -> HttpRequest {
    let url = format!("https://{SERVICE_HOST}/api/v2/ep/api.userstore.skype.com/api/v1/user/{mri}/conversations");
    HttpRequest { method: "GET".to_string(), url, headers: Vec::new(), body: Vec::new() }
}

/// The call store item of the meeting chat `thread`, if the user is in that meeting ("Live
/// results" 18: an item's `groupContext.threadId`). A matching item whose URL the module would not
/// send a token to is an error.
pub fn live_call(body: &[u8], thread: &str) -> Result<Option<Live>, ProtoError> {
    let v = parse_json(body)?;
    let items = v.as_array().ok_or_else(|| incompatible("$"))?;
    let Some(item) = items.iter().find(|item| item.pointer("/groupContext/threadId").and_then(Value::as_str) == Some(thread)) else {
        return Ok(None);
    };
    let controller = item.get("conversationController").and_then(Value::as_str).filter(|url| on_service(url));
    let controller = controller.ok_or_else(|| incompatible("conversationController"))?.to_string();
    let meeting_info = item.get("meetingInfo").filter(|info| info.is_object()).cloned().unwrap_or(Value::Null);
    let message = item.pointer("/groupContext/messageId").and_then(Value::as_str).unwrap_or("0").to_string();
    Ok(Some(Live { controller, meeting_info, message }))
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

/// The join (`POST <controller>`): the probe's body that gave captions with a data-only offer
/// ("Live results" 15 and 18) - no `payload` envelope, muted, no call modalities.
pub fn join(me: &Me<'_>, links: &Links<'_>, live: &Live, thread: &str, offer: &str, media_leg: &str) -> Value {
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
        "groupChat": {"threadId": thread, "messageId": live.message},
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

/// The `leave` link of a conversation (the join's answer or a read of the conversation, docs
/// 10.7), and the roster in it, if it has one.
pub fn conversation(body: &[u8], me: &str, endpoint: &str) -> Result<(String, Option<Roster>), ProtoError> {
    let v = parse_json(body)?;
    let leave = v.pointer("/links/leave").and_then(Value::as_str).filter(|url| on_service(url));
    let leave = leave.ok_or_else(|| incompatible("links.leave"))?.to_string();
    let roster = match v.get("roster") {
        Some(roster) => Some(read_roster(roster, me, endpoint)?),
        None => None,
    };
    Ok((leave, roster))
}

/// The roster of a conversation (`roster.participants`, keyed by MRI; "Live results" 15-18): the
/// user's entry lists its endpoints by id, and the captions bot (a `28:` MRI) has
/// `processingModes.closedCaptions.state` `Active` while captions are on (docs 10.7).
fn read_roster(roster: &Value, me: &str, endpoint: &str) -> Result<Roster, ProtoError> {
    let participants = roster.get("participants").and_then(Value::as_object).ok_or_else(|| incompatible("roster.participants"))?;
    let mut user_elsewhere = false;
    let mut captions = false;
    for (mri, entry) in participants {
        if mri.eq_ignore_ascii_case(me) {
            let endpoints = entry.get("endpoints").and_then(Value::as_object).ok_or_else(|| incompatible("roster.participants.endpoints"))?;
            user_elsewhere |= endpoints.keys().any(|id| !id.eq_ignore_ascii_case(endpoint));
        } else if mri.starts_with("28:") {
            captions |= captions_active(entry, 0);
        }
    }
    Ok(Roster { user_elsewhere, captions })
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
    /// `/conversation/rosterUpdate/`: the roster changed; the module reads it whole.
    Roster,
    /// `/call/end/` or `/conversation/conversationEnd/`.
    Ended,
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
            let answer = v
                .pointer("/callAcceptance/mediaContent/blob")
                .and_then(Value::as_str)
                .ok_or_else(|| incompatible("callAcceptance.mediaContent.blob"))?;
            let acknowledgement =
                v.pointer("/callAcceptance/links/acknowledgement").and_then(Value::as_str).filter(|url| on_service(url)).map(str::to_string);
            Ok(Call::Acceptance { answer: answer.to_string(), acknowledgement })
        }
        "/conversation/rosterUpdate/" => Ok(Call::Roster),
        "/call/end/" | "/conversation/conversationEnd/" => Ok(Call::Ended),
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

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "8:orgid:00000000-0000-0000-0000-000000000001";
    const ENDPOINT: &str = "11111111-1111-4111-8111-111111111111";
    const THREAD: &str = "19:meeting_c3ludGhldGlj@thread.v2";
    const CONTROLLER: &str = "https://api.flightproxy.teams.microsoft.com/api/v2/ep/conv.example/conv/abc";

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

    fn caption(text: &str, last: bool) -> Vec<u8> {
        let results = json!({"recognitionResults": [
            {"isFinal": false, "text": "partial", "displayName": "Speaker"},
            {"isFinal": last, "text": text, "displayName": "Speaker", "spokenLanguage": "pl-pl", "userId": "8:orgid:x"},
        ], "events": [], "version": 1});
        packet(true, 3, 1, 0, results.to_string().as_bytes())
    }

    fn live(now: i64) -> Session {
        let mut s = Session::new(THREAD, "dG9rZW4", now);
        s.answered();
        s.channel_open(now);
        assert_eq!(s.roster(&Roster { user_elsewhere: true, captions: true }, now), None);
        assert_eq!(s.due(now + CAPTIONS_WAIT_MS), None);
        s
    }

    #[test]
    fn final_captions_are_paged_with_cursor_and_language() {
        let mut s = live(0);
        assert_eq!(s.data(&[caption("one", true), caption("not yet", false), caption("two", true)], 0, 1_800_000_000_000), None);
        let page = s.read(None, None, 65536, 1).unwrap();
        assert_eq!(page["state"], "live");
        assert_eq!(page["captions_on"], true);
        assert_eq!(page["next_cursor"], 2);
        assert_eq!(page["language"], "pl-pl");
        assert_eq!(page["reason"], Value::Null);
        assert_eq!(
            page["untrusted"]["cues"],
            json!([
                {"at": "2027-01-15T08:00:00Z", "speaker": "Speaker", "text": "one"},
                {"at": "2027-01-15T08:00:00Z", "speaker": "Speaker", "text": "two"},
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
        assert_eq!(page["untrusted"]["cues"][0]["text"], "joined");
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
    fn the_oldest_cues_go_and_unread_ones_count_as_missed() {
        let mut buffer = Buffer::default();
        let cue = |n: usize| Cue { at_ms: 0, speaker: None, text: n.to_string(), language: None, bytes: 1 };
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
        let (items, next, _) = buffer.page(0, 100);
        assert_eq!((items[0]["text"].as_str(), next), (Some("10"), 10 + items.len() as u64));
        // Bytes too: a cue over the space left pushes out old ones.
        let mut buffer = Buffer::default();
        buffer.push(Cue { bytes: MAX_BUFFER_BYTES - 10, ..cue(0) });
        buffer.push(Cue { bytes: 20, ..cue(1) });
        assert_eq!((buffer.first, buffer.bytes), (1, 20));
    }

    #[test]
    fn deadlines_end_the_session_with_their_reasons() {
        // No answer within the join budget, then no data channel after it.
        let mut s = Session::new(THREAD, "t", 0);
        assert_eq!((s.due(JOIN_MS - 1), s.due(JOIN_MS)), (None, Some(Due::End(Reason::Protocol))));
        let mut s = Session::new(THREAD, "t", 0);
        s.answered();
        assert_eq!(s.due(JOIN_MS), Some(Due::End(Reason::MediaFailed)));
        // Nobody turned captions on within 30 s of the data channel opening.
        let mut s = Session::new(THREAD, "t", 0);
        s.channel_open(1000);
        s.roster(&Roster { user_elsewhere: true, captions: false }, 1000);
        assert_eq!(s.next_at(), Some(1000 + CAPTIONS_WAIT_MS));
        assert_eq!(s.due(1000 + CAPTIONS_WAIT_MS), Some(Due::End(Reason::NoCaptions)));
        // On when joining but off before the 30 s were up: the end at 30 s (Codex review of 96070fd).
        let mut s = Session::new(THREAD, "t", 0);
        s.roster(&Roster { user_elsewhere: true, captions: true }, 0);
        s.channel_open(1000);
        s.roster(&Roster { user_elsewhere: true, captions: false }, 2000);
        assert_eq!(s.due(1000 + CAPTIONS_WAIT_MS), Some(Due::End(Reason::NoCaptions)));
        // Captions turned off later in the session: 5 minutes, then the end.
        let mut s = live(0);
        s.roster(&Roster { user_elsewhere: true, captions: false }, 40_000);
        assert_eq!(s.read(None, None, 4096, 40_000).unwrap()["captions_on"], false);
        assert_eq!(s.next_at(), Some(40_000 + CAPTIONS_OFF_MS));
        assert_eq!(s.due(40_000 + CAPTIONS_OFF_MS - 1), None);
        s.roster(&Roster { user_elsewhere: true, captions: true }, 200_000);
        assert_eq!(s.due(40_000 + CAPTIONS_OFF_MS), None);
        let mut s = live(0);
        s.roster(&Roster { user_elsewhere: true, captions: false }, 40_000);
        assert_eq!(s.due(40_000 + CAPTIONS_OFF_MS), Some(Due::End(Reason::NoCaptions)));
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
    fn the_user_must_be_in_the_meeting_on_another_endpoint() {
        let mut s = Session::new(THREAD, "t", 0);
        assert_eq!(s.roster(&Roster { user_elsewhere: false, captions: true }, 0), Some(Reason::UserNotInMeeting));
        let mut s = live(0);
        assert_eq!(s.roster(&Roster { user_elsewhere: false, captions: true }, 5), Some(Reason::UserLeft));
        s.end(Reason::UserLeft, 5);
        assert_eq!(s.state(), State::Ended);
        s.end(Reason::NoCaptions, 6);
        let mut failed = Session::new(THREAD, "t", 0);
        failed.end(Reason::UserNotInMeeting, 1);
        assert_eq!(failed.state(), State::Failed);
    }

    #[test]
    fn while_trouter_is_down_the_roster_is_read_every_10_s() {
        let mut s = live(0);
        s.down(1000);
        // A read that finds the user keeps the reads going and the captions out (Codex review of 96070fd).
        assert_eq!(s.roster(&Roster { user_elsewhere: true, captions: true }, 11_000), None);
        assert_eq!(s.due(21_000), Some(Due::Roster));
        s.data(&[caption("lost", true)], 0, 21_000);
        assert_eq!(s.read(None, None, 4096, 21_000).unwrap()["missed"], 1);
        assert_eq!(s.roster(&Roster { user_elsewhere: false, captions: true }, 21_000), Some(Reason::UserLeft));
        // Captions back on during the gap: the 5 minutes of captions off stop counting.
        let mut s = live(0);
        s.roster(&Roster { user_elsewhere: true, captions: false }, 40_000);
        s.down(50_000);
        s.roster(&Roster { user_elsewhere: true, captions: true }, 60_000);
        assert_eq!(s.due(40_000 + CAPTIONS_OFF_MS), Some(Due::Roster));
        assert_eq!(s.read(None, None, 4096, 60_000).unwrap()["captions_on"], Value::Null);
        // Back up: the next read lets captions in again.
        let mut s = live(0);
        s.down(1000);
        s.up();
        assert_eq!(s.roster(&Roster { user_elsewhere: true, captions: true }, 2000), None);
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
        assert_eq!(s.roster(&Roster { user_elsewhere: false, captions: true }, 21_000), Some(Reason::UserLeft));
        let mut s = live(0);
        s.pause(1000);
        s.roster(&Roster { user_elsewhere: true, captions: true }, 2000);
        s.data(&[caption("kept", true)], 0, 2000);
        assert_eq!(s.read(None, None, 4096, 2000).unwrap()["untrusted"]["cues"][0]["text"], "kept");
        assert_eq!(s.due(60_000), None);
    }

    #[test]
    fn the_call_store_gives_the_meetings_call_on_the_service_host_only() {
        let item = |url: &str, thread: &str| {
            json!({"conversationController": url, "correlationId": "c", "groupContext": {"messageId": "1700000000000", "threadId": thread},
                   "meetingInfo": {"organizerId": "o", "tenantId": "t"}, "userJoinTime": "x"})
        };
        let body = json!([item(CONTROLLER, "19:other@thread.v2"), item(CONTROLLER, THREAD)]).to_string();
        let live = live_call(body.as_bytes(), THREAD).unwrap().unwrap();
        assert_eq!((live.controller.as_str(), live.message.as_str()), (CONTROLLER, "1700000000000"));
        assert_eq!(live.meeting_info, json!({"organizerId": "o", "tenantId": "t"}));
        assert_eq!(live_call(b"[]", THREAD), Ok(None));
        assert_eq!(live_call(br#"{"conversations": []}"#, THREAD), Err(incompatible("$")));
        for url in [
            "http://api.flightproxy.teams.microsoft.com/x",
            "https://evil.example/x",
            "https://u@api.flightproxy.teams.microsoft.com/x",
            "https://api.flightproxy.teams.microsoft.com:8443/x",
        ] {
            let body = json!([item(url, THREAD)]).to_string();
            assert_eq!(live_call(body.as_bytes(), THREAD), Err(incompatible("conversationController")), "{url}");
        }
    }

    fn me() -> Me<'static> {
        Me { mri: ME, name: "Synthetic Person One", endpoint: ENDPOINT, participant: "p" }
    }

    #[test]
    fn the_join_is_the_probes_data_only_body() {
        let live = Live { controller: CONTROLLER.to_string(), meeting_info: json!({"organizerId": "o"}), message: "0".to_string() };
        let links = Links { surl: "https://trouter.example/v4/f/abc/", session: "s1", tag: 0xffff_fffe };
        let body = join(&me(), &links, &live, THREAD, "v=0\r\n", "leg");
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
            join(&me(), &no_slash, &live, THREAD, "", "leg")["conversationRequest"]["links"]["conversationEnd"]
                .as_str()
                .is_some_and(|l| l.starts_with("https://trouter.example/v4/f/abc/callAgent/s1/"))
        );
    }

    #[test]
    fn the_roster_tells_the_users_other_endpoint_and_the_captions_bot() {
        let conversation = |endpoints: Value, bot: &str| {
            json!({"links": {"leave": format!("{CONTROLLER}/leave")}, "roster": {"participants": {
                ME.to_uppercase(): {"state": "active", "endpoints": endpoints},
                "8:orgid:someone-else": {"endpoints": {"x": {}}},
                "28:bot": {"endpoints": {"b": {"endpointMetadata": {"processingModes": {"closedCaptions": {"state": bot}}}}}},
            }}})
            .to_string()
        };
        let both = conversation(json!({ENDPOINT: {}, "official": {}}), "Active");
        let (leave, roster) = super::conversation(both.as_bytes(), ME, ENDPOINT).unwrap();
        assert_eq!(leave, format!("{CONTROLLER}/leave"));
        assert_eq!(roster, Some(Roster { user_elsewhere: true, captions: true }));
        let alone = conversation(json!({ENDPOINT: {}}), "Inactive");
        assert_eq!(super::conversation(alone.as_bytes(), ME, ENDPOINT).unwrap().1, Some(Roster { user_elsewhere: false, captions: false }));
        let no_roster = json!({"links": {"leave": format!("{CONTROLLER}/leave")}}).to_string();
        assert_eq!(super::conversation(no_roster.as_bytes(), ME, ENDPOINT).unwrap().1, None);
        let foreign = json!({"links": {"leave": "https://evil.example/leave"}}).to_string();
        assert_eq!(super::conversation(foreign.as_bytes(), ME, ENDPOINT), Err(incompatible("links.leave")));
        let listed = conversation(json!([ENDPOINT]), "Active");
        assert!(super::conversation(listed.as_bytes(), ME, ENDPOINT).is_err());
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
        assert!(callback(&format!("{base}/call/acceptance/"), "{}", "s1").is_err());
        assert_eq!(callback(&format!("{base}/conversation/rosterUpdate/"), "{}", "s1"), Ok(Call::Roster));
        assert_eq!(callback(&format!("{base}/call/end/"), "", "s1"), Ok(Call::Ended));
        assert_eq!(callback(&format!("{base}/conversation/conversationEnd/"), "", "s1"), Ok(Call::Ended));
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
