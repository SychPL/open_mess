//! The `om` CLI against an in-process API.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use open_mess::instance::InstanceLock;
use open_mess::tools::Registry;
use open_mess::ui::Opener;
use rmcp::ServiceExt;
use rmcp::model::ClientConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::oneshot;

const TOKEN: &str = "t0ken";

/// Serves the API on an ephemeral port until the returned sender is dropped.
async fn start() -> (u16, oneshot::Sender<()>) {
    let (port, stop, _) = start_with_window().await;
    (port, stop)
}

/// [`start`], with the opener of the configuration window.
async fn start_with_window() -> (u16, oneshot::Sender<()>, Opener) {
    let listener = open_mess::api::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, stopped) = oneshot::channel::<()>();
    let opener = Opener::default();
    tokio::spawn(open_mess::api::serve(listener, TOKEN.into(), Arc::new(Registry::with_builtin(Instant::now())), opener.clone(), async {
        let _ = stopped.await;
    }));
    (port, stop, opener)
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_config_opens_the_window() {
    let (port, _stop, opener) = start_with_window().await;
    let dir = running();
    let answer = |answer: Result<(), String>| {
        let opener = opener.clone();
        tokio::spawn(async move {
            let _ = opener.next().await.unwrap().send(answer);
        })
    };
    if cfg!(windows) {
        answer(Ok(()));
        let output = om(dir.path(), port, TOKEN, &["config"]);
        assert!(output.status.success(), "{}", stderr(&output));
        // A window that fails to open is reported, not taken for success.
        answer(Err("the window did not open: no WebView2".into()));
        let output = om(dir.path(), port, TOKEN, &["config"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains("no WebView2"), "{}", stderr(&output));
    } else {
        let output = om(dir.path(), port, TOKEN, &["config"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains("only on Windows"), "{}", stderr(&output));
    }
    // Without a daemon it says so.
    let output = om(dir.path(), free_port(), TOKEN, &["config"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
}

/// `om` with its own data folder, port and token.
fn om_command(data_dir: &Path, port: u16, token: &str, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_om"));
    command.args(args).env("OPEN_MESS_DATA_DIR", data_dir).env("OPEN_MESS_PORT", port.to_string()).env("OPEN_MESS_TOKEN", token);
    command
}

fn om(data_dir: &Path, port: u16, token: &str, args: &[&str]) -> Output {
    om_command(data_dir, port, token, args).output().unwrap()
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

/// A data folder whose instance lock is held, as by the daemon whose API a test serves.
struct Running {
    dir: TempDir,
    _lock: InstanceLock,
}

impl Running {
    fn path(&self) -> &Path {
        self.dir.path()
    }
}

fn running() -> Running {
    let dir = data_dir();
    let lock = InstanceLock::acquire(dir.path()).unwrap();
    Running { dir, _lock: lock }
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_status_json() {
    let (port, _stop) = start().await;
    let dir = running();
    let output = om(dir.path(), port, TOKEN, &["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_status_for_people() {
    let (port, _stop) = start().await;
    let dir = running();
    let output = om(dir.path(), port, TOKEN, &["status"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.lines().any(|line| line == format!("version: {}", env!("CARGO_PKG_VERSION"))), "{stdout}");
    assert!(stdout.lines().any(|line| line == "modules: []"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_unknown_tool() {
    let (port, _stop) = start().await;
    let dir = running();
    let output = om(dir.path(), port, TOKEN, &["nope"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(stderr(&output).contains("nope"));
}

#[tokio::test(flavor = "multi_thread")]
async fn write_tools_exist_over_mcp_and_cli() {
    let (port, _stop) = start().await;
    let dir = running();
    // The CLI names the tools the daemon offers when asked for one it doesn't.
    let unknown = stderr(&om(dir.path(), port, TOKEN, &["nope"]));
    let cli: BTreeSet<String> = unknown.split("available: ").nth(1).unwrap().trim().split(", ").map(str::to_owned).collect();

    rustls::crypto::ring::default_provider().install_default().ok();
    let config = StreamableHttpClientTransportConfig::with_uri(format!("http://127.0.0.1:{port}/mcp")).auth_header(TOKEN);
    let client = ClientConfig::default().serve(StreamableHttpClientTransport::from_config(config)).await.unwrap();
    let mcp: BTreeSet<String> = client.list_all_tools().await.unwrap().into_iter().map(|tool| tool.name.to_string()).collect();
    client.cancel().await.unwrap();

    assert_eq!(cli, mcp);
    for tool in ["send", "edit_message", "react", "pending"] {
        assert!(cli.contains(tool), "{tool}: {cli:?}");
    }
    let output = om(dir.path(), port, TOKEN, &["pending", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(serde_json::from_slice::<Value>(&output.stdout).unwrap(), json!({"items": []}));
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_bad_args() {
    let (port, _stop) = start().await;
    let dir = running();
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

#[tokio::test(flavor = "multi_thread")]
async fn cli_types_flags_by_the_schema() {
    let (port, _stop) = start().await;
    let dir = running();
    // A string parameter keeps a value that looks like a number, as Teams message ids do.
    let output = om(dir.path(), port, TOKEN, &["pending", "--account", "1800000000300"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stderr(&output).contains(r#"unknown account "1800000000300""#), "{}", stderr(&output));
    // An integer parameter gets a number.
    let output = om(dir.path(), port, TOKEN, &["pending", "--limit", "5", "--json"]);
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
    // The program that took the daemon's port: it must never see a request, nor the token.
    let other_program = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = other_program.local_addr().unwrap().port();
    let output = om(dir.path(), port, TOKEN, &["status"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains(&format!("open_mess daemon is not running - start it with: om daemon\nlast start failed: {reason}")),
        "{}",
        stderr(&output)
    );
    other_program.set_nonblocking(true).unwrap();
    assert!(other_program.accept().is_err());
}

#[test]
fn cli_takes_only_a_held_lock_as_a_running_daemon() {
    let dir = data_dir();
    std::fs::write(dir.path().join("last-start-error.txt"), "port 7341 is already in use\n").unwrap();
    // daemon.lock can't be opened (here: it is a folder), which proves nothing about a daemon.
    std::fs::create_dir(dir.path().join("daemon.lock")).unwrap();
    let other_program = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let output = om(dir.path(), other_program.local_addr().unwrap().port(), TOKEN, &["status"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    other_program.set_nonblocking(true).unwrap();
    assert!(other_program.accept().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_ignores_a_stale_start_error_while_the_daemon_runs() {
    let (port, _stop) = start().await;
    let dir = running();
    // A good start could not delete the file (say, a virus scanner held it open).
    std::fs::write(dir.path().join("last-start-error.txt"), "port 7341 is already in use\n").unwrap();
    let output = om(dir.path(), port, TOKEN, &["status"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn cli_never_sends_the_token_without_a_daemon_holding_the_lock() {
    // The daemon stopped cleanly (no start error) and another program, another Windows user's say,
    // took its port.
    let dir = data_dir();
    let other_program = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = other_program.local_addr().unwrap().port();
    for args in [&["status"][..], &["config"]] {
        let output = om(dir.path(), port, TOKEN, args);
        assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
        assert!(stderr(&output).contains("open_mess daemon is not running"), "{}", stderr(&output));
    }
    other_program.set_nonblocking(true).unwrap();
    assert!(other_program.accept().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_ignores_proxy_settings() {
    let (port, _stop) = start().await;
    let dir = running();
    // A proxy would get the token; this one doesn't even exist.
    let proxy = format!("http://127.0.0.1:{}", free_port());
    let mut command = om_command(dir.path(), port, TOKEN, &["status"]);
    for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        command.env(name, &proxy);
    }
    command.env_remove("NO_PROXY").env_remove("no_proxy");
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn cli_gives_up_on_a_port_that_never_answers() {
    let dir = running();
    // Takes connections (into the backlog) and never answers.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let started = Instant::now();
    let output = om(dir.path(), silent.local_addr().unwrap().port(), TOKEN, &["status"]);
    assert!(started.elapsed() < Duration::from_secs(45));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("did not answer"), "{}", stderr(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_bad_token() {
    let (port, _stop) = start().await;
    let dir = running();
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
fn cli_closed_stdout_is_fine() {
    let dir = data_dir();
    let mut child = om_command(dir.path(), 7400, TOKEN, &["mcp-command"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    // The reader goes away before om writes, like `om ... | head -0`.
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_reports_a_failed_write() {
    let (port, _stop) = start().await;
    let dir = running();
    // A stdout that refuses writes: a full device on Unix, a read-only file on Windows.
    #[cfg(unix)]
    let refusing = std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap();
    #[cfg(windows)]
    let refusing = {
        let path = dir.path().join("read-only");
        std::fs::write(&path, "").unwrap();
        std::fs::File::open(&path).unwrap()
    };
    let output = om_command(dir.path(), port, TOKEN, &["status", "--json"]).stdout(refusing).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("cannot write"), "{}", stderr(&output));
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
