//! The modules folder: approval by file and manifest hash, replacements and `status`.

mod common;

use std::time::Instant;

use common::{approve_all, echo, echo_rebuilt, echo_with_another_host, install, scan, store};
use open_mess::modules::{self, ApprovalReason, ModuleState};
use open_mess::tools::Registry;
use open_mess_host::Engine;
use serde_json::json;

#[tokio::test]
async fn new_module_waits_for_approval() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine) = (store(dir.path()), Engine::new().unwrap());
    install(dir.path(), "echo.wasm", &echo());
    let modules = scan(&store, &engine, dir.path()).await;
    assert_eq!(modules.state("test-echo"), Some(ModuleState::AwaitingApproval { reason: ApprovalReason::New }));
    assert!(modules.handle("test-echo").is_none());
}

#[tokio::test]
async fn approved_module_is_active() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine) = (store(dir.path()), Engine::new().unwrap());
    install(dir.path(), "echo.wasm", &echo());
    approve_all(&store, &scan(&store, &engine, dir.path()).await);
    let modules = scan(&store, &engine, dir.path()).await;
    assert_eq!(modules.state("test-echo"), Some(ModuleState::Active));
    assert_eq!(modules.handle("test-echo").unwrap().call("a", "echo", "{}").await.unwrap(), "{}");
}

#[tokio::test]
async fn changed_manifest_waits_for_approval() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine) = (store(dir.path()), Engine::new().unwrap());
    install(dir.path(), "echo.wasm", &echo());
    approve_all(&store, &scan(&store, &engine, dir.path()).await);
    let running = scan(&store, &engine, dir.path()).await;
    let old = running.handle("test-echo").unwrap().clone();

    // A rebuild that may reach one more host replaces the file.
    install(dir.path(), "echo.wasm", &echo_with_another_host());
    let modules = scan(&store, &engine, dir.path()).await;
    assert_eq!(modules.state("test-echo"), Some(ModuleState::AwaitingApproval { reason: ApprovalReason::ManifestChanged }));
    assert!(modules.handle("test-echo").is_none());
    assert_eq!(modules.list()[0].hosts, ["localhost", "extra.example.com"]);
    // The version that was approved and loaded keeps serving calls, with its own host list.
    assert_eq!(old.call("a", "echo", "{}").await.unwrap(), "{}");
    assert_eq!(old.manifest().unwrap().hosts, ["localhost"]);
}

#[tokio::test]
async fn changed_file_same_manifest_waits_for_approval() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine) = (store(dir.path()), Engine::new().unwrap());
    install(dir.path(), "echo.wasm", &echo());
    let first = scan(&store, &engine, dir.path()).await;
    approve_all(&store, &first);

    install(dir.path(), "echo.wasm", &echo_rebuilt());
    let modules = scan(&store, &engine, dir.path()).await;
    assert_eq!(modules.state("test-echo"), Some(ModuleState::AwaitingApproval { reason: ApprovalReason::FileChanged }));
    assert!(modules.handle("test-echo").is_none());
    assert_eq!(modules.list()[0].manifest_sha256, first.list()[0].manifest_sha256);
    assert_ne!(modules.list()[0].file_sha256, first.list()[0].file_sha256);
}

#[tokio::test]
async fn duplicate_name_is_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine) = (store(dir.path()), Engine::new().unwrap());
    install(dir.path(), "a.wasm", &echo());
    approve_all(&store, &scan(&store, &engine, dir.path()).await);

    // A second file with the same manifest name waits; the approved one keeps running.
    install(dir.path(), "b.wasm", &echo_with_another_host());
    let modules = scan(&store, &engine, dir.path()).await;
    assert_eq!(modules.state("test-echo"), Some(ModuleState::Active));
    assert!(modules.handle("test-echo").is_some());
    let states: Vec<_> = modules.list().into_iter().map(|info| (info.file, info.state)).collect();
    assert_eq!(states, [("a.wasm".to_string(), ModuleState::Active), ("b.wasm".to_string(), ModuleState::PendingReplacement)]);

    // Approving the replacement makes it the module, and the old file the one waiting.
    let replacement = modules.list().into_iter().find(|info| info.file == "b.wasm").unwrap();
    modules::approve(&store, "test-echo", &replacement.file_sha256, &replacement.manifest_sha256).unwrap();
    let modules = scan(&store, &engine, dir.path()).await;
    let states: Vec<_> = modules.list().into_iter().map(|info| (info.file, info.state)).collect();
    assert_eq!(states, [("a.wasm".to_string(), ModuleState::PendingReplacement), ("b.wasm".to_string(), ModuleState::Active)]);
    assert_eq!(modules.handle("test-echo").unwrap().manifest().unwrap().hosts, ["localhost", "extra.example.com"]);

    // Two new files: exactly one waits for approval, the other is its replacement.
    let fresh = tempfile::tempdir().unwrap();
    let fresh_store = common::store(fresh.path());
    install(fresh.path(), "a.wasm", &echo());
    install(fresh.path(), "b.wasm", &echo_rebuilt());
    let modules = scan(&fresh_store, &engine, fresh.path()).await;
    let states: Vec<_> = modules.list().into_iter().map(|info| info.state).collect();
    assert_eq!(states, [ModuleState::AwaitingApproval { reason: ApprovalReason::New }, ModuleState::PendingReplacement]);
}

#[tokio::test]
async fn disabled_module_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine) = (store(dir.path()), Engine::new().unwrap());
    install(dir.path(), "echo.wasm", &echo());
    approve_all(&store, &scan(&store, &engine, dir.path()).await);
    modules::set_enabled(&store, "test-echo", false).unwrap();
    let modules = scan(&store, &engine, dir.path()).await;
    assert_eq!(modules.state("test-echo"), Some(ModuleState::Disabled));
    assert!(modules.handle("test-echo").is_none());
}

#[tokio::test]
async fn status_lists_modules() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine) = (store(dir.path()), Engine::new().unwrap());
    install(dir.path(), "echo.wasm", &echo());
    install(dir.path(), "broken.wasm", b"not a component");
    approve_all(&store, &scan(&store, &engine, dir.path()).await);
    let registry = Registry::new(Instant::now(), scan(&store, &engine, dir.path()).await);

    let status = registry.call("status", json!({})).await.unwrap();
    let modules = status["modules"].as_array().unwrap();
    let echo = modules.iter().find(|module| module["name"] == "test-echo").unwrap();
    assert_eq!(
        *echo,
        json!({"name": "test-echo", "file": "echo.wasm", "version": env!("CARGO_PKG_VERSION"), "platform": "test", "state": "active", "hosts": ["localhost"]})
    );
    let broken = modules.iter().find(|module| module["file"] == "broken.wasm").unwrap();
    assert!(broken["state"].as_str().unwrap().starts_with("not a valid module"), "{broken}");
}
