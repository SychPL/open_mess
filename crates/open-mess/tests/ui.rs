//! The configuration window's commands, without a webview: accounts, modules and their approval.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::sync::Arc;
use std::time::Instant;

use common::{Daemon, ECHO, daemon, echo, echo_rebuilt, echo_with_another_host, info, install, scan, store};
use open_mess::pending::{Kind, Pending, State};
use open_mess::ui::UiState;
use open_mess::ui::commands::{self, MODULE_CHANGED, ModuleDto};
use open_mess_host::Engine;
use serde_json::json;
use tempfile::TempDir;

fn state(d: &Daemon) -> UiState {
    UiState {
        accounts: Arc::clone(&d.accounts),
        pending: Arc::clone(d.registry.pending().unwrap()),
        started: Instant::now(),
        port: 7341,
        token: "t0ken".into(),
    }
}

fn module<'a>(list: &'a [ModuleDto], status: &str) -> &'a ModuleDto {
    list.iter().find(|module| module.status == status).unwrap_or_else(|| panic!("no {status} module in {list:?}"))
}

/// A daemon whose modules folder has `files`, none approved.
async fn unapproved(files: &[(&str, Vec<u8>)]) -> (TempDir, UiState) {
    let dir = tempfile::tempdir().unwrap();
    for (file, bytes) in files {
        install(dir.path(), file, bytes);
    }
    let engine = Engine::new().unwrap();
    let modules = scan(&store(dir.path()), &engine, dir.path()).await;
    let accounts = common::accounts(dir.path(), modules, keyring_core::mock::Store::new().unwrap());
    let pending = Arc::new(Pending::new(Arc::clone(&accounts)));
    (dir, UiState { accounts, pending, started: Instant::now(), port: 7341, token: "t0ken".into() })
}

#[tokio::test]
async fn module_dto_lists_hosts_and_identity() {
    let (_dir, state) = unapproved(&[("teams.wasm", std::fs::read(common::wasm("open_mess_teams")).unwrap())]).await;
    let list = commands::modules(&state);
    assert_eq!(list.len(), 1);
    let teams = &list[0];
    assert_eq!((teams.name.as_str(), teams.status, teams.detail.as_str()), ("teams", "awaiting-approval", "czeka na zatwierdzenie: nowy moduł"));
    assert!(teams.hosts.contains(&"*.sharepoint.com".to_string()), "{:?}", teams.hosts);
    assert_eq!(teams.sign_in_hosts, ["login.microsoftonline.com"]);
    let wam = teams.wam.as_ref().unwrap();
    assert_eq!(wam.client_id, "1fec8e78-bce4-4aaf-ab1b-5451cc387264");
    assert_eq!(wam.scopes.len(), 3);
    assert_eq!(teams.file_sha256.len(), 64);
    assert_eq!(teams.manifest_sha256.len(), 64);
    // A new module takes over nothing.
    assert!(!teams.takes_over);
    assert!(teams.changes.is_empty());
}

#[tokio::test]
async fn stale_module_approval_is_refused() {
    let (dir, state) = unapproved(&[("echo.wasm", echo())]).await;
    let shown = commands::modules(&state)[0].clone();
    // The file changes after the window showed it.
    install(dir.path(), "echo.wasm", &echo_rebuilt());
    let refused = commands::approve_module(&state, "test-echo", &shown.file_sha256, &shown.manifest_sha256).await;
    assert_eq!(refused, Err(MODULE_CHANGED.to_string()));
    assert!(state.accounts.modules().handle("test-echo").is_none());
    // Made-up hashes are refused the same way.
    assert_eq!(commands::approve_module(&state, "test-echo", "00", "00").await, Err(MODULE_CHANGED.to_string()));

    // Refreshed, the window shows the new file, and approving that loads it.
    commands::rescan_modules(&state).await.unwrap();
    let fresh = commands::modules(&state)[0].clone();
    assert_ne!(fresh.file_sha256, shown.file_sha256);
    commands::approve_module(&state, "test-echo", &fresh.file_sha256, &fresh.manifest_sha256).await.unwrap();
    assert_eq!(commands::modules(&state)[0].status, "active");
    assert!(state.accounts.modules().handle("test-echo").is_some());

    // Turned off and on again without a restart.
    commands::set_module_enabled(&state, "test-echo", false).await.unwrap();
    assert_eq!(commands::modules(&state)[0].status, "disabled");
    assert!(state.accounts.modules().handle("test-echo").is_none());
    commands::set_module_enabled(&state, "test-echo", true).await.unwrap();
    assert!(state.accounts.modules().handle("test-echo").is_some());
    assert!(commands::set_module_enabled(&state, "nope", true).await.is_err());
}

#[tokio::test]
async fn rescan_keeps_running_version_until_approved() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    let waiting = state.pending.create(Kind::Send, "test-echo:a", "c1", json!({"conversation": "c1", "text": "hi"})).await.unwrap();

    // A rebuild that may reach one more host replaces the file.
    install(d.dir.path(), "echo.wasm", &echo_with_another_host());
    commands::rescan_modules(&state).await.unwrap();
    let running = d.accounts.modules().handle("test-echo").unwrap();
    assert_eq!(running.manifest().unwrap().hosts, ["localhost"]);
    let list = commands::modules(&state);
    assert_eq!(list.len(), 2);
    assert_eq!(module(&list, "active").hosts, ["localhost"]);
    let replacement = module(&list, "replacement").clone();
    assert_eq!(replacement.hosts, ["localhost", "extra.example.com"]);
    assert_eq!(replacement.changes, ["nowy host: extra.example.com"]);
    assert!(replacement.takes_over);
    // The account and its waiting action are untouched.
    assert_eq!(commands::accounts(&state).unwrap()[0].state, "online");
    assert_eq!(state.pending.state(&waiting.action).unwrap(), State::Waiting);
    // Another rescan changes nothing.
    commands::rescan_modules(&state).await.unwrap();
    assert_eq!(commands::modules(&state).len(), 2);

    // Approved, the new file takes over; what the old one described for approval expires.
    commands::approve_module(&state, "test-echo", &replacement.file_sha256, &replacement.manifest_sha256).await.unwrap();
    let list = commands::modules(&state);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, "active");
    assert_eq!(d.accounts.modules().handle("test-echo").unwrap().manifest().unwrap().hosts, ["localhost", "extra.example.com"]);
    assert_eq!(state.pending.state(&waiting.action).unwrap(), State::Expired);
    assert!(state.pending.waiting().is_empty());
    // test-echo restores only its "known" account; the account was asked again.
    assert_eq!(commands::accounts(&state).unwrap()[0].state, "auth-required");
}

#[tokio::test]
async fn remove_account_from_window() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    d.accounts.add("test-echo", info("b")).unwrap();
    let state = state(&d);
    let accounts = commands::accounts(&state).unwrap();
    assert_eq!(
        accounts.iter().map(|account| (account.id.as_str(), account.platform.as_deref(), account.display_name.as_str())).collect::<Vec<_>>(),
        [("test-echo:a", Some("test"), "User a"), ("test-echo:b", Some("test"), "User b")]
    );

    commands::remove_account(&state, "test-echo:a").await.unwrap();
    let ids: Vec<String> = commands::accounts(&state).unwrap().into_iter().map(|account| account.id).collect();
    assert_eq!(ids, ["test-echo:b"]);
    assert!(commands::remove_account(&state, "test-echo:a").await.is_err());
}

#[tokio::test]
async fn status_and_mcp_command() {
    let d = daemon(&[ECHO]).await;
    install(d.dir.path(), "broken.wasm", b"not a component");
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    commands::rescan_modules(&state).await.unwrap();
    let status = commands::status(&state).unwrap();
    assert_eq!((status.accounts, status.accounts_online, status.modules, status.modules_active), (1, 1, 2, 1));
    assert_eq!(status.port, 7341);
    assert_eq!(status.problems.len(), 1);
    assert!(status.problems[0].starts_with("broken.wasm (broken.wasm): to nie jest prawidłowy moduł"), "{:?}", status.problems);
    assert_eq!(
        commands::mcp_command(&state),
        "claude mcp add --transport http --scope user open_mess http://127.0.0.1:7341/mcp --header \"Authorization: Bearer t0ken\""
    );
}
