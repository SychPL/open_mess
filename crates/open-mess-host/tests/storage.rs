//! Module storage with its quota, and the sign-in imports (refused until the configuration window
//! exists).

// clippy.toml allows unwrap in #[test] functions only; the helpers here are test code too.
#![allow(clippy::unwrap_used)]

mod common;

use common::{context_in, wasm};
use open_mess_host::{Engine, ModuleHandle, limits};

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

#[tokio::test]
async fn sign_in_is_refused_for_now() {
    let dir = tempfile::tempdir().unwrap();
    let module = echo(dir.path()).await;
    let refused = "refused: sign-in UI not available";
    assert_eq!(module.call("a", "sign-in", "{}").await.unwrap(), format!(r#"{{"open-page":"{refused}","wam-token":"{refused}"}}"#));
}
