//! The `om` CLI against an in-process API.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use open_mess::tools::Registry;
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::oneshot;

const TOKEN: &str = "t0ken";

/// Serves the API on an ephemeral port until the returned sender is dropped.
async fn start() -> (u16, oneshot::Sender<()>) {
    let listener = open_mess::api::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(open_mess::api::serve(listener, TOKEN.into(), Arc::new(Registry::with_builtin(Instant::now())), async {
        let _ = stopped.await;
    }));
    (port, stop)
}

/// Runs `om` with its own data folder, port and token.
fn om(data_dir: &Path, port: u16, token: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_om"))
        .args(args)
        .env("OPEN_MESS_DATA_DIR", data_dir)
        .env("OPEN_MESS_PORT", port.to_string())
        .env("OPEN_MESS_TOKEN", token)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A port nobody listens on.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn data_dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_status_json() {
    let (port, _stop) = start().await;
    let dir = data_dir();
    let output = om(dir.path(), port, TOKEN, &["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_status_for_people() {
    let (port, _stop) = start().await;
    let dir = data_dir();
    let output = om(dir.path(), port, TOKEN, &["status"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.lines().any(|line| line == format!("version: {}", env!("CARGO_PKG_VERSION"))), "{stdout}");
    assert!(stdout.lines().any(|line| line == "modules: []"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_unknown_tool() {
    let (port, _stop) = start().await;
    let dir = data_dir();
    let output = om(dir.path(), port, TOKEN, &["nope"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(stderr(&output).contains("nope"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_bad_args() {
    let (port, _stop) = start().await;
    let dir = data_dir();
    // status takes no parameters: the API refuses them.
    let output = om(dir.path(), port, TOKEN, &["status", "--verbose", "true"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stderr(&output).contains("verbose"));
    // Malformed on our side.
    for args in [&["status", "--json-args", "{not json"][..], &["status", "--verbose"], &["status", "stray"], &["status", "--json-args", "[]"]] {
        let output = om(dir.path(), port, TOKEN, args);
        assert_eq!(output.status.code(), Some(3), "{args:?}: {}", stderr(&output));
    }
    let output = om(dir.path(), port, TOKEN, &["status", "--json-args", "{}"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn cli_without_daemon() {
    let dir = data_dir();
    let started = Instant::now();
    let output = om(dir.path(), free_port(), TOKEN, &["status"]);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("open_mess daemon is not running - start it with: om daemon"));
    assert!(!stderr(&output).contains("last start failed"));
}

#[test]
fn cli_without_daemon_shows_the_last_start_error() {
    let dir = data_dir();
    let reason = "port 7341 is already in use - change it in the configuration window or set another port";
    std::fs::write(dir.path().join("last-start-error.txt"), format!("{reason}\n")).unwrap();
    let output = om(dir.path(), free_port(), TOKEN, &["status"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains(&format!("open_mess daemon is not running - start it with: om daemon\nlast start failed: {reason}")),
        "{}",
        stderr(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_bad_token() {
    let (port, _stop) = start().await;
    let dir = data_dir();
    let output = om(dir.path(), port, "wrong", &["status"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr(&output);
    assert!(stderr.contains("401") || stderr.contains("token"), "{stderr}");
}

#[test]
fn cli_mcp_command() {
    let dir = data_dir();
    let output = om(dir.path(), 7400, TOKEN, &["mcp-command"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("claude mcp add --transport http"), "{stdout}");
    assert!(stdout.contains("open_mess http://127.0.0.1:7400/mcp"), "{stdout}");
    assert!(stdout.contains("\"Authorization: Bearer t0ken\""), "{stdout}");
}

#[test]
fn cli_version_and_bad_usage() {
    let dir = data_dir();
    let output = om(dir.path(), free_port(), TOKEN, &["--version"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), format!("om {}", env!("CARGO_PKG_VERSION")));
    let output = om(dir.path(), free_port(), TOKEN, &["--no-such-flag"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
}
