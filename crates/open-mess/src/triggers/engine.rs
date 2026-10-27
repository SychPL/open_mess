//! The trigger engine: follows the event bus, matches messages against the rules, collects each
//! rule's messages per conversation into a batch, and hands the batch to the rule's action once the
//! conversation has been quiet for the rule's quiet period, but never later than its longest delay
//! after the first message (spec 8.7, R4). Quiet hours and the hourly cap drop batches; every drop
//! is counted for `status`. What the batches hold is capped in count and in bytes, per rule and
//! in all: ids come from modules, which may send long ones.
//!
//! Never fired: the user's own messages, messages a rule has handled already (a module may report
//! one again after a restart; they are kept in the database for [`SEEN_FOR`], except those of a
//! batch the daemon stopped before sending, which can fire again), accounts that are
//! unknown or being removed, and conversations the agent may not read (spec 8.4, 8.6, 8.7). The
//! account and the permission are checked again when a batch goes out, so taking the read
//! permission away mutes it at once (R7).
//!
//! What the engine is doing is counted in [`Activity`], for an update waiting for it to finish; its
//! hold keeps the batches collecting without handing any over.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::NaiveTime;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use super::rules::{self, Condition, Rule};
use super::{Activity, Counters};
use crate::accounts::{Accounts, REMOVING};
use crate::events::{Bus, Event};
use crate::permissions;
use crate::store::Store;

/// Messages a batch keeps, the newest; `count` still counts them all.
pub const MAX_BATCH: usize = 100;
/// Batches collected at once over all rules and conversations; a message that would open one more
/// is dropped and counted.
pub const MAX_OPEN: usize = 1000;
/// Bytes of ids the open batches of one rule hold. A message past it pushes the oldest messages of
/// its own batch out (the batch's `count` still counts them); with none to push out it is dropped
/// and counted.
pub const MAX_RULE_BYTES: usize = 2 * 1024 * 1024;
/// The same over all rules.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
/// How long a rule remembers the messages it handled.
pub const SEEN_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Messages remembered over all rules; the oldest go first.
pub const MAX_SEEN: i64 = 50_000;
/// Forgetting runs when a batch goes out, and after this many messages remembered.
const PRUNE_EVERY: usize = 1000;
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

impl BatchedMessage {
    /// What it holds, as counted against [`MAX_RULE_BYTES`] and [`MAX_BYTES`].
    fn bytes(&self) -> usize {
        self.id.len() + self.sender.len()
    }
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
pub fn spawn<D: Delivery>(
    bus: &Bus,
    accounts: Arc<Accounts>,
    delivery: Arc<D>,
    counters: Counters,
    activity: Arc<Activity>,
    local_time: fn() -> NaiveTime,
) -> JoinHandle<()> {
    let events = bus.subscribe();
    // Batches collected when the daemon stopped never went out: a module that reports their
    // messages again wakes the rules for them.
    // ponytail: a failed delete leaves them muted until they are forgotten (SEEN_FOR).
    let _ = accounts.store().conn().execute("DELETE FROM trigger_seen WHERE batch IS NOT NULL", []);
    let engine = Engine { accounts, delivery, counters, activity, local_time, open: HashMap::new(), sent: Arc::default(), remembered: 0, batches: 0 };
    tokio::spawn(engine.run(events))
}

/// When each rule's batches of the last hour went out, by rule id.
type Sent = Arc<Mutex<HashMap<String, VecDeque<Instant>>>>;

struct Engine<D> {
    accounts: Arc<Accounts>,
    delivery: Arc<D>,
    counters: Counters,
    activity: Arc<Activity>,
    local_time: fn() -> NaiveTime,
    /// Batches being collected, by rule id and conversation.
    open: HashMap<(String, String), Open>,
    /// Shared with the deliveries: one that woke nobody gives its place back.
    sent: Sent,
    /// Messages remembered as handled since the engine started.
    remembered: usize,
    /// Batches opened since the engine started; the last one's number.
    batches: i64,
}

struct Open {
    /// Its number, under which its messages are remembered until it goes out.
    id: i64,
    batch: Batch,
    first: Instant,
    last: Instant,
    quiet: Duration,
    max_delay: Duration,
    /// The batch's ids, its messages' and its own.
    bytes: usize,
}

impl Open {
    fn due(&self) -> Instant {
        (self.last + self.quiet).min(self.first + self.max_delay)
    }

    /// Lets the oldest message go; it stays counted in `count`.
    fn drop_oldest(&mut self) {
        if !self.batch.messages.is_empty() {
            let gone = self.batch.messages.remove(0);
            self.bytes = self.bytes.saturating_sub(gone.bytes());
        }
    }
}

impl<D: Delivery> Engine<D> {
    async fn run(mut self, mut events: broadcast::Receiver<Event>) {
        let mut hold = self.activity.hold.subscribe();
        loop {
            // Held, due batches wait for the release rather than for a deadline already past.
            let due = if *hold.borrow_and_update() { None } else { self.open.values().map(Open::due).min() };
            let consumed = tokio::select! {
                event = events.recv() => match event {
                    Ok(Event::Message { account, conversation, message, sender, own, mentions_me, direct }) => {
                        if !own {
                            self.add(&account, &conversation, BatchedMessage { id: message, sender, at_ms: now_ms(), untrusted: None }, mentions_me, direct);
                        }
                        1
                    }
                    Ok(Event::Session { .. }) => 1,
                    // The bus kept the newest events; the engine goes on with them.
                    Err(RecvError::Lagged(missed)) => {
                        self.counters.add_lagged(missed);
                        missed
                    }
                    Err(RecvError::Closed) => return,
                },
                () = sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => {
                    self.flush_due();
                    0
                }
                Ok(()) = hold.changed() => 0,
            };
            // Consumed once it is in its batch: whoever sees the event consumed sees the batch.
            self.activity.open_batches.store(self.open.len(), Ordering::SeqCst);
            self.activity.consumed.fetch_add(consumed, Ordering::SeqCst);
        }
    }

    /// Adds a message from someone else to the batch of every rule it fires.
    fn add(&mut self, account: &str, conversation: &str, message: BatchedMessage, mentions_me: bool, direct: bool) {
        if !self.known(account) {
            return;
        }
        let hash = seen_hash(account, conversation, &message.id);
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
            fired.into_iter().filter(|rule| !seen(&store, &rule.id, &hash)).collect()
        };
        let now = Instant::now();
        for rule in fired {
            let key = (rule.id.clone(), conversation.to_owned());
            let new = !self.open.contains_key(&key);
            let cost = message.bytes() + if new { account.len() + conversation.len() } else { 0 };
            if (new && self.open.len() >= MAX_OPEN) || !self.make_room(&key, cost) {
                self.counters.count(&rule.id, |counters| &mut counters.dropped_queue);
                continue;
            }
            if new {
                self.batches = self.batches.wrapping_add(1);
            }
            let open = self.open.entry(key).or_insert_with(|| Open {
                id: self.batches,
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
                bytes: account.len() + conversation.len(),
            });
            open.last = now;
            open.batch.last_ms = message.at_ms;
            open.batch.count = open.batch.count.saturating_add(1);
            if open.batch.messages.len() >= MAX_BATCH {
                open.drop_oldest();
            }
            open.bytes = open.bytes.saturating_add(message.bytes());
            open.batch.messages.push(message.clone());
            let batch = open.id;
            // Handled from here on, even if pushed out of its batch later or dropped with it.
            // ponytail: a failed write only means a message reported again may wake the rule again.
            let store = self.accounts.store();
            let _ = remember(&store, &rule.id, account, &hash, batch);
            // Also between batches, which a flood of messages may keep open for minutes.
            self.remembered = self.remembered.wrapping_add(1);
            if self.remembered.is_multiple_of(PRUNE_EVERY) {
                let _ = forget_old(&store);
            }
        }
    }

    /// Bytes held by the open batches of `rule`, and by all of them.
    // ponytail: every open batch is summed for each message; keep running totals if that shows.
    fn held(&self, rule: &str) -> (usize, usize) {
        self.open.iter().fold((0usize, 0usize), |(of_rule, all), (key, open)| {
            (if key.0 == rule { of_rule.saturating_add(open.bytes) } else { of_rule }, all.saturating_add(open.bytes))
        })
    }

    /// Makes room for `cost` more bytes in the batch at `key` within its rule's budget and the
    /// engine's, by letting that batch's own oldest messages go; false, letting none go, when even
    /// all of them would not be enough.
    fn make_room(&mut self, key: &(String, String), cost: usize) -> bool {
        let fits = |(of_rule, all): (usize, usize), freed: usize| {
            of_rule.saturating_sub(freed).saturating_add(cost) <= MAX_RULE_BYTES && all.saturating_sub(freed).saturating_add(cost) <= MAX_BYTES
        };
        let own = self.open.get(key).map_or(0, |open| open.batch.messages.iter().map(BatchedMessage::bytes).sum());
        if !fits(self.held(&key.0), own) {
            return false;
        }
        while !fits(self.held(&key.0), 0) {
            match self.open.get_mut(key) {
                Some(open) if !open.batch.messages.is_empty() => open.drop_oldest(),
                _ => return false,
            }
        }
        true
    }

    fn flush_due(&mut self) {
        if *self.activity.hold.borrow() {
            return;
        }
        let now = Instant::now();
        let due: Vec<(String, String)> = self.open.iter().filter(|(_, open)| open.due() <= now).map(|(key, _)| key.clone()).collect();
        for key in due {
            if let Some(open) = self.open.remove(&key) {
                self.flush(&key.0, open.id, open.batch, now);
            }
        }
    }

    /// Hands a batch to its rule's action, unless the rule, the account or the read permission is
    /// gone by now, or quiet hours or the hourly cap stop it.
    fn flush(&mut self, rule_id: &str, id: i64, batch: Batch, now: Instant) {
        {
            // Its messages are handled now, whatever happens to it, and a daemon stopping from here
            // on does not bring them back. ponytail: failed writes are tried again with the next batch.
            let store = self.accounts.store();
            let _ = store.conn().execute("UPDATE trigger_seen SET batch = NULL WHERE batch = ?1", [id]);
            let _ = forget_old(&store);
        }
        let Some(rule) = current_rule(&self.accounts, rule_id, &batch.account, &batch.conversation) else {
            return;
        };
        if in_quiet_hours(rule.quiet_hours, (self.local_time)()) {
            self.counters.count(&rule.id, |counters| &mut counters.dropped_quiet_hours);
            return;
        }
        {
            let mut sent = self.sent.lock().unwrap_or_else(PoisonError::into_inner);
            let sent = sent.entry(rule.id.clone()).or_default();
            while sent.front().is_some_and(|at| now.duration_since(*at) >= HOUR) {
                sent.pop_front();
            }
            if sent.len() >= usize::try_from(rule.max_per_hour).unwrap_or(usize::MAX) {
                self.counters.count(&rule.id, |counters| &mut counters.dropped_rate);
                return;
            }
            sent.push_back(now);
        }
        // In its own task: an action may wait for a module, and the bus must keep moving.
        let (delivery, counters, sent) = (Arc::clone(&self.delivery), self.counters.clone(), Arc::clone(&self.sent));
        let delivering = Delivering::new(&self.activity);
        tokio::spawn(async move {
            let _delivering = delivering;
            let id = rule.id.clone();
            match delivery.deliver(rule, batch).await {
                DeliveryResult::Delivered => counters.count(&id, |counters| &mut counters.delivered),
                DeliveryResult::DeliveredDroppingOldest => {
                    counters.count(&id, |counters| &mut counters.delivered);
                    counters.count(&id, |counters| &mut counters.dropped_queue);
                }
                DeliveryResult::NoSession => {
                    counters.count(&id, |counters| &mut counters.dropped_no_session);
                    // Nobody was woken, so it does not count against the hourly cap.
                    let mut sent = sent.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some(sent) = sent.get_mut(&id)
                        && let Some(at) = sent.iter().position(|at| *at == now)
                    {
                        sent.remove(at);
                    }
                }
            }
        });
    }

    fn known(&self, account: &str) -> bool {
        known(&self.accounts, account)
    }
}

/// A batch handed to its action, counted from the engine letting it go until the action returns.
struct Delivering(Arc<Activity>);

impl Delivering {
    fn new(activity: &Arc<Activity>) -> Delivering {
        activity.delivering.fetch_add(1, Ordering::SeqCst);
        Delivering(Arc::clone(activity))
    }
}

impl Drop for Delivering {
    fn drop(&mut self) {
        // Before the count drops: whoever sees no delivery sees when the last one ended.
        self.0.last_delivery_ms.store(now_ms(), Ordering::SeqCst);
        self.0.delivering.fetch_sub(1, Ordering::SeqCst);
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

/// How a handled message is kept: its ids can be long, the hash is not.
fn seen_hash(account: &str, conversation: &str, message: &str) -> String {
    crate::modules::sha256_hex(format!("{account}\n{conversation}\n{message}").as_bytes())
}

/// Whether the rule has handled the message. A database that can't say lets it through.
fn seen(store: &Store, rule: &str, hash: &str) -> bool {
    let found = store.conn().query_row("SELECT 1 FROM trigger_seen WHERE rule = ?1 AND message = ?2", [rule, hash], |_| Ok(()));
    found.is_ok()
}

/// Keeps the message as handled by the rule, in the open batch `batch`.
fn remember(store: &Store, rule: &str, account: &str, hash: &str, batch: i64) -> Result<(), rusqlite::Error> {
    let now = i64::try_from(now_ms()).unwrap_or(i64::MAX);
    store.conn().execute(
        "INSERT OR REPLACE INTO trigger_seen (rule, account, message, at_ms, batch) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![rule, account, hash, now, batch],
    )?;
    Ok(())
}

/// Forgets the handled messages past [`SEEN_FOR`] or [`MAX_SEEN`].
fn forget_old(store: &Store) -> Result<(), rusqlite::Error> {
    let cutoff = i64::try_from(now_ms()).unwrap_or(i64::MAX).saturating_sub(i64::try_from(SEEN_FOR.as_millis()).unwrap_or(i64::MAX));
    store.conn().execute("DELETE FROM trigger_seen WHERE at_ms < ?1", [cutoff])?;
    store
        .conn()
        .execute("DELETE FROM trigger_seen WHERE rowid IN (SELECT rowid FROM trigger_seen ORDER BY at_ms DESC LIMIT -1 OFFSET ?1)", [MAX_SEEN])?;
    Ok(())
}

/// Whether `now` falls in the quiet hours; a window that starts later than it ends wraps midnight.
fn in_quiet_hours(hours: Option<(NaiveTime, NaiveTime)>, now: NaiveTime) -> bool {
    match hours {
        Some((start, end)) if start <= end => start <= now && now < end,
        Some((start, end)) => start <= now || now < end,
        None => false,
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}
