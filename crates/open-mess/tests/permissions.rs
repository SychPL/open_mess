//! Agent permissions (spec 8.6): what the read tools show and what they refuse.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used)]

mod common;

use common::{ECHO, daemon, info};
use open_mess::permissions::{self, Write};
use open_mess::tools::ToolError;
use open_mess_host::SessionState;
use serde_json::{Value, json};

fn refused(result: &Result<Value, ToolError>, why: &str) -> bool {
    matches!(result, Err(ToolError::NotAllowed(message)) if message == why)
}

fn ids(result: &Value) -> Vec<String> {
    result["items"].as_array().unwrap().iter().map(|item| item["id"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn unreadable_account_is_hidden() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    d.accounts.add("test-echo", info("b")).unwrap();
    permissions::set(&common::store(d.dir.path()), "test-echo:a", None, false, Write::Confirm).unwrap();
    // Offline too: an offline account the agent may read is an `errors` entry.
    d.accounts.set_state("test-echo", "a", SessionState::Offline).unwrap();

    for tool in ["unread", "list_conversations"] {
        let result = d.registry.call(tool, json!({})).await.unwrap();
        let items = result["items"].as_array().unwrap();
        assert!(!items.is_empty() && items.iter().all(|item| item["account"] == "test-echo:b"), "{tool}: {result}");
        assert!(result.get("errors").is_none(), "{tool}: {result}");
    }
    for (tool, args) in [
        ("read_messages", json!({"account": "test-echo:a", "conversation": "c1"})),
        ("get_attachment", json!({"account": "test-echo:a", "conversation": "c1", "message": "m1", "attachment": "x.txt"})),
        ("writing_context", json!({"account": "test-echo:a", "conversation": "c1"})),
    ] {
        let result = d.registry.call(tool, args).await;
        assert!(refused(&result, "reading this conversation is not allowed"), "{tool}: {result:?}");
    }
    let result = d.registry.call("unread", json!({"account": "test-echo:a"})).await;
    assert!(refused(&result, "reading this account is not allowed"), "{result:?}");
}

#[tokio::test]
async fn unreadable_conversation_is_filtered_from_lists() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let store = common::store(d.dir.path());
    permissions::set(&store, "test-echo:a", Some("c2"), false, Write::Confirm).unwrap();

    assert_eq!(ids(&d.registry.call("list_conversations", json!({})).await.unwrap()), ["c1"]);
    assert_eq!(ids(&d.registry.call("list_conversations", json!({"account": "test-echo:a"})).await.unwrap()), ["c1"]);
    assert!(d.registry.call("read_messages", json!({"account": "test-echo:a", "conversation": "c1"})).await.is_ok());
    let result = d.registry.call("read_messages", json!({"account": "test-echo:a", "conversation": "c2"})).await;
    assert!(refused(&result, "reading this conversation is not allowed"), "{result:?}");

    // An account that may read one conversation only shows that one.
    permissions::set(&store, "test-echo:a", None, false, Write::Confirm).unwrap();
    permissions::set(&store, "test-echo:a", Some("c2"), true, Write::Confirm).unwrap();
    assert_eq!(ids(&d.registry.call("list_conversations", json!({"account": "test-echo:a"})).await.unwrap()), ["c2"]);
    // test-echo's unread conversation is "c".
    assert_eq!(d.registry.call("unread", json!({})).await.unwrap(), json!({"items": []}));
}
