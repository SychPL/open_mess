//! The write tools `send`, `edit_message` and `react`, a module's own write tool (test-echo's
//! `poke`, spec 2026-10-29-live-captions 5.1), and `pending`, through the tool registry.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Daemon, ECHO, daemon, info};
use open_mess::modules;
use open_mess::pending::{Kind, MAX_WAITING, State};
use open_mess::permissions::{self, Write};
use open_mess::tools::{GateMode, ToolError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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
        ("send", json!({"account": "test-echo:a", "conversation": "c".repeat(1025), "text": "hi"})),
        ("react", json!({"account": "test-echo:a", "conversation": "c1", "message": "m".repeat(1025), "reaction": "like"})),
        ("pending", json!({"account": "test-echo:nobody"})),
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

/// test-echo's tool directly, bypassing the core.
async fn echo(d: &Daemon, tool: &str, args: Value) -> Value {
    let handle = d.accounts.modules().handle("test-echo").unwrap();
    serde_json::from_str(&handle.call("a", tool, &args.to_string()).await.unwrap()).unwrap()
}

/// What test-echo was asked to write, in order.
async fn writes(d: &Daemon) -> Vec<Value> {
    echo(d, "writes", json!({})).await["items"].as_array().unwrap().clone()
}

fn poke(text: &str) -> Value {
    json!({"account": "test-echo:a", "conversation": "c1", "text": text})
}

fn action(result: &Value) -> String {
    result["action"].as_str().unwrap().to_owned()
}

/// What `pending` says of the newest action.
async fn newest(d: &Daemon) -> Value {
    d.registry.call("pending", json!({"limit": 1})).await.unwrap()["items"][0].clone()
}

fn is_changing(result: &Result<Value, ToolError>) -> bool {
    matches!(result, Err(ToolError::Failed(why)) if why.contains("being updated or reloaded"))
}

#[tokio::test]
async fn module_write_tool_always_waits_for_the_approval() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let (store, pending) = (common::store(d.dir.path()), d.registry.pending().unwrap());
    // Listed, the account it writes as required; one that names no conversation is not.
    let listed = d.registry.list().into_iter().find(|tool| tool.name == "test_echo_poke").unwrap();
    assert_eq!(listed.input_schema["required"], json!(["conversation", "account"]));
    assert!(!d.registry.list().iter().any(|tool| tool.name == "test_echo_nudge"));
    let result = d.registry.call("test_echo_poke", json!({"conversation": "c1"})).await;
    assert!(matches!(&result, Err(ToolError::InvalidArgs(why)) if why == "account is required"), "{result:?}");

    // A write needs the write permission, not the read one, and waits for the user.
    permissions::set(&store, "test-echo:a", None, false, Write::Confirm).unwrap();
    let first = d.registry.call("test_echo_poke", poke("hi")).await.unwrap();
    assert_eq!(first["state"], "waiting");
    let preview = pending.waiting()[0].clone();
    assert_eq!(preview.kind, Kind::ModuleTool("test_echo_poke".into()));
    assert_eq!(preview.description.as_deref(), Some("Pokes the conversation. It records its arguments as a write."));
    assert_eq!(
        (preview.conversation_title.as_str(), preview.participants.as_slice(), preview.arguments.as_slice()),
        ("Echo c1", ["Pat".to_string()].as_slice(), ["text: hi".to_string()].as_slice())
    );
    assert_eq!((preview.text, preview.target_text), (None, None));
    // Writing on its own waits too; never is refused.
    permissions::set(&store, "test-echo:a", None, true, Write::Autonomous).unwrap();
    assert_eq!(d.registry.call("test_echo_poke", poke("again")).await.unwrap()["state"], "waiting");
    permissions::set(&store, "test-echo:a", None, true, Write::Never).unwrap();
    let refused = d.registry.call("test_echo_poke", poke("no")).await;
    assert!(matches!(&refused, Err(ToolError::NotAllowed(why)) if why == "writing to this conversation is not allowed"), "{refused:?}");
    assert!(writes(&d).await.is_empty());

    // Approved, the module gets its own tool's name and the arguments without the account.
    permissions::set(&store, "test-echo:a", None, false, Write::Confirm).unwrap();
    assert_eq!(pending.decide(&action(&first), true).await.unwrap(), State::Done);
    assert_eq!(writes(&d).await, [json!({"tool": "poke", "args": {"conversation": "c1", "text": "hi"}})]);
    // `pending` names it as the agent called it.
    let done = d.registry.call("pending", json!({"state": "done"})).await.unwrap();
    assert_eq!((&done["items"][0]["kind"], &done["items"][0]["action"]), (&json!("test_echo_poke"), &first["action"]));
}

#[tokio::test]
async fn module_write_approval_binds_to_the_title_not_the_participants() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let pending = d.registry.pending().unwrap();
    // People joining the meeting while the approval waits do not void it.
    let joined = d.registry.call("test_echo_poke", poke("one")).await.unwrap();
    echo(&d, "set-target", json!({"conversation": "c1", "participants": [["p1", "Pat"], ["p2", "Sam"]]})).await;
    assert_eq!(pending.decide(&action(&joined), true).await.unwrap(), State::Done);
    // Another title does: the approval showed another conversation.
    let renamed = d.registry.call("test_echo_poke", poke("two")).await.unwrap();
    echo(&d, "set-target", json!({"conversation": "c1", "title": "Another meeting"})).await;
    assert_eq!(pending.decide(&action(&renamed), true).await.unwrap(), State::Voided);
    assert_eq!(newest(&d).await["detail"], "the conversation's title changed after the approval; nothing was done");
    assert_eq!(writes(&d).await.len(), 1);
}

#[tokio::test]
async fn module_write_tool_is_a_write_for_the_gate_and_the_module_block() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let pending = d.registry.pending().unwrap();
    // While an update's new version is checked, as `send` (known by its declaration).
    d.registry.gate().set(GateMode::WritesBlocked);
    let result = d.registry.call("test_echo_poke", poke("hi")).await;
    assert!(matches!(&result, Err(ToolError::Failed(why)) if why == "update in progress, try again shortly"), "{result:?}");
    assert!(d.registry.call("test_echo_peek", json!({"account": "test-echo:a"})).await.is_ok());
    d.registry.gate().set(GateMode::Open);

    // While the modules change, no action is made and none approved calls its module.
    let waiting = d.registry.call("test_echo_poke", poke("hi")).await.unwrap();
    {
        let _block = d.accounts.module_writes().block();
        assert!(is_changing(&d.registry.call("test_echo_poke", poke("hi")).await));
        assert_eq!(pending.decide(&action(&waiting), true).await.unwrap(), State::Failed);
        assert!(newest(&d).await["detail"].as_str().unwrap().contains("being updated or reloaded"));
    }
    assert!(writes(&d).await.is_empty());
    assert_eq!(d.registry.call("test_echo_poke", poke("hi")).await.unwrap()["state"], "waiting");
}

#[tokio::test]
async fn an_approval_runs_only_the_tool_it_showed() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let waiting = d.registry.call("test_echo_poke", poke("hi")).await.unwrap();
    // As if the module had been replaced since with one whose tool is declared otherwise: the
    // approval's record of the tool no longer matches it.
    let db = rusqlite::Connection::open(d.dir.path().join("state.sqlite3")).unwrap();
    db.execute("UPDATE pending SET fingerprint = substr(fingerprint, 1, 65) || 'another tool' WHERE id = ?1", [action(&waiting)]).unwrap();
    assert_eq!(d.registry.pending().unwrap().decide(&action(&waiting), true).await.unwrap(), State::Failed);
    assert_eq!(newest(&d).await["detail"], "the module's tool changed after the approval; nothing was done");
    assert!(writes(&d).await.is_empty());
}

#[tokio::test]
async fn a_scan_blocks_module_writes_until_the_old_version_left() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    // test-echo's `end-sessions` of this account takes 2 s.
    d.accounts.add("test-echo", info("slow")).unwrap();
    // A new file of test-echo, approved: the scan swaps it in and the old version ends its sessions.
    let rebuilt = common::echo_rebuilt();
    common::install(d.dir.path(), "echo.wasm", &rebuilt);
    let manifest = d.accounts.modules().list()[0].manifest_sha256.clone();
    let file: String = Sha256::digest(&rebuilt).iter().map(|byte| format!("{byte:02x}")).collect();
    modules::approve(common::store(d.dir.path()).conn(), "test-echo", &file, &manifest).unwrap();
    let scan = |accounts: Arc<open_mess::accounts::Accounts>| tokio::spawn(async move { accounts.rescan_modules(None).await.unwrap() });
    let first = scan(Arc::clone(&d.accounts));
    for _ in 0..1000 {
        if d.accounts.module_writes().blocked() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(is_changing(&d.registry.call("test_echo_poke", poke("hi")).await));
    // A second scan meanwhile waits for the first and does not lift its block.
    let second = scan(Arc::clone(&d.accounts));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(is_changing(&d.registry.call("test_echo_poke", poke("hi")).await));
    assert_eq!(first.await.unwrap(), ["test-echo"]);
    second.await.unwrap();
    // The old version ended both accounts' sessions; the new one takes writes.
    assert!(!d.accounts.module_writes().blocked());
    assert_eq!(d.registry.call("test_echo_poke", poke("hi")).await.unwrap()["state"], "waiting");
}
