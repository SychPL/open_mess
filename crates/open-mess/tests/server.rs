//! `om daemon` as a server next to Paperclip (spec 2026-11-03-paperclip-plugin, 5): secrets in the
//! data folder, every interface with `OPEN_MESS_LISTEN=0.0.0.0`, generated tokens, `om
//! server-tokens`, and SIGTERM from `docker stop`.
//!
//! Unix only: that is where the server runs; Windows keeps Credential Manager and its window.
#![cfg(unix)]
// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use open_mess::store::Store;
use serde_json::Value;

fn om(data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_om"));
    command.env("OPEN_MESS_DATA_DIR", data_dir).env_remove("OPEN_MESS_TOKEN").env_remove("OPEN_MESS_LISTEN").env_remove("OPEN_MESS_PORT");
    command.stdin(Stdio::null());
    command
}

fn run(data_dir: &Path, args: &[&str]) -> Output {
    om(data_dir).args(args).output().unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Starts a server daemon on every interface and returns it with its `listening` line.
fn start_server(data_dir: &Path) -> (Child, String) {
    let mut child = om(data_dir).arg("daemon").env("OPEN_MESS_LISTEN", "0.0.0.0").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (line_tx, line_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });
    let line = line_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    (child, line)
}

/// The daemon's tokens, as its secrets file keeps them.
fn tokens(data_dir: &Path) -> (String, String) {
    let file: Value = serde_json::from_str(&std::fs::read_to_string(data_dir.join("secrets.json")).unwrap()).unwrap();
    let token = |user: &str| file["open_mess"][user].as_str().unwrap().to_owned();
    (token("local-api"), token("admin-api"))
}

/// SIGTERM, as `docker stop` sends it, and the exit code once the daemon is gone.
fn terminate(mut daemon: Child) -> Option<i32> {
    let status = Command::new("kill").args(["-TERM", &daemon.id().to_string()]).status().unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            return status.code();
        }
        assert!(Instant::now() < deadline, "SIGTERM did not stop the daemon");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_server_daemon_keeps_its_tokens_in_the_data_folder() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    Store::open(&dir.path().join("state.sqlite3")).unwrap().set_config("port", &port.to_string()).unwrap();
    // Before the first start there is nothing to print.
    let early = run(dir.path(), &["server-tokens"]);
    assert_eq!(early.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&early.stderr).contains("start the daemon first"));

    let (daemon, line) = start_server(dir.path());
    assert_eq!(line, format!("open_mess daemon listening on http://0.0.0.0:{port}/mcp"));
    let (agents, admin) = tokens(dir.path());
    for token in [&agents, &admin] {
        assert!(token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()), "{token}");
    }
    assert_ne!(agents, admin);
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(std::fs::metadata(dir.path().join("secrets.json")).unwrap().permissions().mode() & 0o777, 0o600);

    // The CLI in the container finds the agents' token in the file, and reaches the daemon
    // through 127.0.0.1.
    let status = run(dir.path(), &["status", "--json"]);
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    let printed = String::from_utf8(run(dir.path(), &["server-tokens"]).stdout).unwrap();
    assert_eq!(printed, format!("agents: {agents}\nadmin: {admin}\n"));

    assert_eq!(terminate(daemon), Some(0));
    // The next start keeps both tokens.
    let (daemon, _) = start_server(dir.path());
    assert_eq!(tokens(dir.path()), (agents, admin));
    assert_eq!(terminate(daemon), Some(0));
}
