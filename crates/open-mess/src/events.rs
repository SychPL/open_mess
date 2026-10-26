//! The daemon's event bus: what modules report through the `host` imports (new messages, session
//! changes), with the core's account ids, for whoever subscribes (the triggers of milestone D).
//! Session changes also update the accounts table.

use std::sync::Arc;

use open_mess_host::{HostEvent, SessionState};
use tokio::sync::{broadcast, mpsc};

use crate::accounts::Accounts;

/// Events kept for subscribers; one that falls further behind loses the oldest and gets their
/// count (`RecvError::Lagged`).
pub const CAPACITY: usize = 1024;

#[derive(Clone, PartialEq, Eq)]
pub enum Event {
    /// A new message in a conversation of `account`.
    Message {
        account: String,
        conversation: String,
        message: String,
        own: bool,
        mentions_me: bool,
        direct: bool,
    },
    Session {
        account: String,
        state: SessionState,
    },
}

// Hand-written so that a `{:?}` in a log line never prints conversation or message ids.
impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Message { account, own, mentions_me, direct, .. } => f
                .debug_struct("Message")
                .field("account", account)
                .field("own", own)
                .field("mentions_me", mentions_me)
                .field("direct", direct)
                .finish_non_exhaustive(),
            Event::Session { account, state } => f.debug_struct("Session").field("account", account).field("state", state).finish(),
        }
    }
}

#[derive(Clone)]
pub struct Bus(broadcast::Sender<Event>);

impl Default for Bus {
    fn default() -> Self {
        Bus(broadcast::channel(CAPACITY).0)
    }
}

impl Bus {
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.subscribe()
    }

    /// Publishes every host event until all senders (the modules' contexts) are gone.
    pub async fn forward(self, mut events: mpsc::Receiver<HostEvent>, accounts: Arc<Accounts>) {
        while let Some(event) = events.recv().await {
            let event = match event {
                HostEvent::Session { module, account, state } => {
                    // A failed write keeps the old state; the event still goes out.
                    let _ = accounts.set_state(&module, &account, state);
                    Event::Session { account: format!("{module}:{account}"), state }
                }
                HostEvent::MessageArrived(msg) => Event::Message {
                    account: format!("{}:{}", msg.module, msg.account),
                    conversation: msg.conversation,
                    message: msg.message,
                    own: msg.own,
                    mentions_me: msg.mentions_me,
                    direct: msg.direct,
                },
            };
            // With no subscriber the event is simply dropped.
            let _ = self.0.send(event);
        }
    }
}
