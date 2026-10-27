//! The trigger engine: which messages fire a rule, batching, quiet hours and the hourly cap. The
//! clock is tokio's paused one; deliveries go to a recorder instead of a session or a program.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::NaiveTime;
use common::{Daemon, info};
use open_mess::events::{self, Bus, Event};
use open_mess::permissions::{self, Write};
use open_mess::triggers::Counters;
use open_mess::triggers::engine::{self, Batch, Delivery, DeliveryResult};
use open_mess::triggers::rules::{self, Action, Condition, Rule};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

const ACCOUNT: &str = "test-echo:a";

/// The local time the engine sees, in minutes since midnight; only `quiet_hours_suppress` moves it.
static LOCAL_MINUTES: AtomicU32 = AtomicU32::new(12 * 60);

fn local_time() -> NaiveTime {
    let minutes = LOCAL_MINUTES.load(Ordering::SeqCst);
    NaiveTime::from_hms_opt(minutes / 60, minutes % 60, 0).unwrap()
}

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, Batch, Instant)>>,
}

impl Delivery for Recorder {
    async fn deliver(&self, rule: Rule, batch: Batch) -> DeliveryResult {
        self.calls.lock().unwrap().push((rule.id, batch, Instant::now()));
        DeliveryResult::Delivered
    }
}

struct Setup {
    d: Daemon,
    bus: Bus,
    recorder: Arc<Recorder>,
    counters: Counters,
    started: Instant,
    _engine: JoinHandle<()>,
}

impl Setup {
    fn calls(&self) -> Vec<(String, Batch, Instant)> {
        self.recorder.calls.lock().unwrap().clone()
    }

    fn store(&self) -> open_mess::store::Store {
        common::store(self.d.dir.path())
    }
}

/// Account "a" of test-echo (not loaded: the engine needs only the accounts table) with `rules`.
async fn setup(rules: &[Rule]) -> Setup {
    let d = common::daemon(&[]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let store = common::store(d.dir.path());
    for rule in rules {
        rules::put(&store, rule).unwrap();
    }
    let bus = Bus::default();
    let (recorder, counters) = (Arc::new(Recorder::default()), Counters::default());
    let engine = engine::spawn(&bus, Arc::clone(&d.accounts), Arc::clone(&recorder), counters.clone(), local_time);
    Setup { d, bus, recorder, counters, started: Instant::now(), _engine: engine }
}

fn rule(id: &str, condition: Condition) -> Rule {
    Rule::new(id, ACCOUNT, condition, Action::Channel { fallback_spawn: None })
}

#[derive(Clone, Copy)]
enum Kind {
    Plain,
    Direct,
    Mention,
    Own,
}

fn message(conversation: &str, id: &str, kind: Kind) -> Event {
    from(conversation, id, "p1", kind)
}

fn from(conversation: &str, id: &str, sender: &str, kind: Kind) -> Event {
    Event::Message {
        account: ACCOUNT.into(),
        conversation: conversation.into(),
        message: id.into(),
        sender: sender.into(),
        own: matches!(kind, Kind::Own),
        mentions_me: matches!(kind, Kind::Mention),
        direct: matches!(kind, Kind::Direct),
    }
}

fn ids(batch: &Batch) -> Vec<&str> {
    batch.messages.iter().map(|message| message.id.as_str()).collect()
}

#[tokio::test(start_paused = true)]
async fn direct_message_fires() {
    let t = setup(&[rule("r", Condition::Direct)]).await;
    t.bus.publish(message("c1", "m1", Kind::Plain));
    t.bus.publish(message("c1", "m2", Kind::Direct));
    sleep(Duration::from_secs(61)).await;
    let calls = t.calls();
    assert_eq!(calls.len(), 1);
    let (rule, batch, _) = &calls[0];
    assert_eq!((rule.as_str(), batch.account.as_str(), batch.conversation.as_str(), batch.kind), ("r", ACCOUNT, "c1", "direct"));
    assert_eq!((ids(batch), batch.count), (vec!["m2"], 1));
    assert_eq!(batch.messages[0].sender, "p1");
    assert!(batch.first_ms > 0 && batch.first_ms == batch.last_ms);
    assert_eq!(t.counters.rule("r").delivered, 1);
}

#[tokio::test(start_paused = true)]
async fn mention_fires() {
    let t = setup(&[rule("r", Condition::Mention)]).await;
    t.bus.publish(message("c1", "m1", Kind::Direct));
    t.bus.publish(message("c2", "m2", Kind::Mention));
    sleep(Duration::from_secs(61)).await;
    let calls = t.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!((calls[0].1.conversation.as_str(), calls[0].1.kind, ids(&calls[0].1)), ("c2", "mention", vec!["m2"]));
}

#[tokio::test(start_paused = true)]
async fn sender_and_conversation_fire() {
    let t = setup(&[
        rule("people", Condition::From { senders: vec!["p2".into(), "p3".into()] }),
        rule("c9", Condition::Conversation { id: "c9".into() }),
    ])
    .await;
    t.bus.publish(from("c1", "m1", "p1", Kind::Plain));
    t.bus.publish(from("c1", "m2", "p3", Kind::Plain));
    t.bus.publish(from("c9", "m3", "p1", Kind::Plain));
    sleep(Duration::from_secs(61)).await;
    let mut fired: Vec<(String, &str, Vec<String>)> =
        t.calls().iter().map(|(rule, batch, _)| (rule.clone(), batch.kind, batch.messages.iter().map(|m| m.id.clone()).collect())).collect();
    fired.sort();
    assert_eq!(fired, [("c9".into(), "conversation", vec!["m3".into()]), ("people".into(), "from", vec!["m2".into()])]);
}

#[tokio::test(start_paused = true)]
async fn own_messages_never_fire() {
    // The user's own message, sent from any client, in a watched conversation, as a DM and with a
    // mention of themselves.
    let t = setup(&[rule("c1", Condition::Conversation { id: "c1".into() }), rule("dm", Condition::Direct), rule("at", Condition::Mention)]).await;
    t.bus.publish(message("c1", "m1", Kind::Own));
    let mut own_dm = from("c1", "m2", "me", Kind::Own);
    if let Event::Message { direct, mentions_me, .. } = &mut own_dm {
        (*direct, *mentions_me) = (true, true);
    }
    t.bus.publish(own_dm);
    sleep(Duration::from_secs(600)).await;
    assert!(t.calls().is_empty());
    // Someone else's message in the same conversation does fire.
    t.bus.publish(message("c1", "m3", Kind::Plain));
    sleep(Duration::from_secs(61)).await;
    assert_eq!(t.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn unreadable_conversation_never_fires() {
    let t = setup(&[rule("r", Condition::Direct)]).await;
    permissions::set(&t.store(), ACCOUNT, Some("c1"), false, Write::Confirm).unwrap();
    t.bus.publish(message("c1", "m1", Kind::Direct));
    t.bus.publish(message("c2", "m2", Kind::Direct));
    sleep(Duration::from_secs(61)).await;
    assert_eq!(t.calls().iter().map(|(_, batch, _)| batch.conversation.as_str()).collect::<Vec<_>>(), ["c2"]);
    // Taking the read permission away mutes a batch already being collected (spec 8.7, R7).
    t.bus.publish(message("c2", "m3", Kind::Direct));
    sleep(Duration::from_secs(10)).await;
    permissions::set(&t.store(), ACCOUNT, None, false, Write::Confirm).unwrap();
    sleep(Duration::from_secs(60)).await;
    assert_eq!(t.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn removing_account_never_fires() {
    let t =
        setup(&[rule("r", Condition::Direct), Rule::new("ghost", "test-echo:ghost", Condition::Direct, Action::Channel { fallback_spawn: None })])
            .await;
    // An account that was never added.
    let mut ghost = message("c1", "m1", Kind::Direct);
    if let Event::Message { account, .. } = &mut ghost {
        *account = "test-echo:ghost".into();
    }
    t.bus.publish(ghost);
    // An account being removed (accounts.rs, REMOVING).
    let db = rusqlite::Connection::open(t.d.dir.path().join("state.sqlite3")).unwrap();
    db.execute("UPDATE accounts SET state = 'removing' WHERE id = ?1", [ACCOUNT]).unwrap();
    t.bus.publish(message("c1", "m2", Kind::Direct));
    sleep(Duration::from_secs(600)).await;
    assert!(t.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn quiet_period_batches() {
    let t = setup(&[rule("r", Condition::Direct)]).await;
    for id in ["m1", "m2", "m3"] {
        t.bus.publish(message("c1", id, Kind::Direct));
        sleep(Duration::from_secs(10)).await;
    }
    // A minute after the last message, one batch with all three.
    sleep(Duration::from_secs(49)).await;
    assert!(t.calls().is_empty());
    sleep(Duration::from_secs(2)).await;
    let calls = t.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!((ids(&calls[0].1), calls[0].1.count), (vec!["m1", "m2", "m3"], 3));
    assert_eq!(calls[0].2 - t.started, Duration::from_secs(80));
    // Wall-clock times, which tokio's paused clock does not move.
    assert!(calls[0].1.first_ms <= calls[0].1.last_ms);
}

#[tokio::test(start_paused = true)]
async fn max_delay_caps_batching() {
    // A chat that never goes quiet: a message every 10 s for 10 minutes.
    let t = setup(&[rule("r", Condition::Direct)]).await;
    for n in 0..60 {
        t.bus.publish(message("c1", &format!("m{n}"), Kind::Direct));
        sleep(Duration::from_secs(10)).await;
    }
    sleep(Duration::from_secs(70)).await;
    let calls = t.calls();
    // The first batch goes out five minutes after its first message, not never.
    assert_eq!(calls[0].2 - t.started, Duration::from_secs(300));
    assert!(calls.len() >= 2, "{}", calls.len());
    assert_eq!(calls.iter().map(|(_, batch, _)| batch.count).sum::<usize>(), 60);
}

#[tokio::test(start_paused = true)]
async fn quiet_hours_suppress() {
    let mut night = rule("r", Condition::Direct);
    night.quiet_hours = Some((NaiveTime::from_hms_opt(22, 0, 0).unwrap(), NaiveTime::from_hms_opt(7, 0, 0).unwrap()));
    let t = setup(&[night]).await;
    // The window wraps midnight: 23:30, 00:30 and 06:59 are quiet, 07:00 and 21:59 are not.
    for (n, (hour, minute)) in [(23, 30), (0, 30), (6, 59), (7, 0), (21, 59)].into_iter().enumerate() {
        LOCAL_MINUTES.store(hour * 60 + minute, Ordering::SeqCst);
        t.bus.publish(message("c1", &format!("m{n}"), Kind::Direct));
        sleep(Duration::from_secs(61)).await;
    }
    LOCAL_MINUTES.store(12 * 60, Ordering::SeqCst);
    assert_eq!(t.calls().iter().flat_map(|(_, batch, _)| ids(batch)).collect::<Vec<_>>(), ["m3", "m4"]);
    let counters = t.counters.rule("r");
    assert_eq!((counters.delivered, counters.dropped_quiet_hours), (2, 3));
}

#[tokio::test(start_paused = true)]
async fn hourly_cap() {
    // Twenty DMs in a minute, from twenty people: each their own batch, at most 6 an hour.
    let mut capped = rule("r", Condition::Direct);
    (capped.max_per_hour, capped.quiet_ms, capped.max_delay_ms) = (6, 5_000, 5_000);
    let t = setup(&[capped]).await;
    for n in 0..20 {
        t.bus.publish(message(&format!("c{n}"), &format!("m{n}"), Kind::Direct));
        sleep(Duration::from_secs(3)).await;
    }
    sleep(Duration::from_secs(10)).await;
    assert_eq!(t.calls().len(), 6);
    let counters = t.counters.rule("r");
    assert_eq!((counters.delivered, counters.dropped_rate), (6, 14));
    // An hour after the first delivery there is room again.
    sleep(Duration::from_secs(3600)).await;
    t.bus.publish(message("c1", "later", Kind::Direct));
    sleep(Duration::from_secs(6)).await;
    assert_eq!(t.calls().last().map(|(_, batch, _)| ids(batch)), Some(vec!["later"]));
}

#[tokio::test(start_paused = true)]
async fn disabled_rule_is_ignored() {
    let mut off = rule("r", Condition::Direct);
    off.enabled = false;
    let t = setup(&[off.clone()]).await;
    t.bus.publish(message("c1", "m1", Kind::Direct));
    sleep(Duration::from_secs(30)).await;
    // Disabled while a batch is being collected: nothing goes out either.
    off.enabled = true;
    rules::put(&t.store(), &off).unwrap();
    t.bus.publish(message("c2", "m2", Kind::Direct));
    sleep(Duration::from_secs(10)).await;
    off.enabled = false;
    rules::put(&t.store(), &off).unwrap();
    sleep(Duration::from_secs(600)).await;
    assert!(t.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn lagged_bus_is_counted() {
    let t = setup(&[rule("r", Condition::Direct)]).await;
    // Published without yielding, so the engine falls behind and loses the oldest.
    let flood = events::CAPACITY + 100;
    for n in 0..flood {
        t.bus.publish(message("flood", &format!("m{n}"), Kind::Direct));
    }
    sleep(Duration::from_secs(61)).await;
    assert_eq!(t.counters.lagged(), 100);
    // What was kept is one batch, its message list capped.
    let calls = t.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1.count, events::CAPACITY);
    assert_eq!(calls[0].1.messages.len(), engine::MAX_BATCH);
    assert_eq!(calls[0].1.messages.last().map(|message| message.id.clone()), Some(format!("m{}", flood - 1)));
    // And the engine still works.
    t.bus.publish(message("c1", "after", Kind::Direct));
    sleep(Duration::from_secs(61)).await;
    assert_eq!(t.calls().len(), 2);
}
