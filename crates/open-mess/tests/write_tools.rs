//! The write tools `send`, `edit_message` and `react`, and `pending`, through the tool registry.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use common::{ECHO, daemon, info};
use open_mess::pending::MAX_WAITING;
use open_mess::permissions::{self, Write};
use open_mess::tools::ToolError;
use serde_json::{Value, json};

fn send(account: &str, text: &str) -> Value {
    json!({"account": account, "conversation": "c1", "text": text})
}

#[tokio::test]
async fn send_returns_waiting_on_confirm() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let result = d.registry.call("send", send("test-echo:a", "hi")).await.unwrap();
    assert_eq!(result["state"], "waiting");
    assert_eq!(result.as_object().unwrap().keys().collect::<Vec<_>>(), ["action", "state"]);
    let waiting = d.registry.pending().unwrap().waiting();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].action, result["action"]);

    // The other writes wait the same way; autonomous runs at once, never is refused.
    for (tool, args) in [
        ("edit_message", json!({"account": "test-echo:a", "conversation": "c1", "message": "m1", "text": "fixed"})),
        ("react", json!({"account": "test-echo:a", "conversation": "c1", "message": "m1", "reaction": "like"})),
    ] {
        assert_eq!(d.registry.call(tool, args).await.unwrap()["state"], "waiting", "{tool}");
    }
    let store = common::store(d.dir.path());
    permissions::set(&store, "test-echo:a", None, true, Write::Autonomous).unwrap();
    assert_eq!(d.registry.call("send", send("test-echo:a", "hi")).await.unwrap()["state"], "done");
    permissions::set(&store, "test-echo:a", None, true, Write::Never).unwrap();
    let result = d.registry.call("send", send("test-echo:a", "hi")).await;
    assert!(matches!(&result, Err(ToolError::NotAllowed(why)) if why == "writing to this conversation is not allowed"), "{result:?}");
    // Arguments are checked first.
    for (tool, args) in [
        ("send", json!({"account": "test-echo:a", "conversation": "c1", "text": "x".repeat(28_001)})),
        ("send", json!({"account": "test-echo:a", "conversation": "c1", "text": ""})),
        ("send", json!({"account": "test-echo:a", "conversation": "c1"})),
        ("send", json!({"account": "test-echo:a", "conversation": "c1", "text": "hi", "approved": true})),
        ("react", json!({"account": "test-echo:a", "conversation": "c1", "message": "m1"})),
        ("pending", json!({"limit": 101})),
        ("pending", json!({"state": "sent"})),
    ] {
        assert!(matches!(d.registry.call(tool, args.clone()).await, Err(ToolError::InvalidArgs(_))), "{tool} {args}");
    }
    // The longest text passes them, up to the permission.
    let longest = d.registry.call("send", json!({"account": "test-echo:a", "conversation": "c1", "text": "x".repeat(28_000)})).await;
    assert!(matches!(longest, Err(ToolError::NotAllowed(_))), "{longest:?}");
}

#[tokio::test]
async fn waiting_cap_per_account() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    d.accounts.add("test-echo", info("b")).unwrap();
    // An agent sending in a loop.
    let mut refused = 0;
    for n in 0..25 {
        match d.registry.call("send", send("test-echo:a", &format!("message {n}"))).await {
            Ok(result) => assert_eq!(result["state"], "waiting"),
            Err(ToolError::NotAllowed(why)) if why == "too many actions waiting for approval" => refused += 1,
            Err(err) => panic!("{err}"),
        }
    }
    assert_eq!(refused, 25 - MAX_WAITING);
    assert_eq!(d.registry.pending().unwrap().waiting().len(), usize::try_from(MAX_WAITING).unwrap());
    // Another account has its own share.
    assert_eq!(d.registry.call("send", send("test-echo:b", "hi")).await.unwrap()["state"], "waiting");
}

#[tokio::test]
async fn pending_lists_states_without_text() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    d.accounts.add("test-echo", info("b")).unwrap();
    let pending = d.registry.pending().unwrap();
    let rejected = d.registry.call("send", send("test-echo:a", "secret plan one")).await.unwrap();
    pending.decide(rejected["action"].as_str().unwrap(), false).await.unwrap();
    d.registry.call("react", json!({"account": "test-echo:a", "conversation": "c1", "message": "m1", "reaction": "secret-heart"})).await.unwrap();
    permissions::set(&common::store(d.dir.path()), "test-echo:b", None, true, Write::Autonomous).unwrap();
    d.registry.call("send", send("test-echo:b", "secret plan two")).await.unwrap();

    let all = d.registry.call("pending", json!({})).await.unwrap();
    assert!(!all.to_string().contains("secret"), "{all}");
    let items = all["items"].as_array().unwrap();
    // Newest first.
    let summary: Vec<(&str, &str, &str)> =
        items.iter().map(|item| (item["account"].as_str().unwrap(), item["kind"].as_str().unwrap(), item["state"].as_str().unwrap())).collect();
    assert_eq!(summary, [("test-echo:b", "send", "done"), ("test-echo:a", "react", "waiting"), ("test-echo:a", "send", "rejected")]);
    assert_eq!(items[2]["action"], rejected["action"]);
    assert_eq!(items[2]["conversation"], "c1");
    assert!(items[2]["created"].as_str().unwrap().ends_with('Z'));
    assert!(items[2]["decided"].is_string());
    assert!(items[1].get("decided").is_none());

    let waiting = d.registry.call("pending", json!({"state": "waiting"})).await.unwrap();
    assert_eq!(waiting["items"].as_array().unwrap().len(), 1);
    let of_b = d.registry.call("pending", json!({"account": "test-echo:b"})).await.unwrap();
    assert_eq!(of_b["items"].as_array().unwrap().len(), 1);
    let newest = d.registry.call("pending", json!({"limit": 1})).await.unwrap();
    assert_eq!(newest["items"][0]["account"], "test-echo:b");
}
