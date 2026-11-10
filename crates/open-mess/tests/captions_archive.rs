//! The archive of Teams calls from their live captions (spec 2026-11-04-captions-archive), with
//! test-echo standing in for the Teams module and the archiver's time given by each test.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, SecondsFormat};
use common::{Daemon, ECHO, daemon, info};
use open_mess::permissions::{self, Write};
use open_mess::recording::archive::{self, CallState, Cue, GapReason, Gaps};
use open_mess::recording::captions::{self, Archiver, Notices, Setup};
use open_mess::recording::{Meta, Reason, Recordings, Source, State};
use open_mess::tools::{GateMode, ToolError};
use serde_json::{Value, json};

const THREAD: &str = "19:meeting_abc@thread.v2";
const MINUTE: u64 = 60_000;

#[derive(Default)]
struct Told(Mutex<Vec<String>>);

impl Notices for Told {
    fn started(&self, id: &str) {
        self.0.lock().unwrap().push(id.to_owned());
    }
}

/// A daemon with test-echo's account "a", the tray up, and the archiver turned on at `base`.
struct Archive {
    d: Daemon,
    archiver: Arc<Archiver>,
    told: Arc<Told>,
    free: Arc<AtomicUsize>,
    base: u64,
}

impl Archive {
    async fn new() -> Archive {
        let d = daemon(&[ECHO]).await;
        d.accounts.add("test-echo", info("a")).unwrap();
        d.registry.ui_ready().store(true, Ordering::SeqCst);
        let base = captions::now_ms();
        let (archiver, told, free) = archiver(&d);
        archiver.set(true, "pl-pl", base).await.unwrap();
        Archive { d, archiver, told, free, base }
    }

    /// The archiver of the same folder after a restart of the daemon.
    fn restart(&mut self) {
        (self.archiver, self.told, self.free) = archiver(&self.d);
    }

    fn recordings(&self) -> Recordings {
        Recordings::new(&self.d.dir.path().join("local"))
    }

    /// A pass `ms` after the base.
    async fn at(&self, ms: u64) {
        self.archiver.pass(self.base + ms).await;
    }

    async fn echo(&self, tool: &str, args: Value) -> Value {
        let handle = self.d.accounts.modules().handle("test-echo").unwrap();
        serde_json::from_str(&handle.call("a", tool, &args.to_string()).await.unwrap()).unwrap()
    }

    async fn calls(&self, calls: &[(&str, &str)]) {
        let items: Vec<Value> = calls.iter().map(|(call, conversation)| json!({"call": call, "conversation": conversation})).collect();
        self.echo("set-calls", json!({ "items": items })).await;
    }

    /// Cues said in the conversation's session, at `ms` after the base.
    async fn say(&self, conversation: &str, cues: &[(u64, &str, &str)]) {
        let cues: Vec<Value> =
            cues.iter().map(|(ms, speaker, text)| json!({"at": rfc3339(self.base + ms), "speaker": speaker, "own": false, "text": text})).collect();
        self.echo("captions-say", json!({"conversation": conversation, "cues": cues})).await;
    }

    async fn joins(&self) -> Vec<Value> {
        self.echo("captions-log", json!({})).await["joins"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|join| join["args"].clone())
            .collect()
    }

    async fn leaves(&self) -> usize {
        self.echo("captions-log", json!({})).await["leaves"].as_array().map_or(0, Vec::len)
    }

    /// The captions recordings, newest first.
    fn kept(&self) -> Vec<(String, Meta)> {
        self.recordings().list().into_iter().filter(|entry| entry.meta.source == Source::Captions).map(|entry| (entry.id, entry.meta)).collect()
    }

    fn cues(&self, id: &str) -> Vec<String> {
        let mut texts = Vec::new();
        archive::read_lines(&self.recordings().path(id).unwrap().join(archive::CAPTIONS), false, |cue: Option<Cue>| {
            texts.push(cue.map_or_else(|| "?".into(), |cue| cue.text));
            true
        })
        .unwrap();
        texts
    }

    fn gaps(&self, id: &str) -> Vec<GapReason> {
        Gaps::load(&self.recordings().path(id).unwrap(), false).unwrap().0.0.iter().map(|gap| gap.reason).collect()
    }

    fn call_state(&self, call: &str) -> Option<CallState> {
        archive::load_calls(&self.d.dir.path().join("local").join("recordings"))
            .0
            .into_iter()
            .find(|entry| entry.call == call)
            .map(|entry| entry.state)
    }
}

fn archiver(d: &Daemon) -> (Arc<Archiver>, Arc<Told>, Arc<AtomicUsize>) {
    let told = Arc::new(Told::default());
    let free = Arc::new(AtomicUsize::new(usize::MAX));
    let disk = Arc::clone(&free);
    let setup = Setup {
        accounts: Arc::clone(&d.accounts),
        gate: d.registry.gate().clone(),
        probation: d.registry.pending().unwrap().probation().clone(),
        ui_ready: Arc::clone(d.registry.ui_ready()),
        notices: Arc::clone(&told) as Arc<dyn Notices>,
        free_disk: Box::new(move || disk.load(Ordering::SeqCst) as u64),
        threads: Arc::clone(d.registry.threads()),
    };
    (Archiver::new(Recordings::new(&d.dir.path().join("local")), setup), told, free)
}

fn rfc3339(ms: u64) -> String {
    DateTime::from_timestamp_millis(i64::try_from(ms).unwrap()).unwrap().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[tokio::test]
async fn with_the_switch_off_nothing_is_joined() {
    let a = Archive::new().await;
    a.archiver.set(false, "pl-pl", a.base).await.unwrap();
    a.calls(&[("c1", THREAD)]).await;
    for step in 0..5 {
        a.at(step * 5000).await;
    }
    assert!(a.joins().await.is_empty() && a.kept().is_empty());
    // A fresh daemon has it off as well.
    let fresh = daemon(&[ECHO]).await;
    let (archiver, ..) = archiver(&fresh);
    assert!(!archiver.auto());
}

#[tokio::test]
async fn a_call_is_kept_word_for_word_from_join_to_end() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    let joins = a.joins().await;
    assert_eq!(joins, [json!({"conversation": THREAD, "call": "c1", "language": "pl-pl", "start_captions": true})]);
    // The recording starts at the first live state, with the time before it a gap.
    a.at(5000).await;
    let [(id, meta)] = &a.kept()[..] else { panic!("{:?}", a.kept()) };
    assert_eq!((meta.state, meta.start_ms, meta.conversation.as_deref(), meta.call.as_deref()), (State::Recording, a.base, Some(THREAD), Some("c1")));
    assert_eq!(*a.told.0.lock().unwrap(), std::slice::from_ref(id));
    assert_eq!(a.archiver.active(), 1);
    let odd = "line one\nline \"two\" </p><script>x</script> zażółć";
    a.say(THREAD, &[(6000, "Ann", odd), (7000, "Bob", "second")]).await;
    a.at(10_000).await;
    assert_eq!(a.cues(id), [odd, "second"]);
    // An agent reads it as it goes on, word for word.
    let page = a.d.registry.call("get_recording_transcript", json!({"recording": id})).await.unwrap();
    assert_eq!(page["untrusted"]["cues"][0], json!({"start": "00:00:06.000", "end": null, "speaker": "Ann", "own": false, "text": odd}));
    assert_eq!(page["gaps"], json!([{"from": "00:00:00.000", "to": "00:00:05.000", "reason": "late_start"}]));
    let listed = a.d.registry.call("list_recordings", json!({})).await.unwrap();
    assert_eq!((&listed["items"][0]["source"], &listed["items"][0]["state"]), (&json!("captions"), &json!("recording")));
    // The user leaves: the recording is done, open_mess leaves nothing behind.
    a.echo("captions-say", json!({"conversation": THREAD, "state": "ended", "reason": "user_left"})).await;
    a.at(15_000).await;
    let meta = &a.recordings().meta(id).unwrap();
    assert_eq!((meta.state, meta.reason, meta.end_ms), (State::Done, None, Some(a.base + 15_000)));
    assert_eq!(meta.coverage, Some(66));
    assert_eq!(a.archiver.active(), 0);
}

#[tokio::test]
async fn zatrzymaj_keeps_the_last_cue_and_blocks_the_call_across_a_restart() {
    let mut a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    // A cue in the module's buffer between two reads, then "Zatrzymaj".
    a.say(THREAD, &[(6000, "Ann", "just said")]).await;
    let stopped = a.archiver.stop(|_, _| true).await;
    let [(id, meta)] = &a.kept()[..] else { panic!() };
    assert_eq!(stopped, std::slice::from_ref(id));
    assert_eq!((meta.state, meta.reason), (State::Done, Some(Reason::Stopped)));
    assert_eq!(a.cues(id), ["just said"]);
    assert_eq!(a.leaves().await, 1);
    assert_eq!(a.call_state("c1"), Some(CallState::Blocked));
    for step in 2..10 {
        a.at(step * 5000).await;
    }
    assert_eq!(a.joins().await.len(), 1);
    // After a restart, an hour later: still blocked.
    a.restart();
    a.at(60 * MINUTE).await;
    a.at(61 * MINUTE).await;
    assert_eq!((a.joins().await.len(), a.kept().len()), (1, 1));
}

#[tokio::test]
async fn turning_the_switch_off_and_on_keeps_the_call_in_a_new_recording() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    a.say(THREAD, &[(6000, "Ann", "before")]).await;
    a.archiver.set(false, "pl-pl", a.base + 7000).await.unwrap();
    let [(first, meta)] = &a.kept()[..] else { panic!() };
    assert_eq!((meta.state, meta.reason), (State::Done, Some(Reason::AutoOff)));
    assert_eq!((a.cues(first), a.leaves().await), (vec!["before".to_string()], 1));
    assert_eq!(a.call_state("c1"), Some(CallState::Active));
    // On again: the call goes on in a new recording from then, nothing of before it in there.
    a.archiver.set(true, "pl-pl", a.base + 20_000).await.unwrap();
    a.say(THREAD, &[(21_000, "Ann", "while off")]).await;
    a.at(25_000).await;
    a.at(30_000).await;
    let kept = a.kept();
    let (second, meta) = &kept[0];
    assert_ne!(second, first);
    assert_eq!((meta.state, meta.start_ms), (State::Recording, a.base + 25_000));
    a.say(THREAD, &[(31_000, "Ann", "after")]).await;
    a.at(35_000).await;
    assert_eq!(a.cues(second), ["after"]);
}

#[tokio::test]
async fn a_permanent_refusal_is_a_failed_recording_and_never_tried_again() {
    let mut a = Archive::new().await;
    a.echo("set-join", json!({"error": "e2ee: the meeting is end-to-end encrypted"})).await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    let [(_, meta)] = &a.kept()[..] else { panic!() };
    assert_eq!((meta.state, meta.reason), (State::Failed, Some(Reason::E2ee)));
    for step in 1..60 {
        a.at(step * 5000).await;
    }
    a.restart();
    a.at(10 * MINUTE).await;
    assert_eq!((a.joins().await.len(), a.kept().len()), (1, 1));
}

#[tokio::test]
async fn a_passing_failure_is_tried_again_in_two_minutes_and_refused_three_times() {
    let a = Archive::new().await;
    a.echo("set-join", json!({"error": "media_failed"})).await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(MINUTE).await;
    assert_eq!(a.joins().await.len(), 1);
    assert!(a.kept().is_empty());
    a.echo("set-join", json!({})).await;
    a.at(2 * MINUTE).await;
    a.at(2 * MINUTE + 5000).await;
    let [(_, meta)] = &a.kept()[..] else { panic!() };
    // Kept from the successful join on, the time before it a gap.
    assert_eq!((meta.state, meta.start_ms), (State::Recording, a.base));
    assert_eq!(a.gaps(&a.kept()[0].0), [GapReason::LateStart]);

    // `refused`, with no reason recognised: three tries.
    a.calls(&[("c1", THREAD), ("c2", "19:other@thread.v2")]).await;
    a.echo("set-join", json!({"error": "refused"})).await;
    for step in 0..20 {
        a.at(3 * MINUTE + step * MINUTE).await;
    }
    let tries = a.joins().await.iter().filter(|join| join["call"] == "c2").count();
    assert_eq!(tries, 3);
    let failed = a.kept().into_iter().find(|(_, meta)| meta.call.as_deref() == Some("c2")).unwrap().1;
    assert_eq!((failed.state, failed.reason), (State::Failed, Some(Reason::Refused)));
}

#[tokio::test]
async fn without_the_user_in_the_call_nothing_is_kept_until_they_come_back() {
    let a = Archive::new().await;
    a.echo("set-join", json!({"error": "user_not_in_meeting"})).await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(MINUTE).await;
    assert_eq!(a.joins().await.len(), 1);
    assert!(a.kept().is_empty() && a.told.0.lock().unwrap().is_empty());
    // A join that ends that way before it is live leaves nothing either.
    a.echo("set-join", json!({"state": "joining"})).await;
    a.at(2 * MINUTE).await;
    a.echo("captions-say", json!({"conversation": THREAD, "state": "failed", "reason": "user_not_in_meeting"})).await;
    a.at(2 * MINUTE + 5000).await;
    assert!(a.kept().is_empty() && a.told.0.lock().unwrap().is_empty());
    // Back in the call: kept from the next try.
    a.echo("set-join", json!({})).await;
    a.at(4 * MINUTE + 5000).await;
    a.at(4 * MINUTE + 10_000).await;
    assert_eq!(a.kept().len(), 1);
    // A call that ended between the list and the join: an error, nothing kept.
    a.calls(&[("c1", THREAD), ("c2", "19:two@thread.v2")]).await;
    a.echo("set-join", json!({"error": "that call is no longer running"})).await;
    a.at(5 * MINUTE).await;
    a.at(5 * MINUTE + 5000).await;
    assert_eq!(a.kept().len(), 1);
}

#[tokio::test]
async fn lost_cues_and_captions_turned_off_are_gaps() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    a.say(THREAD, &[(6000, "Ann", "one")]).await;
    a.at(10_000).await;
    // An agent read the rest faster, and the buffer dropped two cues the archiver never read.
    a.say(THREAD, &[(11_000, "Ann", "two"), (12_000, "Ann", "three"), (13_000, "Ann", "four")]).await;
    a.echo("captions-say", json!({"conversation": THREAD, "drop": 3})).await;
    a.at(15_000).await;
    // Someone turned the captions off for a while.
    a.echo("captions-say", json!({"conversation": THREAD, "captions_on": false})).await;
    a.at(20_000).await;
    a.at(25_000).await;
    a.echo("captions-say", json!({"conversation": THREAD, "captions_on": true})).await;
    a.at(30_000).await;
    let id = &a.kept()[0].0;
    assert_eq!(a.cues(id), ["one", "four"]);
    let gaps = Gaps::load(&a.recordings().path(id).unwrap(), false).unwrap().0.0;
    assert_eq!(gaps.iter().map(|gap| gap.reason).collect::<Vec<_>>(), [GapReason::LateStart, GapReason::Lost, GapReason::CaptionsOff]);
    assert_eq!((gaps[1].from, gaps[1].to), (10_000, Some(15_000)));
    assert_eq!((gaps[2].from, gaps[2].to), (15_000, Some(25_000)));
}

#[tokio::test]
async fn a_session_ended_from_outside_is_joined_again_into_the_same_recording() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    // A cue in the buffer, then the modules' sessions end (an update): it is read first.
    a.say(THREAD, &[(6000, "Ann", "before the update")]).await;
    let handles = a.d.accounts.modules().handles();
    a.d.accounts.end_sessions(&handles).await;
    a.at(10_000).await;
    a.at(15_000).await;
    let kept = a.kept();
    let [(id, meta)] = &kept[..] else { panic!("{kept:?}") };
    assert_eq!((meta.state, a.joins().await.len()), (State::Recording, 2));
    assert_eq!(a.cues(id), ["before the update"]);
    assert_eq!(a.gaps(id), [GapReason::LateStart, GapReason::Rejoin]);
    // An agent's leave and an idle end: joined again the same way.
    a.echo("leave_captions", json!({"conversation": THREAD})).await;
    a.at(20_000).await;
    a.at(25_000).await;
    a.echo("captions-say", json!({"conversation": THREAD, "state": "ended", "reason": "idle"})).await;
    a.at(30_000).await;
    a.at(35_000).await;
    assert_eq!((a.kept().len(), a.joins().await.len()), (1, 4));
    // The call is over: done.
    a.calls(&[]).await;
    a.echo("captions-say", json!({"conversation": THREAD, "state": "ended", "reason": "meeting_ended"})).await;
    a.at(60_000).await;
    assert_eq!(a.kept()[0].1.state, State::Done);
}

#[tokio::test]
async fn an_agent_s_session_is_shared_and_nothing_before_the_switch_is_kept() {
    let a = Archive::new().await;
    a.archiver.set(false, "pl-pl", a.base).await.unwrap();
    // An agent joined with its approval, without turning captions on, and read some.
    a.echo("join_captions", json!({"conversation": THREAD, "call": "c1", "start_captions": false})).await;
    a.say(THREAD, &[(1000, "Ann", "said before the switch")]).await;
    a.calls(&[("c1", THREAD)]).await;
    a.archiver.set(true, "pl-pl", a.base + 2000).await.unwrap();
    a.at(3000).await;
    a.at(5000).await;
    a.say(THREAD, &[(6000, "Ann", "said after")]).await;
    a.at(10_000).await;
    // No second join: the module answered done and the archiver read the same session.
    let joins = a.joins().await;
    assert_eq!(joins.len(), 2);
    assert_eq!(joins[1]["start_captions"], true);
    let id = &a.kept()[0].0;
    assert_eq!(a.cues(id), ["said after"]);
}

#[tokio::test]
async fn captions_seen_once_are_never_turned_on_again() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.echo("captions-say", json!({"conversation": THREAD, "captions_seen": true})).await;
    a.at(5000).await;
    // The session ends at its time limit; the next one must not turn captions on.
    a.echo("captions-say", json!({"conversation": THREAD, "state": "ended", "reason": "time_limit"})).await;
    a.at(10_000).await;
    a.at(15_000).await;
    let joins = a.joins().await;
    assert_eq!((joins.len(), &joins[1]["start_captions"]), (2, &json!(false)));
}

#[tokio::test]
async fn a_restart_goes_on_in_the_same_recording_or_ends_it_interrupted() {
    let mut a = Archive::new().await;
    a.calls(&[("c1", THREAD), ("c2", "19:two@thread.v2")]).await;
    a.at(0).await;
    a.at(5000).await;
    assert_eq!(a.kept().len(), 2);
    // The daemon stops; c2 ends meanwhile.
    a.restart();
    a.calls(&[("c1", THREAD)]).await;
    a.at(MINUTE).await;
    a.at(MINUTE + 5000).await;
    let kept = a.kept();
    let c1 = kept.iter().find(|(_, meta)| meta.call.as_deref() == Some("c1")).unwrap();
    let c2 = kept.iter().find(|(_, meta)| meta.call.as_deref() == Some("c2")).unwrap();
    assert_eq!(kept.len(), 2);
    assert_eq!(c1.1.state, State::Recording);
    assert_eq!(a.gaps(&c1.0), [GapReason::LateStart, GapReason::Rejoin]);
    assert_eq!((c2.1.state, c2.1.reason), (State::Done, Some(Reason::Interrupted)));
}

#[tokio::test]
async fn a_lost_state_joins_no_call_listed_at_the_start() {
    let mut a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    a.archiver.stop(|_, _| true).await;
    // Both the state and its previous version are damaged at the next start.
    let dir = a.d.dir.path().join("local").join("recordings");
    std::fs::write(dir.join(archive::CALLS), b"{damaged").unwrap();
    std::fs::write(dir.join(format!("{}.bak", archive::CALLS)), b"{damaged").unwrap();
    a.restart();
    a.calls(&[("c1", THREAD), ("c0", "19:zero@thread.v2")]).await;
    a.at(MINUTE).await;
    a.at(MINUTE + 5000).await;
    assert_eq!(a.joins().await.len(), 1);
    assert_eq!(a.archiver.status()["state_lost"], true);
    // A call that starts later is kept.
    a.calls(&[("c1", THREAD), ("c0", "19:zero@thread.v2"), ("c3", "19:three@thread.v2")]).await;
    a.at(2 * MINUTE).await;
    let joins = a.joins().await;
    assert_eq!((joins.len(), &joins[1]["call"]), (2, &json!("c3")));
}

#[tokio::test]
async fn no_join_without_the_tray_while_writes_are_held_or_with_a_full_disk() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.d.registry.ui_ready().store(false, Ordering::SeqCst);
    a.at(0).await;
    assert_eq!(a.archiver.status()["reason"], "no tray icon");
    a.d.registry.ui_ready().store(true, Ordering::SeqCst);
    a.d.registry.gate().set(GateMode::WritesBlocked);
    a.at(5000).await;
    assert_eq!(a.archiver.status()["reason"], "update in progress");
    a.d.registry.gate().set(GateMode::Open);
    // While an update's new version is checked, too.
    a.d.registry.pending().unwrap().probation().set(true);
    a.at(7000).await;
    assert!(a.joins().await.is_empty());
    a.d.registry.pending().unwrap().probation().set(false);
    a.free.store(1024, Ordering::SeqCst);
    a.at(10_000).await;
    assert_eq!(a.archiver.status()["reason"], "low disk");
    assert!(a.joins().await.is_empty());
    // A full disk ends the recording going on, and leaves the call.
    a.free.store(usize::MAX, Ordering::SeqCst);
    a.at(15_000).await;
    a.at(20_000).await;
    a.free.store(1024, Ordering::SeqCst);
    a.at(25_000).await;
    let meta = &a.kept()[0].1;
    assert_eq!((meta.state, meta.reason), (State::Done, Some(Reason::LowDisk)));
    assert_eq!(a.leaves().await, 1);
}

#[tokio::test]
async fn the_next_call_in_a_thread_has_its_own_recording_and_the_last_cue_stays_with_the_first() {
    let a = Archive::new().await;
    a.calls(&[("call-a", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    a.say(THREAD, &[(6000, "Ann", "end of a")]).await;
    // Call A ended and B started in the same chat before the archiver read A's last cue.
    a.calls(&[("call-b", THREAD)]).await;
    a.at(20_000).await;
    a.say(THREAD, &[(21_000, "Ann", "start of b")]).await;
    a.at(25_000).await;
    a.at(30_000).await;
    let kept = a.kept();
    let first = kept.iter().find(|(_, meta)| meta.call.as_deref() == Some("call-a")).unwrap();
    let second = kept.iter().find(|(_, meta)| meta.call.as_deref() == Some("call-b")).unwrap();
    assert_eq!((first.1.state, a.cues(&first.0)), (State::Done, vec!["end of a".to_string()]));
    assert_eq!((second.1.state, a.cues(&second.0)), (State::Recording, vec!["start of b".to_string()]));
    // A list item of another account's agent-unreadable chat is left out of the agent's list.
    permissions::set(&common::store(a.d.dir.path()), "test-echo:a", Some(THREAD), false, Write::Never).unwrap();
    let listed = a.d.registry.call("test_echo_list_calls", json!({"account": "test-echo:a"})).await;
    assert!(matches!(listed, Err(ToolError::NotAllowed(_))) || listed.unwrap()["items"] == json!([]));
}

#[tokio::test]
async fn turning_the_switch_off_during_a_join_leaves_the_call() {
    let a = Archive::new().await;
    a.echo("set-join", json!({"slow": true})).await;
    a.calls(&[("c1", THREAD)]).await;
    // Listed first, so the next pass goes straight to the join, which takes a second.
    a.d.registry.ui_ready().store(false, Ordering::SeqCst);
    a.at(0).await;
    a.d.registry.ui_ready().store(true, Ordering::SeqCst);
    let off = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        a.archiver.set(false, "pl-pl", a.base + 6000).await.unwrap();
    };
    tokio::join!(a.at(5000), off);
    // The join ran to its end, and then open_mess left the call.
    assert_eq!((a.joins().await.len(), a.leaves().await), (1, 1));
    a.at(10_000).await;
    assert!(a.kept().is_empty());
}

#[tokio::test]
async fn a_scan_during_a_join_ends_its_session_after_it_and_keeps_the_last_cue() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    a.say(THREAD, &[(6000, "Ann", "before the scan")]).await;
    // A second call is listed, and joined slowly while the module is turned off and scanned.
    a.calls(&[("c1", THREAD), ("c2", "19:two@thread.v2")]).await;
    a.echo("set-join", json!({"slow": true})).await;
    a.d.registry.ui_ready().store(false, Ordering::SeqCst);
    a.at(20_000).await;
    a.d.registry.ui_ready().store(true, Ordering::SeqCst);
    let store = common::store(a.d.dir.path());
    let scan = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        open_mess::modules::set_enabled(&store, "test-echo", false).unwrap();
        a.d.accounts.rescan_modules(None).await.unwrap();
    };
    tokio::join!(a.at(25_000), scan);
    // The cue in the buffer was read before the version that went ended its sessions.
    let c1 = a.kept().into_iter().find(|(_, meta)| meta.call.as_deref() == Some("c1")).unwrap().0;
    assert_eq!(a.cues(&c1), ["before the scan"]);
    // The module is gone: the recording ends, interrupted, at the next list.
    a.at(40_000).await;
    assert_eq!(a.recordings().meta(&c1).unwrap().reason, Some(Reason::Interrupted));
    // The join that ran during the scan left no session behind: it was ended after the join.
    open_mess::modules::set_enabled(&store, "test-echo", true).unwrap();
    a.d.accounts.rescan_modules(None).await.unwrap();
    let c2 = a.echo("read_captions", json!({"conversation": "19:two@thread.v2"})).await;
    assert_eq!((&c2["state"], &c2["reason"]), (&json!("ended"), &json!("update")));
}

#[tokio::test]
async fn an_agent_may_not_read_a_session_with_a_thread_it_may_not_read() {
    let a = Archive::new().await;
    // An agent's session of a call without a chat; the module learned the call's thread.
    a.echo("join_captions", json!({"conversation": "call:c5", "call": "c5"})).await;
    a.echo("captions-say", json!({"conversation": "call:c5", "threads": ["19:hidden@thread.v2"]})).await;
    permissions::set(&common::store(a.d.dir.path()), "test-echo:a", Some("19:hidden@thread.v2"), false, Write::Never).unwrap();
    let args = json!({"account": "test-echo:a", "conversation": "call:c5"});
    let read = a.d.registry.call("test_echo_read_captions", args.clone()).await;
    assert!(matches!(read, Err(ToolError::NotAllowed(_))), "{read:?}");
    // Known now: refused before the module is called, so the session goes on.
    assert!(matches!(a.d.registry.call("test_echo_leave_captions", args.clone()).await, Err(ToolError::NotAllowed(_))));
    assert_eq!(a.leaves().await, 0);
    assert_eq!(a.echo("read_captions", json!({"conversation": "call:c5"})).await["state"], "live");
    // Allowed again, the agent reads it.
    permissions::set(&common::store(a.d.dir.path()), "test-echo:a", Some("19:hidden@thread.v2"), true, Write::Never).unwrap();
    assert_eq!(a.d.registry.call("test_echo_read_captions", args).await.unwrap()["state"], "live");
}

#[tokio::test]
async fn an_escalated_call_s_earlier_chat_must_be_readable_too() {
    let a = Archive::new().await;
    let (one_to_one, group) = ("19:a_b@unq.gbl.spaces", "19:group@thread.v2");
    // A 1:1 call a third person joined: the session's buffer holds what was said in both chats.
    a.echo("join_captions", json!({"conversation": one_to_one, "call": "c7"})).await;
    a.say(one_to_one, &[(1000, "Ann", "said in the 1:1 call")]).await;
    a.echo("captions-say", json!({"conversation": one_to_one, "threads": [one_to_one, group]})).await;
    // The agent may read the new group chat only.
    permissions::set(&common::store(a.d.dir.path()), "test-echo:a", Some(one_to_one), false, Write::Never).unwrap();
    let args = json!({"account": "test-echo:a", "conversation": group});
    let read = a.d.registry.call("test_echo_read_captions", args.clone()).await;
    assert!(matches!(read, Err(ToolError::NotAllowed(_))), "{read:?}");
    // Known from then on: refused before the module runs.
    assert!(matches!(a.d.registry.call("test_echo_leave_captions", args).await, Err(ToolError::NotAllowed(_))));
    assert_eq!(a.leaves().await, 0);
    // The archive of that call: its recording names both chats, so the agent cannot read it either.
    a.calls(&[("c7", group)]).await;
    a.at(0).await;
    a.at(5000).await;
    let (id, meta) = &a.kept()[0];
    assert!(meta.threads.contains(&one_to_one.to_string()) && meta.threads.contains(&group.to_string()), "{:?}", meta.threads);
    let page = a.d.registry.call("get_recording_transcript", json!({"recording": id})).await;
    assert!(matches!(page, Err(ToolError::NotAllowed(_))), "{page:?}");
}

#[tokio::test]
async fn what_the_archive_learned_of_a_session_s_threads_holds_an_agent_s_first_call() {
    let a = Archive::new().await;
    let (one_to_one, group) = ("19:a_b@unq.gbl.spaces", "19:group@thread.v2");
    a.calls(&[("c8", one_to_one)]).await;
    a.at(0).await;
    a.at(5000).await;
    // A person joins: the chat is a group now, and the archive reads that.
    a.echo("captions-say", json!({"conversation": one_to_one, "threads": [one_to_one, group]})).await;
    a.calls(&[("c8", group)]).await;
    a.at(20_000).await;
    permissions::set(&common::store(a.d.dir.path()), "test-echo:a", Some(one_to_one), false, Write::Never).unwrap();
    // The agent's very first call, a leave under the group's name, never reaches the module.
    let leave = a.d.registry.call("test_echo_leave_captions", json!({"account": "test-echo:a", "conversation": group})).await;
    assert!(matches!(leave, Err(ToolError::NotAllowed(_))), "{leave:?}");
    assert_eq!(a.leaves().await, 0);
}

/// Makes a file read-only, or writable again.
fn read_only(path: &std::path::Path, on: bool) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(on);
    std::fs::set_permissions(path, permissions).unwrap();
}

// Windows only: a read-only file stops the state's rename over it there, not on Linux (where
// open_mess does not run).
#[cfg(windows)]
#[tokio::test]
async fn a_stop_holds_when_its_state_cannot_be_written() {
    let mut a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    let state = a.d.dir.path().join("local").join("recordings").join(archive::CALLS);
    read_only(&state, true);
    a.archiver.stop(|_, _| true).await;
    // The state on the disk still says active; the recording's "stopped" keeps the call stopped.
    assert_eq!(a.call_state("c1"), Some(CallState::Active));
    a.restart();
    a.at(MINUTE).await;
    a.at(MINUTE + 5000).await;
    assert_eq!(a.joins().await.len(), 1);
    // Deleting it now would take that away: refused while the state cannot be written.
    let id = a.kept()[0].0.clone();
    assert!(a.archiver.delete(&id).await.is_err());
    assert!(a.recordings().meta(&id).is_ok());
    // A recording that keeps no call stopped goes all the same.
    let other = a.recordings().create_with(&Meta { source: Source::Captions, state: State::Done, ..Meta::default() }).unwrap();
    a.archiver.delete(&other).await.unwrap();
    read_only(&state, false);
    a.archiver.delete(&id).await.unwrap();
    assert_eq!(a.call_state("c1"), Some(CallState::Blocked));
}

#[tokio::test]
async fn a_session_whose_reads_keep_failing_is_joined_again_after_a_minute() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    a.echo("captions-say", json!({"conversation": THREAD, "forget": true})).await;
    for step in 2..13 {
        a.at(step * 5000).await;
    }
    assert_eq!(a.joins().await.len(), 1);
    // A minute of failed reads: the session counts as gone, and the call is joined again.
    a.at(65_000).await;
    let id = &a.kept()[0].0;
    assert_eq!((a.joins().await.len(), a.gaps(id)), (2, vec![GapReason::LateStart, GapReason::Rejoin]));
    // Not listed any more and still unread: the recording ends instead of staying open.
    a.echo("captions-say", json!({"conversation": THREAD, "forget": true})).await;
    a.calls(&[]).await;
    for step in 14..30 {
        a.at(step * 5000).await;
    }
    assert_eq!(a.recordings().meta(id).unwrap().state, State::Done);
}

#[tokio::test]
async fn a_session_joined_again_after_failed_reads_writes_no_cue_twice() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    // More cues than a page holds.
    let said: Vec<String> = (0..150).map(|n| format!("cue {n}")).collect();
    let cues: Vec<(u64, &str, &str)> = said.iter().map(|text| (6000, "Ann", text.as_str())).collect();
    a.say(THREAD, &cues).await;
    a.at(10_000).await;
    // The module answers no read for two minutes, its session going on: given up twice, and
    // joined again each time, the join getting that session back.
    a.echo("captions-say", json!({"conversation": THREAD, "fail_reads": 24})).await;
    for step in 3..28 {
        a.at(step * 5000).await;
    }
    a.say(THREAD, &[(141_000, "Ann", "after")]).await;
    for step in 29..33 {
        a.at(step * 5000).await;
    }
    let id = &a.kept()[0].0;
    assert_eq!(a.joins().await.len(), 3);
    let mut expected = said.clone();
    expected.push("after".into());
    assert_eq!(a.cues(id), expected);
}

#[tokio::test]
async fn cues_that_cannot_be_written_are_a_gap() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    a.say(THREAD, &[(6000, "Ann", "kept")]).await;
    a.at(10_000).await;
    let id = a.kept()[0].0.clone();
    let file = a.recordings().path(&id).unwrap().join(archive::CAPTIONS);
    read_only(&file, true);
    a.say(THREAD, &[(11_000, "Ann", "not written")]).await;
    a.at(15_000).await;
    read_only(&file, false);
    assert_eq!(a.cues(&id), ["kept"]);
    assert_eq!(a.gaps(&id), [GapReason::LateStart, GapReason::Lost]);
}

#[tokio::test]
async fn a_removed_account_ends_its_recording_interrupted_in_one_pass() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    a.d.accounts.remove("test-echo:a").await.unwrap();
    a.at(25_000).await;
    let meta = &a.kept()[0].1;
    assert_eq!((meta.state, meta.reason), (State::Done, Some(Reason::Interrupted)));
    assert_eq!(a.gaps(&a.kept()[0].0), [GapReason::LateStart, GapReason::Lost]);
}

#[tokio::test]
async fn twelve_hours_go_on_in_a_new_recording() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    a.at(12 * 60 * MINUTE).await;
    let kept = a.kept();
    let [(new, newer), (old, older)] = &kept[..] else { panic!("{kept:?}") };
    assert_eq!((older.state, older.reason), (State::Done, Some(Reason::TooLong)));
    assert_eq!((newer.state, newer.continues.as_deref()), (State::Recording, Some(old.as_str())));
    a.say(THREAD, &[(12 * 60 * MINUTE + 1000, "Ann", "later")]).await;
    a.at(12 * 60 * MINUTE + 5000).await;
    assert_eq!(a.cues(new), ["later"]);
    assert_eq!(a.joins().await.len(), 1);
}

#[tokio::test]
async fn two_calls_at_once_and_threads_a_call_gains() {
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD), ("c2", "call:c2")]).await;
    a.at(0).await;
    a.at(5000).await;
    assert_eq!(a.kept().len(), 2);
    // A person joins the 1:1 call: its new group chat is a thread of the recording too.
    a.echo("captions-say", json!({"conversation": "call:c2", "threads": ["19:group@thread.v2"]})).await;
    a.at(10_000).await;
    let c2 = a.kept().into_iter().find(|(_, meta)| meta.call.as_deref() == Some("c2")).unwrap().1;
    assert_eq!((c2.conversation.as_deref(), c2.threads), (Some("call:c2"), vec!["19:group@thread.v2".to_string()]));
    // `connection_limit` is tried again, however long.
    a.calls(&[("c1", THREAD), ("c2", "call:c2"), ("c3", "19:three@thread.v2")]).await;
    a.echo("set-join", json!({"error": "connection_limit: at most 4 sessions"})).await;
    for step in 0..21 {
        a.at(MINUTE + step * 2 * MINUTE).await;
    }
    a.echo("set-join", json!({})).await;
    a.at(45 * MINUTE).await;
    a.at(45 * MINUTE + 5000).await;
    assert_eq!(a.kept().len(), 3);
}

#[tokio::test]
async fn stop_recording_stops_only_what_the_agent_may_read() {
    let a = Archive::new().await;
    let mut registry = open_mess::tools::Registry::new(std::time::Instant::now(), Arc::clone(&a.d.accounts));
    registry.set_recordings(a.recordings(), None);
    registry.set_archiver(Arc::clone(&a.archiver));
    a.calls(&[("c1", THREAD), ("c2", "19:secret@thread.v2")]).await;
    a.at(0).await;
    a.at(5000).await;
    let secret = a.kept().into_iter().find(|(_, meta)| meta.call.as_deref() == Some("c2")).unwrap().0;
    permissions::set(&common::store(a.d.dir.path()), "test-echo:a", Some("19:secret@thread.v2"), false, Write::Never).unwrap();
    assert!(matches!(registry.call("stop_recording", json!({"recording": secret})).await, Err(ToolError::NotAllowed(_))));
    assert!(matches!(registry.call("stop_recording", json!({"recording": "0".repeat(32)})).await, Err(ToolError::InvalidArgs(_))));
    let stopped = registry.call("stop_recording", json!({})).await.unwrap();
    let open = a.kept().into_iter().find(|(_, meta)| meta.call.as_deref() == Some("c1")).unwrap().0;
    assert_eq!(stopped, json!({"stopped": [open]}));
    assert_eq!(a.recordings().meta(&secret).unwrap().state, State::Recording);
    assert_eq!(registry.call("status", json!({})).await.unwrap()["captions"]["active"], 1);
}

#[tokio::test]
async fn the_log_of_a_pass_has_no_text_names_or_threads() {
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // For the whole process: a thread's own subscriber misses events whose interest another test
    // thread settled first. The other tests of this file log into it too, all of them checked.
    let sink = Sink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt().with_writer(move || writer.clone()).with_max_level(tracing::Level::TRACE).finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let a = Archive::new().await;
    a.calls(&[("c1", THREAD)]).await;
    a.at(0).await;
    a.at(5000).await;
    a.say(THREAD, &[(6000, "Secret Speaker", "secret words")]).await;
    a.at(10_000).await;
    a.archiver.stop(|_, _| true).await;
    let log = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("a recording started"), "{log}");
    for secret in ["secret words", "Secret Speaker", THREAD, "meeting_abc"] {
        assert!(!log.contains(secret), "{secret} in {log}");
    }
}

fn captions_meta(recordings: &Recordings, conversation: &str, threads: &[&str]) -> String {
    let meta = Meta {
        source: Source::Captions,
        start_ms: 1_700_000_000_000,
        account: Some("test-echo:a".into()),
        conversation: Some(conversation.into()),
        threads: threads.iter().map(|thread| (*thread).to_string()).collect(),
        call: Some("call-1".into()),
        ..Meta::default()
    };
    let id = recordings.create_with(&meta).unwrap();
    archive::append_cues(&recordings.path(&id).unwrap(), &[Cue::capped(0, "Ann", false, "said")]).unwrap();
    id
}

#[tokio::test]
async fn a_captions_recording_is_read_under_its_conversation_s_permission() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let recordings = Recordings::new(&d.dir.path().join("local"));
    let chat = captions_meta(&recordings, "19:chat@thread.v2", &["19:chat@thread.v2", "19:group@thread.v2"]);
    let bare = captions_meta(&recordings, "call:call-2", &[]);
    let read = |id: &str| d.registry.call("get_recording_transcript", json!({"recording": id}));
    let listed = || async { d.registry.call("list_recordings", json!({})).await.unwrap()["items"].as_array().unwrap().len() };
    assert_eq!(read(&chat).await.unwrap()["untrusted"]["cues"][0]["text"], "said");
    assert_eq!(listed().await, 2);

    let store = common::store(d.dir.path());
    // A call without a chat follows its own name's override, like any conversation.
    permissions::set(&store, "test-echo:a", Some("call:call-2"), false, Write::Never).unwrap();
    assert!(matches!(read(&bare).await, Err(ToolError::NotAllowed(_))));
    // The text of the chat it escalated to needs that chat too.
    permissions::set(&store, "test-echo:a", Some("19:group@thread.v2"), false, Write::Never).unwrap();
    assert!(matches!(read(&chat).await, Err(ToolError::NotAllowed(_))));
    assert_eq!(listed().await, 0);
    permissions::set(&store, "test-echo:a", Some("19:group@thread.v2"), true, Write::Never).unwrap();
    assert!(read(&chat).await.is_ok());

    // A removed account's recordings are the owner's only, though its permissions went with it.
    d.accounts.remove("test-echo:a").await.unwrap();
    assert!(matches!(read(&chat).await, Err(ToolError::NotAllowed(_))));
    assert_eq!(listed().await, 0);
}
