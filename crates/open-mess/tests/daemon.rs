//! `om daemon` as a process: single instance, fixed port, the last start error.
//!
//! Windows only: the daemon reads its token from the real Credential Manager, and Linux CI has no
//! keyring backend.
#![cfg(windows)]
// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use open_mess::store::Store;

fn om(data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_om"));
    command.arg("daemon").env("OPEN_MESS_DATA_DIR", data_dir).stdin(Stdio::null());
    command
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Starts a daemon and waits for its `listening` line.
fn start_daemon(data_dir: &Path) -> Child {
    let mut child = om(data_dir).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (line_tx, line_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });
    let line = line_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    assert!(line.starts_with("open_mess daemon listening on http://127.0.0.1:"), "{line}");
    child
}

#[test]
fn second_daemon_exits_with_already_running() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    Store::open(&dir.path().join("state.sqlite3")).unwrap().set_config("port", &port.to_string()).unwrap();

    let mut first = start_daemon(dir.path());
    let second = om(dir.path()).output().unwrap();
    first.kill().unwrap();
    first.wait().unwrap();

    assert_eq!(second.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&second.stderr).contains("open_mess daemon is already running"));
}

#[test]
fn failed_start_leaves_its_reason_until_the_next_good_start() {
    let dir = tempfile::tempdir().unwrap();
    let error_file = dir.path().join("last-start-error.txt");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    Store::open(&dir.path().join("state.sqlite3")).unwrap().set_config("port", &port.to_string()).unwrap();

    let failed = om(dir.path()).output().unwrap();
    assert_ne!(failed.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&failed.stderr).contains(&format!("port {port} is already in use")));
    let reason = std::fs::read_to_string(&error_file).unwrap();
    assert!(reason.contains(&format!("port {port} is already in use")), "{reason}");
    assert_eq!(reason.trim_end().lines().count(), 1);

    drop(taken);
    let mut daemon = start_daemon(dir.path());
    let removed = !error_file.exists();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(removed);
}
