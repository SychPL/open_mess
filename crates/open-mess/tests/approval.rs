//! The approval notifications over real pending actions: one notification on screen at a time, an
//! action's own when it waits alone, one notice when several wait.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{ECHO, daemon, info};
use open_mess::approval;
use open_mess::pending::Preview;
use serde_json::json;

type Events = Arc<Mutex<Vec<String>>>;

/// Waits until `done` holds for what was put on and taken off the screen, then a little longer,
/// so a wrong extra notification has the chance to show up too.
async fn events(events: &Events, done: impl Fn(&[String]) -> bool) -> Vec<String> {
    for _ in 0..100 {
        if done(&events.lock().unwrap()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    events.lock().unwrap().clone()
}

fn notices(events: &[String]) -> usize {
    events.iter().filter(|event| event.ends_with(" wait")).count()
}

#[tokio::test]
async fn several_waiting_actions_share_one_notice() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let pending = Arc::clone(d.registry.pending().unwrap());
    let log: Events = Arc::default();
    let record = |event: fn(String) -> String| {
        let log = Arc::clone(&log);
        move |what: String| log.lock().unwrap().push(event(what))
    };
    let (present, several, withdraw) = (record(|action| action), record(|count| format!("{count} wait")), record(|_| "withdrawn".into()));
    tokio::spawn(approval::run(
        Arc::clone(&pending),
        move |preview: &Preview| present(preview.action.clone()),
        move |count| several(count.to_string()),
        move || withdraw(String::new()),
    ));

    // An agent sends in a loop: 20 wait, and one notice says so, shown once however many join.
    let mut actions = Vec::new();
    for n in 0..25 {
        if let Ok(result) = d.registry.call("send", json!({"account": "test-echo:a", "conversation": "c1", "text": format!("message {n}")})).await {
            actions.push(result["action"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(actions.len(), 20);
    let seen = events(&log, |events| notices(events) == 1).await;
    assert_eq!(notices(&seen), 1, "{seen:?}");
    // At most the first action had its own toast, before the second came; it was taken away.
    let toasts = seen.iter().filter(|event| actions.contains(event)).count();
    assert!(toasts <= 1, "{seen:?}");
    assert_eq!(seen.iter().filter(|event| *event == "withdrawn").count(), toasts);

    // Decided one by one in the window: no notice again on the way, and the last one left gets its
    // own toast, with its buttons.
    for action in &actions[..19] {
        pending.decide(action, false).await.unwrap();
    }
    let seen = events(&log, |events| events.last() == Some(&actions[19])).await;
    assert_eq!(seen[seen.len() - 2..], ["withdrawn".to_string(), actions[19].clone()]);
    assert_eq!(notices(&seen), 1);
    // Decided too: its toast goes.
    pending.decide(&actions[19], false).await.unwrap();
    let seen = events(&log, |events| events.last().is_some_and(|event| event == "withdrawn")).await;
    assert_eq!(seen.last().map(String::as_str), Some("withdrawn"));
}
