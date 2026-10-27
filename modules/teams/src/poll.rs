//! New-message events by polling (docs/protocols/teams.md 3.2): each poll lists the conversations
//! updated since the previous one; a conversation whose last item (a message, or a call or other
//! event) is new has its messages read back to the last one seen, and each new message reports one
//! `message-arrived`, so a mention followed by another message is not lost. More new pages than a
//! poll reads are read on by the next polls. A conversation whose messages cannot be read this time
//! holds the floor, so the next poll tries it again without the others waiting. Read-only:
//! nothing here touches read state. What was reported is kept in module storage, so a restarted
//! instance does not report it again. Trouter (live events) needs live checks first (F2b).

use serde_json::{Value, json};

use crate::chat::{self, Conversation, ConversationKind, Message};

/// The host timer that runs the poll, and its interval.
pub const TIMER: u32 = 1;
pub const INTERVAL_MS: u32 = 60_000;
/// Each poll looks back this far past the previous one, so a message that arrived while a poll ran
/// is not missed; the ids already reported keep it from being reported twice.
const OVERLAP_MS: i64 = 5 * 60_000;
/// Conversations whose last reported message is remembered; the least recent go first.
const MAX_SEEN: usize = 1000;
/// How long a conversation that cannot be read may hold the floor; past that its last message
/// stands for what could not be read, so one broken conversation cannot make every poll list
/// ever more conversations.
const MAX_HOLD_MS: i64 = 60 * 60_000;
/// Conversations whose older pages wait for the next polls; past it the oldest is given up.
const MAX_BEHIND: usize = 20;

/// Where an account's state is stored.
pub fn key(account: &str) -> String {
    format!("poll/{account}")
}

/// One new message, for `host.message-arrived`. No `Debug`: it carries ids.
pub struct Arrival {
    pub conversation: String,
    pub message: String,
    /// The sender's MRI.
    pub sender: String,
    pub own: bool,
    pub mentions_me: bool,
    pub direct: bool,
}

#[derive(Debug, PartialEq)]
pub struct PollState {
    /// Messages that arrived at or before this time are never reported.
    floor_ms: i64,
    /// Conversation, and the id and time of its last message seen, least recent first.
    seen: Vec<(String, String, i64)>,
    /// When a conversation that could not be read began to hold the floor.
    held_since_ms: Option<i64>,
    /// Older pages of conversations that had more new messages than one poll reads.
    behind: Vec<Behind>,
}

/// The older pages of one conversation still to read: from `link` on, back to the first message
/// that was not new when the conversation fell behind.
#[derive(Debug, PartialEq)]
pub struct Behind {
    pub conversation: String,
    /// The chat service's `backwardLink` to the next page.
    pub link: String,
    floor_ms: i64,
    /// The message seen last in the conversation then, if any.
    seen: Option<(String, i64)>,
    since_ms: i64,
}

impl Behind {
    /// Whether `m` was new when the conversation fell behind.
    pub fn fresh(&self, m: &Message) -> bool {
        m.sent_ms > self.floor_ms && self.seen.as_ref().is_none_or(|(id, ms)| m.id != *id && m.sent_ms >= *ms)
    }

    fn encode(&self) -> Value {
        json!({"conversation": self.conversation, "link": self.link, "floor": self.floor_ms, "seen": self.seen, "since": self.since_ms})
    }

    fn decode(v: &Value) -> Option<Behind> {
        let seen = match v.get("seen")? {
            Value::Null => None,
            seen => Some((seen.get(0)?.as_str()?.to_string(), seen.get(1)?.as_i64()?)),
        };
        Some(Behind {
            conversation: v.get("conversation")?.as_str()?.to_string(),
            link: v.get("link")?.as_str()?.to_string(),
            floor_ms: v.get("floor")?.as_i64()?,
            seen,
            since_ms: v.get("since")?.as_i64()?,
        })
    }
}

fn arrival(conversation: &str, m: &Message, direct: bool, me: &str) -> Arrival {
    Arrival {
        conversation: conversation.to_string(),
        message: m.id.clone(),
        sender: m.sender_id.clone(),
        own: m.sender_id == me,
        mentions_me: m.mentions.iter().any(|mention| mention.mri == me),
        direct,
    }
}

impl PollState {
    /// An account whose messages count from `now_ms` on: when it signs in or is restored.
    pub fn new(now_ms: i64) -> PollState {
        PollState { floor_ms: now_ms, seen: Vec::new(), held_since_ms: None, behind: Vec::new() }
    }

    /// The stored state; `None` when there is none or it does not read.
    pub fn decode(bytes: &[u8]) -> Option<PollState> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        // An entry without the message's time (as first stored) compares by id alone.
        let time = |e: &Value| e.get(2).map_or(Some(0), Value::as_i64);
        let entry = |e: &Value| Some((e.get(0)?.as_str()?.to_string(), e.get(1)?.as_str()?.to_string(), time(e)?));
        let seen = v.get("seen")?.as_array()?.iter().map(entry).collect::<Option<_>>()?;
        let held_since_ms = match v.get("held") {
            None | Some(Value::Null) => None,
            Some(ms) => Some(ms.as_i64()?),
        };
        // Stored before there were older pages to keep: none.
        let behind = match v.get("behind") {
            None => Vec::new(),
            Some(list) => list.as_array()?.iter().map(Behind::decode).collect::<Option<_>>()?,
        };
        Some(PollState { floor_ms: v.get("floor")?.as_i64()?, seen, held_since_ms, behind })
    }

    pub fn encode(&self) -> Vec<u8> {
        let behind: Vec<Value> = self.behind.iter().map(Behind::encode).collect();
        json!({"floor": self.floor_ms, "seen": self.seen, "held": self.held_since_ms, "behind": behind}).to_string().into_bytes()
    }

    /// The list's `startTime`: conversations updated since then.
    pub fn start_ms(&self) -> i64 {
        self.floor_ms
    }

    fn seen(&self, conversation: &str) -> Option<(&str, i64)> {
        self.seen.iter().find(|(c, ..)| c == conversation).map(|(_, id, ms)| (id.as_str(), *ms))
    }

    /// Whether the item `id` of `conversation`, sent at `ms`, is new: after the floor, and neither
    /// the item seen last there nor older than it (a list that lags behind the history shows an
    /// older one).
    fn fresh_item(&self, conversation: &str, id: &str, ms: i64) -> bool {
        ms > self.floor_ms && self.seen(conversation).is_none_or(|(seen, seen_ms)| id != seen && ms >= seen_ms)
    }

    /// Whether `m` of `conversation` is new.
    pub fn fresh(&self, conversation: &str, m: &Message) -> bool {
        self.fresh_item(conversation, &m.id, m.sent_ms)
    }

    /// Whether the conversation's last item is new, a text message or not: a call or a member
    /// added after a mention must not hide the mention.
    pub fn changed(&self, c: &Conversation) -> bool {
        match (&c.last_message_id, c.horizons.last_message_ms) {
            (Some(id), Some(ms)) => self.fresh_item(&c.id, id, ms),
            _ => false,
        }
    }

    /// The arrivals of a changed conversation, oldest first, from its new messages (newest first,
    /// read until the first one not `fresh`); `None` when they may not be read, and then the
    /// list's last message stands for them. Remembered is the newest message read: the history may
    /// be ahead of the list, if a message came while the poll ran, or behind it, and then the next
    /// poll reads again. A last item that is no text message (a call) is remembered when it is
    /// newer, since no history shows it. ponytail: a text message the history shows only after
    /// such an item, while the list already has it, is then missed; remembering both needs a
    /// second id per conversation.
    pub fn arrivals(&mut self, c: &Conversation, new_newest_first: Option<&[Message]>, me: &str) -> Vec<Arrival> {
        // An empty answer did not show the new last message: the list's word stands.
        let messages: Vec<&Message> = match new_newest_first {
            Some(new) if !new.is_empty() => new.iter().collect(),
            _ => c.last_message.iter().collect(),
        };
        let read = messages.first().map(|m| (m.id.clone(), m.sent_ms));
        let event = c.last_message_id.clone().zip(c.horizons.last_message_ms).filter(|_| c.last_message.is_none());
        let newest = match (read, event) {
            (Some(read), Some(event)) if event.1 > read.1 => Some(event),
            (read, event) => read.or(event),
        };
        let direct = c.kind == ConversationKind::Direct;
        let mut arrivals: Vec<Arrival> = messages.into_iter().filter(|m| !m.deleted).map(|m| arrival(&c.id, m, direct, me)).collect();
        arrivals.reverse();
        if let Some((id, ms)) = newest {
            self.seen.retain(|(conversation, ..)| *conversation != c.id);
            self.seen.push((c.id.clone(), id, ms));
            if self.seen.len() > MAX_SEEN {
                self.seen.remove(0);
            }
        }
        arrivals
    }

    /// Notes that a conversation had more new messages than one poll reads: the pages from `link`
    /// on are read by the next polls, back to where this one would have stopped. Call it before
    /// [`PollState::arrivals`] moves what was seen there. False when that pushed out the oldest
    /// conversation that fell behind, whose older messages are then not reported.
    pub fn fall_behind(&mut self, conversation: &str, link: String, now_ms: i64) -> bool {
        let seen = self.seen(conversation).map(|(id, ms)| (id.to_string(), ms));
        self.behind.push(Behind { conversation: conversation.to_string(), link, floor_ms: self.floor_ms, seen, since_ms: now_ms });
        if self.behind.len() > MAX_BEHIND {
            self.behind.remove(0);
            return false;
        }
        true
    }

    /// The older pages to read in this poll; [`PollState::keep_behind`] puts back what is left.
    pub fn take_behind(&mut self) -> Vec<Behind> {
        std::mem::take(&mut self.behind)
    }

    /// Older pages to read in a later poll: what is left of them, or the same after a failure,
    /// for an hour from when the conversation fell behind. False when that hour is over and they
    /// are given up.
    pub fn keep_behind(&mut self, b: Behind, now_ms: i64) -> bool {
        if now_ms.saturating_sub(b.since_ms) >= MAX_HOLD_MS {
            return false;
        }
        self.behind.push(b);
        true
    }

    /// The arrivals of older messages read for `b` (newest first), oldest first.
    pub fn older_arrivals(b: &Behind, older_newest_first: &[Message], me: &str) -> Vec<Arrival> {
        let direct = chat::kind(&b.conversation) == ConversationKind::Direct;
        older_newest_first.iter().rev().filter(|m| !m.deleted).map(|m| arrival(&b.conversation, m, direct, me)).collect()
    }

    /// Whether a conversation that could not be read at `now_ms` may hold the floor for the next
    /// poll: for an hour from when holding began, however old the floor (after a sleep, say).
    pub fn can_hold(&self, now_ms: i64) -> bool {
        self.held_since_ms.is_none_or(|since| now_ms.saturating_sub(since) < MAX_HOLD_MS)
    }

    /// After a poll that started at `started_ms`; `held` when a conversation holds the floor.
    pub fn finish(&mut self, started_ms: i64, held: bool) {
        if held {
            self.held_since_ms.get_or_insert(started_ms);
        } else {
            self.held_since_ms = None;
            self.floor_ms = self.floor_ms.max(started_ms.saturating_sub(OVERLAP_MS));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{decode_conversations, decode_messages};

    const ME: &str = "8:orgid:00000000-0000-0000-0000-000000000001";
    const OTHER: &str = "8:orgid:00000000-0000-0000-0000-000000000002";
    const NOW: i64 = 1_800_000_000_000;

    fn message(conv: &str, id: &str, from: &str, at: i64, mention: Option<&str>) -> Value {
        let mentions = mention.map(|mri| json!([{"mri": mri}]).to_string());
        json!({"id": id, "conversationid": conv, "from": from, "messagetype": "RichText/Html", "content": "<p>x</p>",
               "originalarrivaltime": at, "properties": {"mentions": mentions}})
    }

    /// A conversation whose last message is `last`.
    fn conversation(conv: &str, last: Value) -> Conversation {
        let body = json!({"conversations": [{"id": conv, "lastMessage": last}], "_metadata": {}});
        decode_conversations(body.to_string().as_bytes()).unwrap().items.remove(0)
    }

    fn page(messages: Vec<Value>) -> Vec<Message> {
        decode_messages(json!({"messages": messages, "_metadata": {}}).to_string().as_bytes()).unwrap().items
    }

    /// What `tools::poll` reads: messages newest first, until the first one not fresh.
    fn new(state: &PollState, conv: &str, newest_first: Vec<Value>) -> Vec<Message> {
        page(newest_first).into_iter().take_while(|m| state.fresh(conv, m)).collect()
    }

    #[test]
    fn every_new_message_arrives_once_oldest_first() {
        let mut state = PollState::new(NOW);
        let mention = message("19:a@thread.v2", "2", OTHER, NOW + 2, Some(ME));
        let plain = message("19:a@thread.v2", "3", OTHER, NOW + 3, None);
        let old = message("19:a@thread.v2", "1", OTHER, NOW - 1, Some(ME));
        let c = conversation("19:a@thread.v2", plain.clone());
        assert!(state.changed(&c));
        let fresh = new(&state, "19:a@thread.v2", vec![plain, mention, old]);
        let arrivals = state.arrivals(&c, Some(&fresh), ME);
        let got: Vec<_> = arrivals.iter().map(|a| (a.message.as_str(), a.mentions_me)).collect();
        // The mention before the last message is not lost; the one before the floor is old.
        assert_eq!(got, [("2", true), ("3", false)]);
        assert!(!state.changed(&c));
    }

    #[test]
    fn reading_stops_at_the_message_seen() {
        let mut state = PollState::new(NOW);
        let first = message("19:a@thread.v2", "2", OTHER, NOW + 2, None);
        let c = conversation("19:a@thread.v2", first.clone());
        let fresh = new(&state, "19:a@thread.v2", vec![first.clone()]);
        state.arrivals(&c, Some(&fresh), ME);
        let second = message("19:a@thread.v2", "3", ME, NOW + 3, None);
        let fresh = new(&state, "19:a@thread.v2", vec![second.clone(), first]);
        let arrivals = state.arrivals(&conversation("19:a@thread.v2", second), Some(&fresh), ME);
        assert_eq!(arrivals.iter().map(|a| (a.message.as_str(), a.own)).collect::<Vec<_>>(), [("3", true)]);
    }

    #[test]
    fn a_message_that_came_during_the_poll_is_remembered() {
        // The list showed 2 as the last message; 3 came before the history was read.
        let mut state = PollState::new(NOW);
        let (two, three) = (message("19:a@thread.v2", "2", OTHER, NOW + 2, None), message("19:a@thread.v2", "3", OTHER, NOW + 3, None));
        let fresh = new(&state, "19:a@thread.v2", vec![three.clone(), two.clone()]);
        assert_eq!(state.arrivals(&conversation("19:a@thread.v2", two), Some(&fresh), ME).len(), 2);
        // The next poll's list shows 3: nothing new.
        assert!(!state.changed(&conversation("19:a@thread.v2", three)));
    }

    #[test]
    fn unreadable_messages_leave_the_last_one() {
        let direct = "19:00000000-0000-0000-0000-000000000001_00000000-0000-0000-0000-000000000002@unq.gbl.spaces";
        let c = conversation(direct, message(direct, "5", OTHER, NOW + 5, Some(ME)));
        // Not readable, or read but empty.
        for new in [None, Some(&[][..])] {
            let mut state = PollState::new(NOW);
            let arrivals = state.arrivals(&c, new, ME);
            assert_eq!(arrivals.len(), 1);
            let a = &arrivals[0];
            assert!(a.direct && a.mentions_me && !a.own && a.sender == OTHER && a.message == "5");
        }
    }

    #[test]
    fn a_held_floor_stays_for_an_hour_at_most() {
        let mut state = PollState::new(NOW);
        // After a long sleep the floor is old, but holding has only begun.
        let woke = NOW + 3 * MAX_HOLD_MS;
        assert!(state.can_hold(woke));
        state.finish(woke, true);
        assert_eq!(state.start_ms(), NOW);
        state.finish(woke + 60_000, true);
        assert!(state.can_hold(woke + MAX_HOLD_MS - 1));
        assert!(!state.can_hold(woke + MAX_HOLD_MS));
        // A poll that holds nothing ends the hold.
        state.finish(woke + MAX_HOLD_MS, false);
        assert!(state.can_hold(woke + 10 * MAX_HOLD_MS));
        assert_eq!(state.start_ms(), woke + MAX_HOLD_MS - OVERLAP_MS);
    }

    #[test]
    fn a_list_behind_the_history_reports_nothing_twice() {
        // The list shows 2; the history already has 3, and both are reported.
        let mut state = PollState::new(NOW);
        let (two, three) = (message("19:a@thread.v2", "2", OTHER, NOW + 2, None), message("19:a@thread.v2", "3", OTHER, NOW + 3, None));
        let fresh = new(&state, "19:a@thread.v2", vec![three.clone(), two.clone()]);
        assert_eq!(state.arrivals(&conversation("19:a@thread.v2", two.clone()), Some(&fresh), ME).len(), 2);
        // A minute later the list still shows 2: it is not new, so nothing is read or reported.
        assert!(!state.changed(&conversation("19:a@thread.v2", two)));
        assert!(!state.changed(&conversation("19:a@thread.v2", three)));
    }

    #[test]
    fn a_history_behind_the_list_is_read_again() {
        // The list shows 5 already; the history, read after it, ends at 3.
        let mut state = PollState::new(NOW);
        let conv = "19:a@thread.v2";
        let (three, five) = (message(conv, "3", OTHER, NOW + 3, None), message(conv, "5", OTHER, NOW + 5, Some(ME)));
        let fresh = new(&state, conv, vec![three]);
        let arrivals = state.arrivals(&conversation(conv, five.clone()), Some(&fresh), ME);
        assert_eq!(arrivals.iter().map(|a| a.message.as_str()).collect::<Vec<_>>(), ["3"]);
        // 5 was not read, so it is not taken as seen: the next poll reads it.
        assert!(state.changed(&conversation(conv, five)));
    }

    #[test]
    fn a_call_after_a_mention_does_not_hide_it() {
        let mut state = PollState::new(NOW);
        let call = json!({"id": "3", "conversationid": "19:a@thread.v2", "from": OTHER, "messagetype": "Event/Call",
                          "content": "<partlist/>", "originalarrivaltime": NOW + 3});
        let c = conversation("19:a@thread.v2", call);
        assert!(c.last_message.is_none() && state.changed(&c));
        let fresh = new(&state, "19:a@thread.v2", vec![message("19:a@thread.v2", "2", OTHER, NOW + 2, Some(ME))]);
        let arrivals = state.arrivals(&c, Some(&fresh), ME);
        assert_eq!(arrivals.iter().map(|a| (a.message.as_str(), a.mentions_me)).collect::<Vec<_>>(), [("2", true)]);
        // The call is what was seen last: the next poll finds nothing new there.
        assert!(!state.changed(&c));
        // Not readable: nothing to report, and the call is remembered all the same.
        let mut state = PollState::new(NOW);
        assert!(state.arrivals(&c, None, ME).is_empty());
        assert!(!state.changed(&c));
    }

    #[test]
    fn older_pages_are_read_back_to_where_the_poll_would_have_stopped() {
        let mut state = PollState::new(NOW);
        let conv = "19:a@thread.v2";
        let first = message(conv, "1", OTHER, NOW + 1, None);
        state.arrivals(&conversation(conv, first.clone()), Some(&page(vec![first.clone()])), ME);
        // More new pages than one poll reads: the newest are reported, the rest wait.
        let newest = message(conv, "9", OTHER, NOW + 9, None);
        assert!(state.fall_behind(conv, "https://link/older".into(), NOW + 10));
        state.arrivals(&conversation(conv, newest.clone()), Some(&page(vec![newest])), ME);
        let behind = state.take_behind();
        assert_eq!((behind.len(), behind[0].link.as_str()), (1, "https://link/older"));
        // Back to the message seen before, not the one seen now.
        let older = page(vec![message(conv, "3", OTHER, NOW + 3, Some(ME)), message(conv, "2", OTHER, NOW + 2, None), first.clone()]);
        let fresh: Vec<Message> = older.into_iter().take_while(|m| behind[0].fresh(m)).collect();
        let arrivals = PollState::older_arrivals(&behind[0], &fresh, ME);
        assert_eq!(arrivals.iter().map(|a| (a.message.as_str(), a.mentions_me)).collect::<Vec<_>>(), [("2", false), ("3", true)]);
        // Kept for a later poll, through storage, for an hour.
        let b = behind.into_iter().next().unwrap();
        assert!(state.keep_behind(b, NOW + 10 + MAX_HOLD_MS - 1));
        let mut stored = PollState::decode(&state.encode()).unwrap();
        assert_eq!(stored, state);
        let b = stored.take_behind().pop().unwrap();
        assert!(!stored.keep_behind(b, NOW + 10 + MAX_HOLD_MS));
        // A bounded number of conversations wait.
        for i in 0..=MAX_BEHIND {
            let kept = stored.fall_behind(&format!("19:{i}@thread.v2"), "https://link".into(), NOW);
            assert_eq!(kept, i < MAX_BEHIND);
        }
        assert_eq!(stored.take_behind().len(), MAX_BEHIND);
    }

    #[test]
    fn old_and_unchanged_conversations_are_not_read() {
        let state = PollState::new(NOW);
        assert!(!state.changed(&conversation("19:a@thread.v2", message("19:a@thread.v2", "1", OTHER, NOW, None))));
        assert!(!state.changed(&conversation("19:a@thread.v2", json!({}))));
    }

    #[test]
    fn the_floor_keeps_an_overlap() {
        let mut state = PollState::new(NOW);
        state.finish(NOW + 600_000, false);
        assert_eq!(state.start_ms(), NOW + 600_000 - OVERLAP_MS);
        // Never back.
        state.finish(NOW, false);
        assert_eq!(state.start_ms(), NOW + 600_000 - OVERLAP_MS);
    }

    #[test]
    fn state_survives_storage_and_stays_bounded() {
        let mut state = PollState::new(NOW);
        for i in 0..=MAX_SEEN {
            let conv = format!("19:{i}@thread.v2");
            state.arrivals(&conversation(&conv, message(&conv, "1", OTHER, NOW + 1, None)), None, ME);
        }
        assert_eq!(state.seen.len(), MAX_SEEN);
        assert_eq!(state.seen[0].0, "19:1@thread.v2");
        assert_eq!(PollState::decode(&state.encode()), Some(state));
        let mut held = PollState::new(NOW);
        held.finish(NOW + 1, true);
        assert_eq!(PollState::decode(&held.encode()), Some(held));
        // The first stored form, without message times, still reads.
        let first = PollState::decode(br#"{"floor": 5, "seen": [["19:a@thread.v2", "1"]]}"#).unwrap();
        assert_eq!((first.start_ms(), first.seen("19:a@thread.v2")), (5, Some(("1", 0))));
        for bad in [&b"not json"[..], br#"{"floor": 1}"#, br#"{"floor": 1, "seen": [[1, 2]]}"#, br#"{"floor": 1, "seen": [["a", "b", "c"]]}"#] {
            assert_eq!(PollState::decode(bad), None);
        }
    }
}
