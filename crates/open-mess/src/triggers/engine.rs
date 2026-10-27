//! The trigger engine: follows the event bus, matches messages against the rules, collects each
//! rule's messages per conversation into a batch, and hands the batch to the rule's action once the
//! conversation has been quiet for the rule's quiet period, but never later than its longest delay
//! after the first message (spec 8.7, R4). Quiet hours and the hourly cap drop batches; every drop
//! is counted for `status`.
//!
//! Never fired: the user's own messages, accounts that are unknown or being removed, and
//! conversations the agent may not read (spec 8.6, 8.7). The account and the permission are checked
//! again when a batch goes out, so taking the read permission away mutes it at once (R7).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::NaiveTime;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use super::Counters;
use super::rules::{self, Condition, Rule};
use crate::accounts::{Accounts, REMOVING};
use crate::events::{Bus, Event};
use crate::permissions;
use crate::store::Store;

/// Messages a batch keeps, the newest; `count` still counts them all.
pub const MAX_BATCH: usize = 100;
/// Batches collected at once over all rules and conversations; a message that would open one more
/// is dropped and counted.
pub const MAX_OPEN: usize = 1000;
const HOUR: Duration = Duration::from_secs(60 * 60);

/// The messages of one rule in one conversation, delivered together.
#[derive(Clone)]
pub struct Batch {
    pub account: String,
    pub conversation: String,
    /// What fired the rule: its condition's name.
    pub kind: &'static str,
    /// The newest [`MAX_BATCH`] messages, oldest first.
    pub messages: Vec<BatchedMessage>,
    /// All the batch's messages, those no longer in `messages` included.
    pub count: usize,
    /// When the first and the last message arrived, in milliseconds since 1970.
    pub first_ms: u64,
    pub last_ms: u64,
}

// Hand-written so that a `{:?}` in a log line never prints conversation or message ids.
impl std::fmt::Debug for Batch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Batch").field("account", &self.account).field("kind", &self.kind).field("count", &self.count).finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct BatchedMessage {
    pub id: String,
    /// The sender's platform id.
    pub sender: String,
    pub at_ms: u64,
    /// Other people's content (sender name, text) as the module's `read_messages` gives it, filled
    /// in by the action; None until then, or when the module could not say.
    pub untrusted: Option<serde_json::Value>,
}

/// Where batches go: the rules' actions, or a recorder in tests.
pub trait Delivery: Send + Sync + 'static {
    fn deliver(&self, rule: Rule, batch: Batch) -> impl Future<Output = DeliveryResult> + Send;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryResult {
    /// A session was notified, or the program started or waits for its turn.
    Delivered,
    /// Delivered, and the oldest batch waiting for the rule's program was dropped to make room.
    DeliveredDroppingOldest,
    /// No session to notify and no program to start.
    NoSession,
}

/// Starts the engine on the bus; it runs until the bus closes. `local_time` gives the local time
/// of day, for quiet hours.
pub fn spawn<D: Delivery>(bus: &Bus, accounts: Arc<Accounts>, delivery: Arc<D>, counters: Counters, local_time: fn() -> NaiveTime) -> JoinHandle<()> {
    let events = bus.subscribe();
    let engine = Engine { accounts, delivery, counters, local_time, open: HashMap::new(), sent: HashMap::new() };
    tokio::spawn(engine.run(events))
}

struct Engine<D> {
    accounts: Arc<Accounts>,
    delivery: Arc<D>,
    counters: Counters,
    local_time: fn() -> NaiveTime,
    /// Batches being collected, by rule id and conversation.
    open: HashMap<(String, String), Open>,
    /// When each rule's batches of the last hour went out.
    sent: HashMap<String, VecDeque<Instant>>,
}

struct Open {
    batch: Batch,
    first: Instant,
    last: Instant,
    quiet: Duration,
    max_delay: Duration,
}

impl Open {
    fn due(&self) -> Instant {
        (self.last + self.quiet).min(self.first + self.max_delay)
    }
}

impl<D: Delivery> Engine<D> {
    async fn run(mut self, mut events: broadcast::Receiver<Event>) {
        loop {
            let due = self.open.values().map(Open::due).min();
            tokio::select! {
                event = events.recv() => match event {
                    Ok(Event::Message { account, conversation, message, sender, own, mentions_me, direct }) => {
                        if !own {
                            self.add(&account, &conversation, BatchedMessage { id: message, sender, at_ms: now_ms(), untrusted: None }, mentions_me, direct);
                        }
                    }
                    Ok(Event::Session { .. }) => {}
                    // The bus kept the newest events; the engine goes on with them.
                    Err(RecvError::Lagged(missed)) => self.counters.add_lagged(missed),
                    Err(RecvError::Closed) => return,
                },
                () = sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => self.flush_due(),
            }
        }
    }

    /// Adds a message from someone else to the batch of every rule it fires.
    fn add(&mut self, account: &str, conversation: &str, message: BatchedMessage, mentions_me: bool, direct: bool) {
        if !self.known(account) {
            return;
        }
        let fired: Vec<Rule> = {
            let store = self.accounts.store();
            // ponytail: every rule is read for every message; index them by account if rules get many.
            let Ok(rules) = rules::list(&store) else {
                return;
            };
            let fired = rules.into_iter().filter(|rule| {
                rule.enabled
                    && rule.account == account
                    && match &rule.condition {
                        Condition::Direct => direct,
                        Condition::Mention => mentions_me,
                        Condition::Conversation { id } => id == conversation,
                        Condition::From { senders } => senders.contains(&message.sender),
                    }
            });
            let fired: Vec<Rule> = fired.collect();
            if fired.is_empty() || !readable(&store, account, conversation) {
                return;
            }
            fired
        };
        let now = Instant::now();
        for rule in fired {
            let key = (rule.id.clone(), conversation.to_owned());
            if !self.open.contains_key(&key) && self.open.len() >= MAX_OPEN {
                self.counters.count(&rule.id, |counters| &mut counters.dropped_queue);
                continue;
            }
            let open = self.open.entry(key).or_insert_with(|| Open {
                batch: Batch {
                    account: account.to_owned(),
                    conversation: conversation.to_owned(),
                    kind: rule.condition.name(),
                    messages: Vec::new(),
                    count: 0,
                    first_ms: message.at_ms,
                    last_ms: message.at_ms,
                },
                first: now,
                last: now,
                quiet: Duration::from_millis(rule.quiet_ms),
                max_delay: Duration::from_millis(rule.max_delay_ms),
            });
            open.last = now;
            open.batch.last_ms = message.at_ms;
            open.batch.count = open.batch.count.saturating_add(1);
            if open.batch.messages.len() >= MAX_BATCH {
                open.batch.messages.remove(0);
            }
            open.batch.messages.push(message.clone());
        }
    }

    fn flush_due(&mut self) {
        let now = Instant::now();
        let due: Vec<(String, String)> = self.open.iter().filter(|(_, open)| open.due() <= now).map(|(key, _)| key.clone()).collect();
        for key in due {
            if let Some(open) = self.open.remove(&key) {
                self.flush(&key.0, open.batch, now);
            }
        }
    }

    /// Hands a batch to its rule's action, unless the rule, the account or the read permission is
    /// gone by now, or quiet hours or the hourly cap stop it.
    fn flush(&mut self, rule_id: &str, batch: Batch, now: Instant) {
        let Some(rule) = current_rule(&self.accounts, rule_id, &batch.account, &batch.conversation) else {
            return;
        };
        if in_quiet_hours(rule.quiet_hours, (self.local_time)()) {
            self.counters.count(&rule.id, |counters| &mut counters.dropped_quiet_hours);
            return;
        }
        let sent = self.sent.entry(rule.id.clone()).or_default();
        while sent.front().is_some_and(|at| now.duration_since(*at) >= HOUR) {
            sent.pop_front();
        }
        if sent.len() >= usize::try_from(rule.max_per_hour).unwrap_or(usize::MAX) {
            self.counters.count(&rule.id, |counters| &mut counters.dropped_rate);
            return;
        }
        sent.push_back(now);
        // In its own task: an action may wait for a module, and the bus must keep moving.
        let (delivery, counters) = (Arc::clone(&self.delivery), self.counters.clone());
        tokio::spawn(async move {
            let id = rule.id.clone();
            match delivery.deliver(rule, batch).await {
                DeliveryResult::Delivered => counters.count(&id, |counters| &mut counters.delivered),
                DeliveryResult::DeliveredDroppingOldest => {
                    counters.count(&id, |counters| &mut counters.delivered);
                    counters.count(&id, |counters| &mut counters.dropped_queue);
                }
                DeliveryResult::NoSession => counters.count(&id, |counters| &mut counters.dropped_no_session),
            }
        });
    }

    fn known(&self, account: &str) -> bool {
        known(&self.accounts, account)
    }
}

/// The rule, if a batch of it from `conversation` may still go out: the rule exists and is enabled
/// for the account, the account is known and not being removed, and the agent may read the
/// conversation. Asked again right before a batch reaches an agent.
pub(crate) fn current_rule(accounts: &Accounts, rule_id: &str, account: &str, conversation: &str) -> Option<Rule> {
    if !known(accounts, account) {
        return None;
    }
    let store = accounts.store();
    let rule = rules::get(&store, rule_id).ok()??;
    (rule.enabled && rule.account == account && readable(&store, account, conversation)).then_some(rule)
}

/// Whether the account exists and is not being removed; the bus still carries events of accounts
/// in either state.
fn known(accounts: &Accounts, account: &str) -> bool {
    matches!(accounts.get(account), Ok(Some(found)) if found.state != REMOVING)
}

fn readable(store: &Store, account: &str, conversation: &str) -> bool {
    permissions::effective(store, account, Some(conversation)).is_ok_and(|effective| effective.read)
}

/// Whether `now` falls in the quiet hours; a window that starts later than it ends wraps midnight.
fn in_quiet_hours(hours: Option<(NaiveTime, NaiveTime)>, now: NaiveTime) -> bool {
    match hours {
        Some((start, end)) if start <= end => start <= now && now < end,
        Some((start, end)) => start <= now || now < end,
        None => false,
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}
