//! Pending actions (spec 8.5, 8.6): every write runs through them, under the write permission,
//! bound to what the approval showed.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::Arc;

use common::{Daemon, ECHO, daemon, info};
use open_mess::pending::{APPROVAL_TTL, Kind, MAX_KEPT, Pending, RETENTION, State};
use open_mess::permissions::{self, Write};
use open_mess::tools::ToolError;
use serde_json::{Value, json};

/// test-echo with account "a" writing as `write`, and its pending actions.
async fn setup(write: Write) -> (Daemon, Pending) {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    permissions::set(&common::store(d.dir.path()), "test-echo:a", None, true, write).unwrap();
    let pending = Pending::new(Arc::clone(&d.accounts));
    (d, pending)
}

/// test-echo's tool directly, bypassing the core.
async fn echo(d: &Daemon, tool: &str, args: Value) -> Value {
    let handle = d.accounts.modules().handle("test-echo").unwrap();
    serde_json::from_str(&handle.call("a", tool, &args.to_string()).await.unwrap()).unwrap()
}

/// What test-echo was asked to write, in order.
async fn writes(d: &Daemon) -> Vec<Value> {
    echo(d, "writes", json!({})).await["items"].as_array().unwrap().clone()
}

async fn set_target(d: &Daemon, target: Value) {
    echo(d, "set-target", target).await;
}

/// The daemon's state database, as another process would open it.
fn db(d: &Daemon) -> rusqlite::Connection {
    rusqlite::Connection::open(d.dir.path().join("state.sqlite3")).unwrap()
}

fn millis(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap()
}

fn send(text: &str) -> Value {
    json!({"conversation": "c1", "text": text})
}

#[tokio::test]
async fn autonomous_runs_immediately() {
    let (d, pending) = setup(Write::Autonomous).await;
    let outcome = pending.create(Kind::Send, "test-echo:a", "c1", send("hi")).await.unwrap();
    assert_eq!((outcome.state, outcome.detail), (State::Done, None));
    assert_eq!(writes(&d).await, [json!({"tool": "send", "args": send("hi")})]);
    assert!(pending.waiting().is_empty());
    assert_eq!(pending.state(&outcome.action).unwrap(), State::Done);
}

#[tokio::test]
async fn confirm_waits_then_runs_on_approve() {
    let (d, pending) = setup(Write::Confirm).await;
    let mut changed = pending.subscribe();
    let args = json!({"conversation": "c1", "text": "on my way", "reply_to": "m7"});
    let outcome = pending.create(Kind::Send, "test-echo:a", "c1", args.clone()).await.unwrap();
    assert_eq!(outcome.state, State::Waiting);
    assert!(writes(&d).await.is_empty());
    assert!(changed.has_changed().unwrap());
    changed.mark_unchanged();

    // The approval shows the account, the conversation, its participants and the text.
    let waiting = pending.waiting();
    assert_eq!(waiting.len(), 1);
    let preview = &waiting[0];
    assert_eq!((preview.action.as_str(), preview.kind), (outcome.action.as_str(), Kind::Send));
    assert_eq!(preview.account, "User a (test-echo:a)");
    assert_eq!(preview.conversation_title, "Echo c1");
    assert_eq!(preview.participants, ["Pat"]);
    assert_eq!(preview.text.as_deref(), Some("on my way"));
    assert_eq!(preview.target_text.as_deref(), Some("message m7"));

    assert_eq!(pending.decide(&outcome.action, true).await.unwrap(), State::Done);
    assert_eq!(writes(&d).await, [json!({"tool": "send", "args": args})]);
    assert!(pending.waiting().is_empty());
    assert!(changed.has_changed().unwrap());
    // A second click changes nothing.
    assert_eq!(pending.decide(&outcome.action, true).await.unwrap(), State::Done);
    assert_eq!(writes(&d).await.len(), 1);
}

#[tokio::test]
async fn reject_never_runs() {
    let (d, pending) = setup(Write::Confirm).await;
    let outcome = pending.create(Kind::Edit, "test-echo:a", "c1", json!({"conversation": "c1", "message": "m1", "text": "fixed"})).await.unwrap();
    assert_eq!(pending.decide(&outcome.action, false).await.unwrap(), State::Rejected);
    assert_eq!(pending.decide(&outcome.action, true).await.unwrap(), State::Rejected);
    assert!(writes(&d).await.is_empty());
    assert!(pending.waiting().is_empty());
    assert!(pending.decide("nope", true).await.is_err());
}

#[tokio::test]
async fn waiting_expires_after_ttl() {
    let (d, pending) = setup(Write::Confirm).await;
    let late = pending.create(Kind::Send, "test-echo:a", "c1", send("late")).await.unwrap();
    let swept = pending.create(Kind::Send, "test-echo:a", "c1", send("swept")).await.unwrap();
    // Both were created a TTL ago.
    let ttl = i64::try_from(APPROVAL_TTL.as_millis()).unwrap();
    rusqlite::Connection::open(d.dir.path().join("state.sqlite3"))
        .unwrap()
        .execute("UPDATE pending SET created_ms = created_ms - ?1", [ttl])
        .unwrap();

    // An approval that comes too late runs nothing, even before the sweep.
    assert_eq!(pending.decide(&late.action, true).await.unwrap(), State::Expired);
    pending.expire_due().unwrap();
    assert_eq!(pending.state(&swept.action).unwrap(), State::Expired);
    assert!(pending.waiting().is_empty());
    assert!(writes(&d).await.is_empty());
}

#[tokio::test]
async fn changed_participants_void_the_action() {
    let (d, pending) = setup(Write::Confirm).await;
    set_target(&d, json!({"conversation": "c1", "participants": [["p1", "Pat"], ["p2", "Sam"]]})).await;
    let same = pending.create(Kind::Send, "test-echo:a", "c1", send("first")).await.unwrap();
    let joined = pending.create(Kind::Send, "test-echo:a", "c1", send("second")).await.unwrap();
    // The same people in another order still match.
    set_target(&d, json!({"conversation": "c1", "participants": [["p2", "Sam"], ["p1", "Pat"]]})).await;
    assert_eq!(pending.decide(&same.action, true).await.unwrap(), State::Done);
    // Someone joins between the toast and the click.
    set_target(&d, json!({"conversation": "c1", "participants": [["p1", "Pat"], ["p2", "Sam"], ["p3", "Mallory"]]})).await;
    assert_eq!(pending.decide(&joined.action, true).await.unwrap(), State::Voided);
    assert_eq!(writes(&d).await, [json!({"tool": "send", "args": send("first")})]);
}

#[tokio::test]
async fn changed_target_text_voids_a_reaction() {
    let (d, pending) = setup(Write::Confirm).await;
    set_target(&d, json!({"conversation": "c1", "text": "lunch at 12?"})).await;
    let outcome = pending.create(Kind::React, "test-echo:a", "c1", json!({"conversation": "c1", "message": "m1", "reaction": "like"})).await.unwrap();
    assert_eq!(pending.waiting()[0].target_text.as_deref(), Some("lunch at 12?"));
    assert_eq!(pending.waiting()[0].reaction.as_deref(), Some("like"));
    // The author edits the message after the approval was shown.
    set_target(&d, json!({"conversation": "c1", "text": "lunch is cancelled"})).await;
    assert_eq!(pending.decide(&outcome.action, true).await.unwrap(), State::Voided);
    assert!(writes(&d).await.is_empty());
}

#[tokio::test]
async fn timeout_after_send_is_uncertain() {
    let (d, pending) = setup(Write::Autonomous).await;
    // test-echo records the write, then stays busy past the call limit.
    let slow = pending.create(Kind::Send, "test-echo:a", "c1", send("slow")).await.unwrap();
    assert_eq!(slow.state, State::Uncertain);
    assert!(slow.detail.unwrap().contains("check the conversation"));
    // The module says so itself.
    let unsure = pending.create(Kind::Send, "test-echo:a", "c1", send("uncertain")).await.unwrap();
    assert_eq!(unsure.state, State::Uncertain);
    // A refusal is a failure: nothing was sent.
    let failed = pending.create(Kind::Send, "test-echo:a", "c1", send("fail")).await.unwrap();
    assert_eq!(failed.state, State::Failed);
    // Each was asked for once: nothing is tried again.
    let texts: Vec<Value> = writes(&d).await.iter().map(|write| write["args"]["text"].clone()).collect();
    assert_eq!(texts, ["slow", "uncertain", "fail"]);
    assert_eq!(pending.state(&slow.action).unwrap(), State::Uncertain);
}

#[tokio::test]
async fn restart_expires_waiting_and_marks_running_uncertain() {
    let (d, pending) = setup(Write::Confirm).await;
    let waiting = pending.create(Kind::Send, "test-echo:a", "c1", send("left")).await.unwrap();
    let running = pending.create(Kind::Send, "test-echo:a", "c1", send("running")).await.unwrap();
    // The daemon stopped while `running` ran.
    db(&d).execute("UPDATE pending SET state = 'approved' WHERE id = ?1", [&running.action]).unwrap();

    let restarted = Pending::new(Arc::clone(&d.accounts));
    restarted.recover().unwrap();
    assert_eq!(restarted.state(&waiting.action).unwrap(), State::Expired);
    assert_eq!(restarted.state(&running.action).unwrap(), State::Uncertain);
    // A late click runs nothing, and no text is kept.
    assert_eq!(restarted.decide(&waiting.action, true).await.unwrap(), State::Expired);
    let texts: i64 = db(&d).query_row("SELECT COUNT(*) FROM pending WHERE args <> ''", [], |row| row.get(0)).unwrap();
    assert_eq!(texts, 0);
    assert!(writes(&d).await.is_empty());
}

#[tokio::test]
async fn sweep_expires_and_deletes_old_actions() {
    let (d, pending) = setup(Write::Confirm).await;
    let pending = Arc::new(pending);
    let waiting = pending.create(Kind::Send, "test-echo:a", "c1", send("waiting")).await.unwrap();
    let old = pending.create(Kind::Send, "test-echo:a", "c1", send("old")).await.unwrap();
    pending.decide(&old.action, false).await.unwrap();
    let kept = pending.create(Kind::Send, "test-echo:a", "c1", send("kept")).await.unwrap();
    pending.decide(&kept.action, false).await.unwrap();
    db(&d).execute("UPDATE pending SET created_ms = created_ms - ?2 WHERE id = ?1", (&waiting.action, millis(APPROVAL_TTL))).unwrap();
    db(&d).execute("UPDATE pending SET created_ms = created_ms - ?2 WHERE id = ?1", (&old.action, millis(RETENTION) + 1)).unwrap();

    // Both sweeps run when the sweeper starts.
    tokio::spawn(Arc::clone(&pending).sweep());
    for _ in 0..100 {
        if pending.state(&old.action).is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(pending.state(&old.action).is_err());
    assert_eq!(pending.state(&waiting.action).unwrap(), State::Expired);
    assert_eq!(pending.state(&kept.action).unwrap(), State::Rejected);
    assert!(pending.waiting().is_empty());
}

#[tokio::test]
async fn an_account_keeps_a_bounded_number_of_actions() {
    let (d, pending) = setup(Write::Autonomous).await;
    // Finished actions of an agent writing in a loop, oldest first.
    let mut db = db(&d);
    let tx = db.transaction().unwrap();
    for n in 1..=MAX_KEPT + 5 {
        // The oldest one may have reached the platform: it is the last to go.
        let state = if n == 1 { "uncertain" } else { "done" };
        tx.execute(
            "INSERT INTO pending (id, account, conversation, kind, args, fingerprint, state, created_ms) VALUES (?1, 'test-echo:a', 'c1', 'send', '', '', ?2, ?3)",
            (format!("old{n}"), state, n),
        )
        .unwrap();
    }
    tx.commit().unwrap();
    assert_eq!(pending.create(Kind::Send, "test-echo:a", "c1", send("one more")).await.unwrap().state, State::Done);
    let count = |sql: &str| db.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
    // The oldest done ones went; the uncertain one and the new one are kept.
    assert_eq!(count("SELECT COUNT(*) FROM pending"), MAX_KEPT + 1);
    assert_eq!(count("SELECT COUNT(*) FROM pending WHERE created_ms BETWEEN 2 AND 6"), 0);
    assert_eq!(pending.state("old1").unwrap(), State::Uncertain);
}

#[tokio::test]
async fn never_is_refused() {
    let (d, pending) = setup(Write::Never).await;
    let result = pending.create(Kind::Send, "test-echo:a", "c1", send("hi")).await;
    assert!(matches!(&result, Err(ToolError::NotAllowed(why)) if why == "writing to this conversation is not allowed"), "{result:?}");
    // A conversation the user allowed is the exception.
    permissions::set(&common::store(d.dir.path()), "test-echo:a", Some("c2"), true, Write::Autonomous).unwrap();
    let outcome = pending.create(Kind::Send, "test-echo:a", "c2", json!({"conversation": "c2", "text": "hi"})).await.unwrap();
    assert_eq!(outcome.state, State::Done);
    assert_eq!(writes(&d).await.len(), 1);
    // An unknown or removed account is not written to.
    assert!(matches!(pending.create(Kind::Send, "test-echo:b", "c1", send("hi")).await, Err(ToolError::InvalidArgs(_))));
}

#[tokio::test]
async fn count_by_state() {
    let (d, pending) = setup(Write::Confirm).await;
    let first = pending.create(Kind::Send, "test-echo:a", "c1", send("one")).await.unwrap();
    pending.create(Kind::Send, "test-echo:a", "c1", send("two")).await.unwrap();
    let rejected = pending.create(Kind::Send, "test-echo:a", "c1", send("three")).await.unwrap();
    pending.decide(&rejected.action, false).await.unwrap();
    // Over all accounts.
    d.accounts.add("test-echo", info("b")).unwrap();
    permissions::set(&common::store(d.dir.path()), "test-echo:b", None, true, Write::Confirm).unwrap();
    pending.create(Kind::Send, "test-echo:b", "c1", send("four")).await.unwrap();
    // One running.
    db(&d).execute("UPDATE pending SET state = 'approved' WHERE id = ?1", [&first.action]).unwrap();
    let counts: Vec<usize> =
        [State::Waiting, State::Approved, State::Rejected, State::Done].into_iter().map(|state| pending.count(state).unwrap()).collect();
    assert_eq!(counts, [2, 1, 1, 0]);
}
