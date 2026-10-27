//! The daemon's event bus: what modules report through the `host` imports (new messages, session
//! changes), with the core's account ids, for whoever subscribes (the triggers of milestone D).
//! Session changes also update the accounts table.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use open_mess_host::{HostEvent, SessionState};
use tokio::sync::{broadcast, mpsc};

use crate::accounts::Accounts;
use crate::triggers::Activity;

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
        /// The sender's platform id.
        sender: String,
        own: bool,
        mentions_me: bool,
        direct: bool,
    },
    Session {
        account: String,
        state: SessionState,
    },
}

// Hand-written so that a `{:?}` in a log line never prints conversation, message or sender ids.
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

    /// Publishes one event; with no subscriber it is dropped. The modules' events come through
    /// [`Bus::forward`].
    pub fn publish(&self, event: Event) {
        let _ = self.0.send(event);
    }

    /// Publishes every host event until all senders (the modules' contexts) are gone, counting
    /// each in `activity` as received.
    pub async fn forward(self, mut events: mpsc::Receiver<HostEvent>, accounts: Arc<Accounts>, activity: Arc<Activity>) {
        while let Some(event) = events.recv().await {
            activity.received.fetch_add(1, Ordering::SeqCst);
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
                    sender: msg.sender,
                    own: msg.own,
                    mentions_me: msg.mentions_me,
                    direct: msg.direct,
                },
            };
            self.publish(event);
        }
    }
}
