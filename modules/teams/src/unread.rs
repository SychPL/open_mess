//! Whether the user has read a conversation, by the official client's rule
//! (docs/protocols/teams.md 3.4). It only reads horizons; nothing here produces a write.

use crate::chat::{Conversation, Horizon, Horizons};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreadState {
    Read,
    Unread,
    /// The inputs don't settle it; never reported as read.
    Unknown,
}

/// `app_horizon_ms` is `appConsumptionHorizon.chat` for a chat or `.channel` for a channel, `None`
/// when it could not be read; `me` is the user's MRI.
pub fn state(conv: &Conversation, app_horizon_ms: Option<i64>, me: &str) -> UnreadState {
    let h = &conv.horizons;
    let Some(own) = &h.consumption else { return UnreadState::Unknown };
    let verdict = rule(h, own, app_horizon_ms.unwrap_or(0), me);
    // Without the account-wide horizon a "mark all as read" may have cleared what looks unread. A
    // read verdict holds for any account-wide horizon: it only ever raises the effective one.
    if verdict == UnreadState::Unread && app_horizon_ms.is_none() { UnreadState::Unknown } else { verdict }
}

fn rule(h: &Horizons, own: &Horizon, app: i64, me: &str) -> UnreadState {
    // 1. The effective horizon is the later of the conversation's and the account-wide one, except
    //    that the account-wide one is not applied to an unseen chat joined (or a channel created)
    //    after it.
    let newer_than_app = |t: Option<i64>| t.is_some_and(|t| t > app);
    let skip_app = own.arrival_ms == 0 && (newer_than_app(h.joined_ms) || newer_than_app(h.created_ms));
    let horizon = if skip_app { own.arrival_ms } else { own.arrival_ms.max(app) };
    // 2. "Mark as unread" survives a passed horizon, until a later "mark all as read".
    if h.bookmark.as_ref().is_some_and(|b| app < b.timestamp_ms && b.arrival_ms != 0 && b.client_message_id != "0") {
        return UnreadState::Unread;
    }
    // 3. The user's own last message, or a call the user started, is read.
    let is_me = |mri: &str| !me.is_empty() && (mri == me || mri.strip_suffix(me).is_some_and(|rest| rest.ends_with('/')));
    if h.last_message_from.as_deref().is_some_and(is_me) || h.last_call_initiator.as_deref().is_some_and(is_me) {
        return UnreadState::Read;
    }
    // 4. Otherwise unread when the last message arrived after the effective horizon.
    match h.last_message_ms {
        Some(t) if t > horizon => UnreadState::Unread,
        Some(_) => UnreadState::Read,
        None => UnreadState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ConversationKind;

    const ME: &str = "8:orgid:00000000-0000-0000-0000-000000000001";
    const OTHER: &str = "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/contacts/8:orgid:00000000-0000-0000-0000-000000000002";

    fn h(arrival_ms: i64, timestamp_ms: i64, id: &str) -> Option<Horizon> {
        Some(Horizon { arrival_ms, timestamp_ms, client_message_id: id.to_string() })
    }

    /// Own horizon at 1000, last message from someone else at 2000.
    fn base() -> Horizons {
        Horizons {
            consumption: h(1000, 1000, "1"),
            bookmark: None,
            last_message_ms: Some(2000),
            last_message_from: Some(OTHER.to_string()),
            last_call_initiator: None,
            joined_ms: None,
            created_ms: None,
        }
    }

    fn of(horizons: Horizons, app: Option<i64>) -> UnreadState {
        let conv = Conversation { id: "19:x@thread.v2".to_string(), kind: ConversationKind::Group, title: None, last_message_id: None, horizons };
        state(&conv, app, ME)
    }

    #[test]
    fn message_after_the_horizon_is_unread() {
        assert_eq!(of(base(), Some(0)), UnreadState::Unread);
    }

    #[test]
    fn message_before_the_horizon_is_read() {
        assert_eq!(of(Horizons { consumption: h(3000, 3000, "1"), ..base() }, Some(0)), UnreadState::Read);
    }

    #[test]
    fn mark_all_as_read_horizon_wins_over_old_messages() {
        assert_eq!(of(base(), Some(3000)), UnreadState::Read);
    }

    #[test]
    fn app_horizon_is_skipped_for_a_chat_joined_after_it() {
        let joined = Horizons { consumption: h(0, 0, "0"), joined_ms: Some(4000), ..base() };
        assert_eq!(of(joined, Some(3000)), UnreadState::Unread);
        // The exception needs an own horizon of 0.
        let seen = Horizons { joined_ms: Some(4000), ..base() };
        assert_eq!(of(seen, Some(3000)), UnreadState::Read);
    }

    #[test]
    fn app_horizon_is_skipped_for_a_channel_created_after_it() {
        let created = Horizons { consumption: h(0, 0, "0"), created_ms: Some(4000), ..base() };
        assert_eq!(of(created, Some(3000)), UnreadState::Unread);
        let older = Horizons { consumption: h(0, 0, "0"), created_ms: Some(2500), ..base() };
        assert_eq!(of(older, Some(3000)), UnreadState::Read);
    }

    #[test]
    fn bookmark_marks_unread_even_if_horizon_passed() {
        let marked = Horizons { consumption: h(5000, 5000, "1"), bookmark: h(1500, 2500, "7"), ..base() };
        assert_eq!(of(marked, Some(1000)), UnreadState::Unread);
    }

    #[test]
    fn stale_bookmark_older_than_app_horizon_is_read() {
        let marked = Horizons { consumption: h(5000, 5000, "1"), bookmark: h(1500, 2500, "7"), ..base() };
        assert_eq!(of(marked, Some(2500)), UnreadState::Read);
    }

    #[test]
    fn empty_bookmark_does_not_count() {
        for bookmark in [h(0, 2500, "7"), h(1500, 2500, "0")] {
            let marked = Horizons { consumption: h(5000, 5000, "1"), bookmark, ..base() };
            assert_eq!(of(marked, Some(1000)), UnreadState::Read);
        }
    }

    #[test]
    fn own_last_message_is_read() {
        let mine = format!("https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/contacts/{ME}");
        for from in [mine, ME.to_string()] {
            assert_eq!(of(Horizons { last_message_from: Some(from), ..base() }, Some(0)), UnreadState::Read);
        }
        // Another MRI that merely ends with the same characters is not the user.
        let lookalike = format!("https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/contacts/2{ME}");
        assert_eq!(of(Horizons { last_message_from: Some(lookalike), ..base() }, Some(0)), UnreadState::Unread);
    }

    #[test]
    fn own_call_is_read() {
        assert_eq!(of(Horizons { last_call_initiator: Some(ME.to_string()), ..base() }, Some(0)), UnreadState::Read);
    }

    #[test]
    fn empty_user_is_never_the_sender() {
        let conv =
            Conversation { id: "19:x@thread.v2".to_string(), kind: ConversationKind::Group, title: None, last_message_id: None, horizons: base() };
        assert_eq!(state(&conv, Some(0), ""), UnreadState::Unread);
    }

    #[test]
    fn missing_horizon_is_unknown() {
        assert_eq!(of(Horizons { consumption: None, ..base() }, Some(0)), UnreadState::Unknown);
    }

    #[test]
    fn missing_last_message_time_is_unknown() {
        assert_eq!(of(Horizons { last_message_ms: None, ..base() }, Some(0)), UnreadState::Unknown);
    }

    #[test]
    fn missing_app_horizon_leaves_only_read_certain() {
        // A "mark all as read" may have passed what looks unread; a read verdict holds anyway.
        assert_eq!(of(base(), None), UnreadState::Unknown);
        assert_eq!(of(Horizons { consumption: h(3000, 3000, "1"), ..base() }, None), UnreadState::Read);
    }
}
