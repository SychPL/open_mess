//! New-message events by polling the conversation list (docs/protocols/teams.md 3.2): each
//! conversation whose last message is new reports one `message-arrived`. Read-only: nothing here
//! touches read state. What was reported is kept in module storage, so a restarted instance does
//! not report it again. Trouter (live events) needs live checks first (F2b).

use serde_json::{Value, json};

use crate::chat::{Conversation, ConversationKind};

/// The host timer that runs the poll, and its interval.
pub const TIMER: u32 = 1;
pub const INTERVAL_MS: u32 = 60_000;
/// Each poll looks back this far past the previous one, so a message that arrived while a poll ran
/// is not missed; the ids already reported keep it from being reported twice.
const OVERLAP_MS: i64 = 5 * 60_000;
/// Conversations whose last reported message is remembered; the least recent go first.
const MAX_SEEN: usize = 1000;

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
    /// Conversation and the id of its last message seen, least recent first.
    seen: Vec<(String, String)>,
}

impl PollState {
    /// An account polled for the first time: messages from before the last interval are old.
    pub fn new(now_ms: i64) -> PollState {
        PollState { floor_ms: now_ms.saturating_sub(i64::from(INTERVAL_MS)), seen: Vec::new() }
    }

    /// The stored state; `None` when there is none or it does not read.
    pub fn decode(bytes: &[u8]) -> Option<PollState> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        let seen = v.get("seen")?.as_array()?.iter().map(|pair| Some((pair.get(0)?.as_str()?.to_string(), pair.get(1)?.as_str()?.to_string())));
        Some(PollState { floor_ms: v.get("floor")?.as_i64()?, seen: seen.collect::<Option<_>>()? })
    }

    pub fn encode(&self) -> Vec<u8> {
        json!({"floor": self.floor_ms, "seen": self.seen}).to_string().into_bytes()
    }

    /// The list's `startTime`: conversations updated since then.
    pub fn start_ms(&self) -> i64 {
        self.floor_ms
    }

    /// The conversation's last message, if it is new; it is remembered either way.
    pub fn arrival(&mut self, c: &Conversation, me: &str) -> Option<Arrival> {
        let m = c.last_message.as_ref()?;
        if self.seen.iter().any(|(conversation, id)| *conversation == c.id && *id == m.id) {
            return None;
        }
        self.seen.retain(|(conversation, _)| *conversation != c.id);
        self.seen.push((c.id.clone(), m.id.clone()));
        if self.seen.len() > MAX_SEEN {
            self.seen.remove(0);
        }
        (m.sent_ms > self.floor_ms).then(|| Arrival {
            conversation: c.id.clone(),
            message: m.id.clone(),
            sender: m.sender_id.clone(),
            own: m.sender_id == me,
            mentions_me: m.mentions.iter().any(|mention| mention.mri == me),
            direct: c.kind == ConversationKind::Direct,
        })
    }

    /// After a poll that started at `started_ms`.
    pub fn finish(&mut self, started_ms: i64) {
        self.floor_ms = self.floor_ms.max(started_ms.saturating_sub(OVERLAP_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::decode_conversations;

    const ME: &str = "8:orgid:00000000-0000-0000-0000-000000000001";
    const NOW: i64 = 1_800_000_000_000;

    /// A conversation whose last message `id` came from `from` at `at`, mentioning `mention`.
    fn conversation(conv: &str, id: &str, from: &str, at: i64, mention: Option<&str>) -> Conversation {
        let mentions = mention.map(|mri| json!([{"mri": mri}]).to_string());
        let last = json!({"id": id, "conversationid": conv, "from": from, "messagetype": "RichText/Html", "content": "<p>x</p>",
                          "originalarrivaltime": at, "properties": {"mentions": mentions}});
        let body = json!({"conversations": [{"id": conv, "lastMessage": last}], "_metadata": {}});
        decode_conversations(body.to_string().as_bytes()).unwrap().items.remove(0)
    }

    #[test]
    fn new_messages_arrive_once() {
        let mut state = PollState::new(NOW);
        let c = conversation("19:a@thread.v2", "1", "8:orgid:2", NOW + 1, None);
        let a = state.arrival(&c, ME).unwrap();
        assert_eq!((a.conversation.as_str(), a.message.as_str(), a.sender.as_str()), ("19:a@thread.v2", "1", "8:orgid:2"));
        assert!(!a.own && !a.mentions_me && !a.direct);
        assert!(state.arrival(&c, ME).is_none());
        // The next message in the same conversation is new again.
        assert!(state.arrival(&conversation("19:a@thread.v2", "2", "8:orgid:2", NOW + 2, None), ME).is_some());
    }

    #[test]
    fn old_messages_are_remembered_not_reported() {
        let mut state = PollState::new(NOW);
        let old = conversation("19:a@thread.v2", "1", "8:orgid:2", NOW - i64::from(INTERVAL_MS) - 1, None);
        assert!(state.arrival(&old, ME).is_none());
        assert_eq!(state.seen, [("19:a@thread.v2".to_string(), "1".to_string())]);
    }

    #[test]
    fn flags_follow_the_message() {
        let mut state = PollState::new(NOW);
        let direct = "19:00000000-0000-0000-0000-000000000001_00000000-0000-0000-0000-000000000002@unq.gbl.spaces";
        let mine = state.arrival(&conversation(direct, "1", ME, NOW + 1, None), ME).unwrap();
        assert!(mine.own && mine.direct && !mine.mentions_me);
        let mention = state.arrival(&conversation("19:b@thread.v2", "2", "8:orgid:2", NOW + 1, Some(ME)), ME).unwrap();
        assert!(mention.mentions_me && !mention.direct && !mention.own);
    }

    #[test]
    fn the_floor_keeps_an_overlap() {
        let mut state = PollState::new(NOW);
        state.finish(NOW + 600_000);
        assert_eq!(state.start_ms(), NOW + 600_000 - OVERLAP_MS);
        // Never back.
        state.finish(NOW);
        assert_eq!(state.start_ms(), NOW + 600_000 - OVERLAP_MS);
    }

    #[test]
    fn state_survives_storage_and_stays_bounded() {
        let mut state = PollState::new(NOW);
        for i in 0..=MAX_SEEN {
            state.arrival(&conversation(&format!("19:{i}@thread.v2"), "1", "8:orgid:2", NOW + 1, None), ME);
        }
        assert_eq!(state.seen.len(), MAX_SEEN);
        assert_eq!(state.seen[0].0, "19:1@thread.v2");
        assert_eq!(PollState::decode(&state.encode()), Some(state));
        for bad in [&b"not json"[..], br#"{"floor": 1}"#, br#"{"floor": 1, "seen": [[1, 2]]}"#] {
            assert_eq!(PollState::decode(bad), None);
        }
    }
}
