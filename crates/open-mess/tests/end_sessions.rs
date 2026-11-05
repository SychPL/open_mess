//! What others see of a module's sessions ends before the module goes (spec
//! 2026-10-29-live-captions, 4.3): `end-sessions` for every account when the daemon stops, an
//! update pauses the modules or a scan drops a module, and an immediate `restore` after a crash so a
//! new instance starts. The test-echo module records both exports in its storage; its tool `ended`
//! returns them.

// clippy.toml allows unwrap in #[test] functions only; the helpers here are test code too.
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use open_mess::modules;
use open_mess_host::ModuleHandle;
use serde_json::{Value, json};

async fn ended(handle: &ModuleHandle) -> Value {
    serde_json::from_str(&handle.call("a", "ended", "{}").await.unwrap()).unwrap()
}

fn echo(d: &common::Daemon) -> ModuleHandle {
    d.accounts.modules().handle("test-echo").unwrap()
}

#[tokio::test]
async fn every_account_of_every_module_ends_its_sessions() {
    let d = common::daemon(&[common::ECHO]).await;
    d.accounts.add("test-echo", common::info("a")).unwrap();
    d.accounts.add("test-echo", common::info("b")).unwrap();
    d.accounts.end_sessions(&d.accounts.modules().handles()).await;
    let mut accounts: Vec<String> = serde_json::from_value(ended(&echo(&d)).await["ended"].clone()).unwrap();
    accounts.sort();
    assert_eq!(accounts, ["a", "b"]);
}

#[tokio::test]
async fn a_scan_that_turns_a_module_off_ends_its_sessions_first() {
    let d = common::daemon(&[common::ECHO]).await;
    d.accounts.add("test-echo", common::info("a")).unwrap();
    let store = common::store(d.dir.path());
    modules::set_enabled(&store, "test-echo", false).unwrap();
    assert_eq!(d.accounts.rescan_modules(None).await.unwrap(), ["test-echo"]);
    assert!(d.accounts.modules().handle("test-echo").is_none());
    // The version that ran ended the account's sessions before it went; its storage tells.
    modules::set_enabled(&store, "test-echo", true).unwrap();
    d.accounts.rescan_modules(None).await.unwrap();
    assert_eq!(ended(&echo(&d)).await["ended"], json!(["a"]));
}

#[tokio::test]
async fn a_crash_restores_the_accounts_at_once() {
    let d = common::daemon(&[common::ECHO]).await;
    d.accounts.add("test-echo", common::info("a")).unwrap();
    tokio::spawn(Arc::clone(&d.accounts).restore_after_crashes());
    let handle = echo(&d);
    assert!(handle.call("a", "trap", "{}").await.is_err());
    // A new instance came up by itself and ran `restore` for the account.
    for _ in 0..50 {
        if ended(&handle).await["restored"] == json!(["a"]) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no restore after the crash: {}", ended(&handle).await);
}
