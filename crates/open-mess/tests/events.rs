//! The event bus: module events reach subscribers with the core's account ids, and session
//! changes update the accounts table.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{install, running, wasm};
use open_mess::accounts::Accounts;
use open_mess::events::{Bus, Event};
use open_mess_host::{AccountInfo, Engine, ModuleHandle, SessionState, limits};
use tempfile::TempDir;
use tokio::sync::{broadcast, mpsc};

/// test-echo with account "a", its events forwarded to a bus.
async fn echo_on_a_bus() -> (TempDir, Arc<Accounts>, ModuleHandle, broadcast::Receiver<Event>) {
    let dir = tempfile::tempdir().unwrap();
    install(dir.path(), "echo.wasm", &std::fs::read(wasm("open_mess_test_echo")).unwrap());
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let (sender, events) = mpsc::channel(64);
    let modules = running(dir.path(), &engine, sender).await;
    let echo = modules.handle("test-echo").unwrap().clone();
    let accounts = common::accounts(dir.path(), modules, keyring_core::mock::Store::new().unwrap());
    accounts.add("test-echo", AccountInfo { id: "a".into(), display_name: "A".into() }).unwrap();
    let bus = Bus::default();
    let subscriber = bus.subscribe();
    tokio::spawn(bus.forward(events, Arc::clone(&accounts)));
    (dir, accounts, echo, subscriber)
}

async fn next(subscriber: &mut broadcast::Receiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(5), subscriber.recv()).await.unwrap().unwrap()
}

#[tokio::test]
async fn message_events_reach_subscribers() {
    let (_dir, _accounts, echo, mut subscriber) = echo_on_a_bus().await;
    // test-echo's `message` tool reports account "a" online, then a message that mentions the user.
    echo.call("a", "message", r#"{"text":"m-42"}"#).await.unwrap();
    assert_eq!(next(&mut subscriber).await, Event::Session { account: "test-echo:a".into(), state: SessionState::Online });
    assert_eq!(
        next(&mut subscriber).await,
        Event::Message {
            account: "test-echo:a".into(),
            conversation: "c".into(),
            message: "m-42".into(),
            own: false,
            mentions_me: true,
            direct: false
        }
    );
}

#[tokio::test]
async fn session_state_updates_account() {
    let (_dir, accounts, echo, mut subscriber) = echo_on_a_bus().await;
    let state = || accounts.get("test-echo:a").unwrap().unwrap().state;
    assert_eq!(state(), "online");
    // test-echo reports an account it signs out as offline.
    echo.sign_out("a").await.unwrap();
    assert_eq!(next(&mut subscriber).await, Event::Session { account: "test-echo:a".into(), state: SessionState::Offline });
    assert_eq!(state(), "offline");
    echo.call("a", "message", r#"{"text":"m"}"#).await.unwrap();
    next(&mut subscriber).await;
    assert_eq!(state(), "online");
}
