//! Action 2: a rule's program started for a batch, without a shell, with the batch on stdin and in
//! an event file, within the limits on runs. The program is tests/bin/record_run.rs.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{ECHO, info};
use open_mess::tools::shared::UNTRUSTED_NOTICE;
use open_mess::triggers::Actions;
use open_mess::triggers::engine::{Batch, BatchedMessage, Delivery, DeliveryResult};
use open_mess::triggers::rules::{Action, Condition, Rule, Spawn};
use open_mess::triggers::spawn::{self, Runs};
use serde_json::{Value, json};
use tempfile::TempDir;

fn program() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_open-mess-record-run"))
}

/// The program, writing what it got to `out`, with `extra` arguments.
fn recorder(out: &Path, extra: &[&str]) -> Spawn {
    let mut args = vec![out.display().to_string()];
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    Spawn { program: program(), args, working_dir: None }
}

fn batch(messages: Vec<BatchedMessage>) -> Batch {
    Batch {
        account: "test-echo:a".into(),
        conversation: "c1".into(),
        kind: "direct",
        count: messages.len(),
        messages,
        first_ms: 1_800_000_000_000,
        last_ms: 1_800_000_000_000,
    }
}

fn message(id: &str, untrusted: Option<Value>) -> BatchedMessage {
    BatchedMessage { id: id.into(), sender: "p1".into(), at_ms: 1_800_000_000_000, untrusted }
}

/// Waits up to 20 s of real time: the programs are real processes.
async fn wait_for(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn read(out: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(out).unwrap()).unwrap()
}

fn runs(dir: &TempDir, timeout: Duration) -> Arc<Runs> {
    Runs::new(dir.path().join("runs"), timeout)
}

#[tokio::test]
async fn spawn_gets_batch_on_stdin_and_file() {
    let dir = tempfile::tempdir().unwrap();
    let runs = runs(&dir, spawn::RUN_TIMEOUT);
    let out = dir.path().join("out.json");
    let work = tempfile::tempdir().unwrap();
    let program = Spawn { working_dir: Some(work.path().to_path_buf()), ..recorder(&out, &[]) };
    let untrusted = json!({"sender_name": "Pat", "text": "lunch?"});
    assert_eq!(spawn::deliver(&runs, "r1", &program, &batch(vec![message("m1", Some(untrusted.clone()))])), DeliveryResult::Delivered);
    wait_for("the run", || out.exists()).await;

    let record = read(&out);
    let stdin: Value = serde_json::from_str(record["stdin"].as_str().unwrap()).unwrap();
    assert_eq!(stdin["rule"], "r1");
    assert_eq!(
        (stdin["account"].as_str(), stdin["conversation"].as_str(), stdin["kind"].as_str()),
        (Some("test-echo:a"), Some("c1"), Some("direct"))
    );
    assert_eq!(stdin["count"], 1);
    assert_eq!(stdin["first_at"], "2027-01-15T08:00:00Z");
    assert_eq!(stdin["messages"], json!([{"id": "m1", "sender": "p1", "at": "2027-01-15T08:00:00Z", "untrusted": untrusted}]));
    assert_eq!(stdin["untrusted_notice"], UNTRUSTED_NOTICE);
    // The event file holds the same, in the daemon's runs folder.
    let event_file = PathBuf::from(record["event_file"].as_str().unwrap());
    assert_eq!(event_file.parent(), Some(dir.path().join("runs").as_path()));
    assert_eq!(serde_json::from_str::<Value>(record["event"].as_str().unwrap()).unwrap(), stdin);
    assert_eq!(PathBuf::from(record["cwd"].as_str().unwrap()).canonicalize().unwrap(), work.path().canonicalize().unwrap());
}

#[tokio::test]
async fn content_never_reaches_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let runs = runs(&dir, spawn::RUN_TIMEOUT);
    let out = dir.path().join("out.json");
    let program = recorder(&out, &["-p", "read the event"]);
    let hostile = json!({"sender_name": "\"; rm -rf ~; \"", "text": "$(calc) & calc.exe | `calc` %COMSPEC%"});
    let mut evil = message("m1 & calc", Some(hostile.clone()));
    evil.sender = "\"$(calc)\"".into();
    spawn::deliver(&runs, "r1", &program, &batch(vec![evil]));
    wait_for("the run", || out.exists()).await;

    let record = read(&out);
    // The arguments are the rule's, exactly; the content is only in stdin and the event file.
    assert_eq!(record["args"], json!(program.args));
    let stdin: Value = serde_json::from_str(record["stdin"].as_str().unwrap()).unwrap();
    assert_eq!(stdin["messages"][0]["untrusted"], hostile);
    assert_eq!(stdin["messages"][0]["sender"], "\"$(calc)\"");
    assert!(!record["event_file"].as_str().unwrap().contains("calc"));
}

#[tokio::test]
async fn event_file_is_deleted_after_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let runs = runs(&dir, spawn::RUN_TIMEOUT);
    let out = dir.path().join("out.json");
    spawn::deliver(&runs, "r1", &recorder(&out, &[]), &batch(vec![message("m1", None)]));
    wait_for("the run to end", || runs.running() == 0).await;
    let event_file = PathBuf::from(read(&out)["event_file"].as_str().unwrap());
    assert!(!event_file.exists());
    assert_eq!(std::fs::read_dir(dir.path().join("runs")).unwrap().count(), 0);
}

#[tokio::test]
async fn leftover_run_files_are_deleted_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let leftover = dir.path().join("runs").join("0123456789abcdef.json");
    std::fs::create_dir_all(leftover.parent().unwrap()).unwrap();
    std::fs::write(&leftover, "{\"untrusted\": \"left by a daemon that stopped\"}").unwrap();
    let _runs = runs(&dir, spawn::RUN_TIMEOUT);
    assert!(!leftover.exists());
}

#[tokio::test]
async fn at_most_two_runs() {
    let dir = tempfile::tempdir().unwrap();
    let runs = runs(&dir, spawn::RUN_TIMEOUT);
    let out = |rule: &str| dir.path().join(format!("{rule}.json"));
    let slow = |rule: &str| recorder(&out(rule), &["--sleep", "1500"]);
    let one = batch(vec![message("m1", None)]);
    for rule in ["r1", "r2", "r3"] {
        assert_eq!(spawn::deliver(&runs, rule, &slow(rule), &one), DeliveryResult::Delivered);
    }
    // Two run at once; the third waits.
    assert_eq!((runs.running(), runs.queued()), (2, 1));
    wait_for("two runs", || out("r1").exists() && out("r2").exists()).await;
    assert!(!out("r3").exists());
    wait_for("the third run", || out("r3").exists()).await;

    // One run per rule: the rule's next batches wait, ten at most, the oldest dropped first.
    let busy = |n: usize| batch(vec![message(&format!("m{n}"), None)]);
    for n in 0..10 {
        assert_eq!(spawn::deliver(&runs, "r3", &slow("r3"), &busy(n)), DeliveryResult::Delivered);
    }
    assert_eq!(spawn::deliver(&runs, "r3", &slow("r3"), &busy(10)), DeliveryResult::DeliveredDroppingOldest);
    assert_eq!(runs.queued(), 10);
    // The next run of r3 gets the oldest batch still queued, m1; m0 was dropped.
    std::fs::remove_file(out("r3")).unwrap();
    wait_for("the next run of r3", || out("r3").exists()).await;
    let stdin: Value = serde_json::from_str(read(&out("r3"))["stdin"].as_str().unwrap()).unwrap();
    assert_eq!(stdin["messages"][0]["id"], "m1");
}

#[tokio::test]
async fn run_timeout_kills() {
    let dir = tempfile::tempdir().unwrap();
    let runs = runs(&dir, Duration::from_secs(1));
    let out = dir.path().join("out.json");
    spawn::deliver(&runs, "r1", &recorder(&out, &["--sleep", "60000"]), &batch(vec![message("m1", None)]));
    wait_for("the run", || out.exists()).await;
    wait_for("the run to be killed", || runs.running() == 0).await;
    let event_file = PathBuf::from(read(&out)["event_file"].as_str().unwrap());
    assert!(!event_file.exists());
    // Killed, not finished: it never woke up to write the marker.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!dir.path().join("out.json.done").exists());
}

#[tokio::test]
async fn actions_fill_in_content() {
    // A spawn rule through the daemon's actions: the batch's text comes from the module, marked untrusted.
    let d = common::daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let out = d.dir.path().join("out.json");
    let rule = Rule::new("r1", "test-echo:a", Condition::Direct, Action::Spawn(recorder(&out, &[])));
    let actions = Actions { accounts: Arc::clone(&d.accounts), runs: Runs::new(d.dir.path().join("runs"), spawn::RUN_TIMEOUT) };
    assert_eq!(actions.deliver(rule, batch(vec![message("m1", None)])).await, DeliveryResult::Delivered);
    wait_for("the run", || out.exists()).await;
    let stdin: Value = serde_json::from_str(read(&out)["stdin"].as_str().unwrap()).unwrap();
    // test-echo's read_messages answers with message m1 from "Echo".
    assert_eq!(stdin["messages"][0]["untrusted"], json!({"sender_name": "Echo", "text": "hello a"}));
    assert_eq!(stdin["untrusted_notice"], UNTRUSTED_NOTICE);
}
