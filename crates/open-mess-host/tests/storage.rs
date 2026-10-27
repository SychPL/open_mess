//! Module storage with its quota.

// clippy.toml allows unwrap in #[test] functions only; the helpers here are test code too.
#![allow(clippy::unwrap_used)]

mod common;

use common::{context_in, context_with_events, wasm};
use open_mess_host::{Engine, ModuleContext, ModuleHandle, limits};

#[tokio::test]
async fn imports_reach_nothing_while_the_manifest_loads() {
    // test-echo's manifest export writes to storage and reports a session.
    let dir = tempfile::tempdir().unwrap();
    let (context, mut events) = context_with_events();
    let context = ModuleContext { data_dir: dir.path().to_path_buf(), ..context };
    let module = Engine::new().unwrap().load(&wasm("open_mess_test_echo"), context).await.unwrap();
    assert!(!dir.path().join("modules").exists());
    assert!(events.try_recv().is_err());
    drop(module);
}

/// With the default call limit: a 16 MiB value in a debug build can take a while.
async fn echo(data_dir: &std::path::Path) -> ModuleHandle {
    let engine = Engine::new().unwrap();
    engine.load(&wasm("open_mess_test_echo"), context_in(data_dir.to_path_buf())).await.unwrap()
}

async fn store(module: &ModuleHandle, size: usize) -> String {
    module.call("a", "store", &format!(r#"{{"size":{size}}}"#)).await.unwrap()
}

#[tokio::test]
async fn storage_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let module = echo(dir.path()).await;
    assert_eq!(store(&module, 100).await, r#"{"len":100}"#);
    assert!(dir.path().join("modules").join("test-echo.sqlite3").exists());
    // Another load of the module finds the same storage.
    let again = echo(dir.path()).await;
    assert_eq!(store(&again, 5).await, r#"{"len":5}"#);
}

#[tokio::test]
async fn quota_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let module = echo(dir.path()).await;
    // The quota counts key and value bytes: key "k" plus the value.
    let fits = limits::STORAGE_QUOTA - 1;
    assert_eq!(store(&module, fits).await, format!(r#"{{"len":{fits}}}"#));
    assert_eq!(store(&module, fits + 1).await, format!(r#"{{"error":"quota-exceeded","len":{fits}}}"#));
}
