//! The approval queue over real pending actions: one action on screen at a time.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{ECHO, daemon, info};
use open_mess::approval;
use open_mess::pending::Preview;
use serde_json::json;

/// What the presenter was handed so far, once the queue has caught up with `count` of them.
async fn shown(shown: &Mutex<Vec<String>>, count: usize) -> Vec<String> {
    for _ in 0..100 {
        if shown.lock().unwrap().len() >= count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Give a wrong extra toast the chance to show up too.
    tokio::time::sleep(Duration::from_millis(50)).await;
    shown.lock().unwrap().clone()
}

#[tokio::test]
async fn one_toast_at_a_time() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let pending = Arc::clone(d.registry.pending().unwrap());
    let presented = Arc::new(Mutex::new(Vec::new()));
    let present = {
        let presented = Arc::clone(&presented);
        move |preview: &Preview| presented.lock().unwrap().push(preview.action.clone())
    };
    tokio::spawn(approval::run(Arc::clone(&pending), present));

    // An agent sends in a loop: 20 wait, one is on screen.
    let mut actions = Vec::new();
    for n in 0..25 {
        if let Ok(result) = d.registry.call("send", json!({"account": "test-echo:a", "conversation": "c1", "text": format!("message {n}")})).await {
            actions.push(result["action"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(actions.len(), 20);
    assert_eq!(shown(&presented, 1).await, actions[..1]);

    // The next one appears once the shown one is decided.
    pending.decide(&actions[0], false).await.unwrap();
    assert_eq!(shown(&presented, 2).await, actions[..2]);
    // Deciding one that is not on screen (in the configuration window, later) shows nothing new.
    pending.decide(&actions[5], false).await.unwrap();
    assert_eq!(shown(&presented, 2).await, actions[..2]);
    pending.decide(&actions[1], false).await.unwrap();
    assert_eq!(shown(&presented, 3).await, actions[..3]);
}
