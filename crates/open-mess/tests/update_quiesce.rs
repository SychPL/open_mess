//! An update stopping the daemon (spec 2026-10-27-self-update, 7.1): phase A lets the work finish,
//! phase B refuses new tool calls, then the helper's scheduled task starts and the daemon exits once
//! the helper holds `update.lock`. The clock is tokio's paused one wherever no module runs; the
//! scheduled task is a fake.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use open_mess::permissions::{self, Write};
use open_mess::tools::{GateMode, ToolError};
use open_mess::triggers::Counters;
use open_mess::triggers::engine::Batch;
use open_mess::triggers::rules::Spawn;
use open_mess::triggers::spawn::{self, Runs};
use open_mess::update::UpdateError;
use open_mess::update::fetch::Downloaded;
use open_mess::update::quiesce::{self, Activity, Cancelled, Launcher};
use open_mess::update::release::{Version, manifest_sha256};
use open_mess::update::state::{self, Paths, PendingFile, Read, Stage};
use open_mess_host::{HostEvent, SessionState};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

const SECOND: Duration = Duration::from_secs(1);

/// Everything phase A watches, over a daemon without modules.
struct Setup {
    d: common::Daemon,
    activity: Arc<Activity>,
    /// The modules' event queue, as the bus takes from it.
    queue: mpsc::Receiver<HostEvent>,
    cancel: watch::Sender<bool>,
}

fn activity(d: &common::Daemon, runs: Arc<Runs>, host_events: mpsc::Sender<HostEvent>) -> Activity {
    Activity {
        gate: d.registry.gate().clone(),
        triggers: Arc::default(),
        pending: Arc::clone(d.registry.pending().unwrap()),
        runs,
        modules: d.accounts.modules().handles(),
        host_events,
    }
}

fn runs(d: &common::Daemon) -> Arc<Runs> {
    Runs::new(d.dir.path().join("runs"), Duration::from_secs(60 * 60), Counters::default())
}

async fn setup() -> Setup {
    let d = common::daemon(&[]).await;
    let (sender, queue) = mpsc::channel(8);
    let activity = Arc::new(activity(&d, runs(&d), sender));
    Setup { d, activity, queue, cancel: watch::channel(false).0 }
}

/// Phase A in its own task; it answers with its result and when it ended.
fn phase_a(t: &Setup) -> JoinHandle<(Result<(), Cancelled>, Instant)> {
    let (activity, cancel) = (Arc::clone(&t.activity), t.cancel.subscribe());
    tokio::spawn(async move { (quiesce::finish_work(&activity, &cancel).await, Instant::now()) })
}

fn store(counter: &std::sync::atomic::AtomicU64, value: u64) {
    counter.store(value, Ordering::SeqCst);
}

#[tokio::test(start_paused = true)]
async fn phase_a_waits_for_an_open_batch() {
    let mut t = setup().await;
    let triggers = &t.activity.triggers;
    let phase = phase_a(&t);
    // Each step of the chain from a module's event to its rule's action keeps phase A waiting, long
    // past its 60 s of quiet (and within its 30 minutes): the event in the modules' queue, taken
    // from it but not yet consumed, in an open batch, being delivered.
    t.activity.host_events.try_send(HostEvent::Session { module: "m".into(), account: "a".into(), state: SessionState::Online }).unwrap();
    sleep(300 * SECOND).await;
    assert!(!phase.is_finished());
    t.queue.recv().await.unwrap();
    store(&triggers.received, 1);
    sleep(300 * SECOND).await;
    assert!(!phase.is_finished());
    triggers.open_batches.store(1, Ordering::SeqCst);
    store(&triggers.consumed, 1);
    sleep(300 * SECOND).await;
    assert!(!phase.is_finished());
    triggers.delivering.store(1, Ordering::SeqCst);
    triggers.open_batches.store(0, Ordering::SeqCst);
    sleep(300 * SECOND).await;
    assert!(!phase.is_finished());
    // Delivered: 60 s of quiet after it.
    store(&triggers.last_delivery_ms, 1_800_000_000_000);
    triggers.delivering.store(0, Ordering::SeqCst);
    let delivered = Instant::now();
    let (result, ended) = phase.await.unwrap();
    assert_eq!(result, Ok(()));
    assert!((60 * SECOND..61 * SECOND).contains(&(ended - delivered)), "{:?}", ended - delivered);
}

#[tokio::test(start_paused = true)]
async fn phase_a_waits_for_60_s_of_quiet() {
    let t = setup().await;
    let started = Instant::now();
    let phase = phase_a(&t);
    sleep(30 * SECOND).await;
    t.d.registry.call("status", json!({})).await.unwrap();
    sleep(59 * SECOND).await;
    assert!(!phase.is_finished());
    let (result, ended) = phase.await.unwrap();
    assert_eq!(result, Ok(()));
    assert!((90 * SECOND..91 * SECOND).contains(&(ended - started)), "{:?}", ended - started);
}

#[tokio::test(start_paused = true)]
async fn phase_a_waits_for_a_running_spawn() {
    let t = setup().await;
    let out = t.d.dir.path().join("run.json");
    // A real program, which runs for 2 s of real time.
    let program = Spawn {
        program: PathBuf::from(env!("CARGO_BIN_EXE_open-mess-record-run")),
        args: vec![out.display().to_string(), "--sleep".into(), "2000".into()],
        working_dir: None,
    };
    let batch =
        Batch { account: "test-echo:a".into(), conversation: "c1".into(), kind: "direct", messages: Vec::new(), count: 0, first_ms: 1, last_ms: 1 };
    spawn::deliver(&t.activity.runs, "r", &program, &batch, Box::new(|| true));
    let phase = phase_a(&t);
    // The paused clock runs far ahead of the program.
    sleep(120 * SECOND).await;
    assert_eq!(t.activity.runs.running(), 1);
    assert!(!phase.is_finished());
    // Real time passes while the paused clock stands still: this task never lets the runtime idle.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while t.activity.runs.running() > 0 {
        assert!(std::time::Instant::now() < deadline, "the program did not end");
        tokio::task::yield_now().await;
    }
    let run_ended = Instant::now();
    let (result, ended) = phase.await.unwrap();
    assert_eq!(result, Ok(()));
    assert!(ended - run_ended < SECOND, "{:?}", ended - run_ended);
}

#[tokio::test(start_paused = true)]
async fn phase_a_gives_up_after_30_min() {
    let t = setup().await;
    let started = Instant::now();
    let phase = phase_a(&t);
    // An agent calls a tool every 30 s: phase A never sees 60 s of quiet.
    while !phase.is_finished() {
        sleep(30 * SECOND).await;
        // A call's end is in wall-clock milliseconds, which the paused clock does not move.
        std::thread::sleep(Duration::from_millis(2));
        t.d.registry.call("status", json!({})).await.unwrap();
    }
    let (result, ended) = phase.await.unwrap();
    assert_eq!(result, Ok(()));
    assert!((1800 * SECOND..1801 * SECOND).contains(&(ended - started)), "{:?}", ended - started);
}

async fn writable() -> common::Daemon {
    let d = common::daemon(&[common::ECHO]).await;
    d.accounts.add("test-echo", common::info("a")).unwrap();
    permissions::set(&common::store(d.dir.path()), "test-echo:a", None, true, Write::Autonomous).unwrap();
    d
}

fn send(text: &str) -> Value {
    json!({"account": "test-echo:a", "conversation": "c1", "text": text})
}

fn is_update_in_progress(result: &Result<Value, ToolError>) -> bool {
    matches!(result, Err(ToolError::Failed(why)) if why == "update in progress, try again shortly")
}

#[tokio::test]
async fn phase_b_rejects_new_calls_and_waits_for_in_flight() {
    let d = writable().await;
    let activity = activity(&d, runs(&d), mpsc::channel(8).0);
    let (_cancel, cancelled) = watch::channel(false);
    // test-echo answers this write only at its call limit (2 s in tests).
    let slow = async {
        let result = d.registry.call("send", send("slow")).await;
        (result, Instant::now())
    };
    let phase_b = async {
        while activity.gate.in_flight() == 0 {
            tokio::task::yield_now().await;
        }
        (quiesce::switch(&activity, &cancelled).await, Instant::now())
    };
    let refused = async {
        sleep(Duration::from_millis(500)).await;
        (d.registry.call("status", json!({})).await, d.registry.call("send", send("hi")).await)
    };
    let ((sent, sent_at), (switched, switched_at), (status, write)) = tokio::join!(slow, phase_b, refused);
    assert_eq!(sent.unwrap()["state"], "uncertain");
    assert_eq!(switched, Ok(()));
    assert!(switched_at >= sent_at);
    assert!(is_update_in_progress(&status) && is_update_in_progress(&write));
    // The gate stays closed: only a cancel or a failed hand-over opens it again.
    assert!(is_update_in_progress(&d.registry.call("status", json!({})).await));
}

#[tokio::test]
async fn phase_b_waits_for_waiting_actions() {
    let d = common::daemon(&[common::ECHO]).await;
    // The default write permission: every write waits for the user.
    d.accounts.add("test-echo", common::info("a")).unwrap();
    let waiting = d.registry.call("send", send("hello")).await.unwrap();
    assert_eq!(waiting["state"], "waiting");
    let activity = activity(&d, runs(&d), mpsc::channel(8).0);
    let (_cancel, cancelled) = watch::channel(false);
    assert!(tokio::time::timeout(SECOND, quiesce::switch(&activity, &cancelled)).await.is_err());
    // Approving in the window still works, and lets phase B end.
    let action = waiting["action"].as_str().unwrap();
    assert_eq!(activity.pending.decide(action, true).await.unwrap(), open_mess::pending::State::Done);
    assert_eq!(tokio::time::timeout(SECOND, quiesce::switch(&activity, &cancelled)).await.unwrap(), Ok(()));
}

static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The modules' logs: test-echo logs the timer events it gets.
fn logs() -> String {
    static INIT: Once = Once::new();
    INIT.call_once(|| tracing_subscriber::fmt().with_writer(|| Capture).init());
    String::from_utf8_lossy(&LOGS.lock().unwrap()).into_owned()
}

async fn wait_for_log(needle: &str) -> bool {
    for _ in 0..100 {
        if logs().contains(needle) {
            return true;
        }
        sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn cancel_resumes_events_and_calls() {
    logs();
    let d = common::daemon(&[common::ECHO]).await;
    d.accounts.add("test-echo", common::info("a")).unwrap();
    let activity = activity(&d, runs(&d), mpsc::channel(8).0);
    assert_eq!(activity.modules.len(), 1);
    let echo = d.accounts.modules().handle("test-echo").unwrap();
    let (cancel, cancelled) = watch::channel(false);
    // Phase A pauses the module: a timer it sets meanwhile does not fire.
    let (finished, ()) = tokio::join!(quiesce::finish_work(&activity, &cancelled), async {
        sleep(Duration::from_millis(300)).await;
        echo.call("a", "timer-at", r#"{"id":71}"#).await.unwrap();
        sleep(Duration::from_millis(500)).await;
        assert!(!logs().contains("timer 71"), "{}", logs());
        cancel.send_replace(true);
    });
    assert_eq!(finished, Err(Cancelled));
    assert!(wait_for_log("timer 71").await, "{}", logs());
    // Phase B, kept going by a waiting action: refused calls until the cancel, taken again after.
    assert_eq!(d.registry.call("send", send("hello")).await.unwrap()["state"], "waiting");
    cancel.send_replace(false);
    let (switched, refused) = tokio::join!(quiesce::switch(&activity, &cancelled), async {
        sleep(Duration::from_millis(300)).await;
        let refused = d.registry.call("status", json!({})).await;
        cancel.send_replace(true);
        refused
    });
    assert!(is_update_in_progress(&refused));
    assert_eq!(switched, Err(Cancelled));
    d.registry.call("status", json!({})).await.unwrap();
    assert_eq!(d.registry.call("send", send("again")).await.unwrap()["state"], "waiting");
}

/// The release the daemon downloaded, with its own `om.exe`.
struct Release {
    _dirs: TempDir,
    paths: Paths,
    downloaded: Downloaded,
}

const OM_EXE: &[u8] = b"MZ the new om.exe";

fn release() -> Release {
    let dirs = tempfile::tempdir().unwrap();
    let paths = Paths { data: dirs.path().join("data"), local: dirs.path().join("local") };
    std::fs::create_dir_all(&paths.data).unwrap();
    let dir = paths.updates().join("1.2.0");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("om.exe"), OM_EXE).unwrap();
    let file = json!({"name": "om.exe", "kind": "exe", "size": OM_EXE.len(), "sha256": manifest_sha256(OM_EXE)});
    let manifest = json!({"version": "1.2.0", "files": [file]}).to_string();
    std::fs::write(dir.join("manifest.json"), &manifest).unwrap();
    let downloaded = Downloaded { version: Version(1, 2, 0), manifest_sha256: manifest_sha256(manifest.as_bytes()), dir };
    Release { _dirs: dirs, paths, downloaded }
}

/// The helper's scheduled task: the helper takes `update.lock` this long after the launch, or never.
struct Helper {
    after: Option<Duration>,
    launched: Mutex<Vec<(PathBuf, PathBuf)>>,
    lock: Arc<Mutex<Option<File>>>,
}

impl Helper {
    fn starting_after(after: Option<Duration>) -> Helper {
        Helper { after, launched: Mutex::default(), lock: Arc::default() }
    }

    fn launched(&self) -> Vec<(PathBuf, PathBuf)> {
        self.launched.lock().unwrap().clone()
    }
}

impl Launcher for Helper {
    fn launch_helper(&self, helper: &Path, data: &Path) -> Result<(), UpdateError> {
        self.launched.lock().unwrap().push((helper.to_path_buf(), data.to_path_buf()));
        if let Some(after) = self.after {
            let (lock, file) = (Arc::clone(&self.lock), data.join("update.lock"));
            tokio::spawn(async move {
                sleep(after).await;
                *lock.lock().unwrap() = state::try_lock(&file, Duration::ZERO).unwrap();
            });
        }
        Ok(())
    }
}

/// Whether `exit` was notified (its stored permit is taken).
async fn notified(exit: &Notify) -> bool {
    tokio::time::timeout(Duration::ZERO, exit.notified()).await.is_ok()
}

#[tokio::test(start_paused = true)]
async fn exits_once_update_lock_is_held() {
    let r = release();
    let helper = Helper::starting_after(Some(5 * SECOND));
    let exit = Notify::new();
    let started = Instant::now();
    let (launched, ()) = tokio::join!(quiesce::launch(&r.paths, &r.downloaded, vec!["test-echo".into()], &helper, &exit), async {
        sleep(4 * SECOND).await;
        assert!(!notified(&exit).await);
    });
    launched.unwrap();
    assert!((5 * SECOND..6 * SECOND).contains(&started.elapsed()), "{:?}", started.elapsed());
    assert!(notified(&exit).await);
    let program = r.paths.bin().join("om.update.exe");
    assert_eq!(helper.launched(), [(program.clone(), r.paths.data.clone())]);
    assert_eq!(std::fs::read(&program).unwrap(), OM_EXE);
    let Read::Found(pending) = state::read(&r.paths.pending_file()) else { panic!("no pending file") };
    let expected = PendingFile {
        version: "1.2.0".into(),
        manifest_sha256: r.downloaded.manifest_sha256.clone(),
        update_dir: r.downloaded.dir.clone(),
        stage: Stage::Started,
        active_modules: vec!["test-echo".into()],
        added: Vec::new(),
        replaced: Vec::new(),
    };
    assert_eq!(pending, expected);
    assert!(state::helper_alive(&r.paths));
}

#[tokio::test(start_paused = true)]
async fn helper_not_started_resumes_after_30_s() {
    let t = setup().await;
    t.activity.gate.set(GateMode::Closed);
    let r = release();
    let helper = Helper::starting_after(None);
    let exit = Notify::new();
    let started = Instant::now();
    let result = quiesce::launch(&r.paths, &r.downloaded, Vec::new(), &helper, &exit).await;
    assert!(matches!(&result, Err(UpdateError::HelperNotStarted)), "{result:?}");
    assert_eq!(result.unwrap_err().to_string(), "pomocnik nie wystartował");
    assert!((30 * SECOND..31 * SECOND).contains(&started.elapsed()), "{:?}", started.elapsed());
    assert!(!notified(&exit).await);
    // A helper that starts late finds nothing to install; the lock is free for it.
    assert!(matches!(state::read(&r.paths.pending_file()), Read::Missing));
    assert!(!state::helper_alive(&r.paths));
    // The daemon goes back to work.
    quiesce::resume(&t.activity).await;
    t.d.registry.call("status", json!({})).await.unwrap();
}

#[tokio::test]
async fn changed_om_exe_is_not_launched() {
    let r = release();
    std::fs::create_dir_all(r.paths.bin()).unwrap();
    std::fs::write(r.paths.bin().join("om.update.exe"), b"the previous helper").unwrap();
    std::fs::write(r.downloaded.dir.join("om.exe"), b"MZ changed after the download").unwrap();
    let helper = Helper::starting_after(Some(SECOND));
    let result = quiesce::launch(&r.paths, &r.downloaded, Vec::new(), &helper, &Notify::new()).await;
    assert!(matches!(&result, Err(UpdateError::Rejected(_))), "{result:?}");
    assert!(helper.launched().is_empty());
    assert!(matches!(state::read(&r.paths.pending_file()), Read::Missing));
    assert_eq!(std::fs::read(r.paths.bin().join("om.update.exe")).unwrap(), b"the previous helper");
}

#[test]
fn pending_file_write_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("update-pending.json");
    let file = |added: usize| PendingFile {
        version: "1.2.0".into(),
        manifest_sha256: "aa".repeat(32),
        update_dir: dir.path().join("updates").join("1.2.0"),
        stage: Stage::BackedUp,
        active_modules: vec!["teams".into()],
        added: (0..added).map(|n| PathBuf::from(format!(r"C:\Users\someone\AppData\Roaming\open_mess\modules\module-{n}.wasm"))).collect(),
        replaced: Vec::new(),
    };
    state::write(&path, &file(0)).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let reader = std::thread::spawn({
        let (path, done) = (path.clone(), Arc::clone(&done));
        move || {
            let mut whole = 0;
            while !done.load(Ordering::SeqCst) {
                // A file the writer holds may not open for a moment; one that opens is whole.
                if let Ok(bytes) = std::fs::read(&path) {
                    let parsed: Result<PendingFile, _> = serde_json::from_slice(&bytes);
                    assert!(parsed.is_ok(), "half a file: {} bytes", bytes.len());
                    whole += 1;
                }
            }
            whole
        }
    });
    for n in 0..300 {
        state::write(&path, &file(n % 30 * 50)).unwrap();
    }
    done.store(true, Ordering::SeqCst);
    assert!(reader.join().unwrap() > 0);
    assert!(matches!(state::read(&path), Read::Found(found) if found == file(299 % 30 * 50)));
    assert!(matches!(state::read(&dir.path().join("missing.json")), Read::Missing));
    std::fs::write(&path, "{\"version\": ").unwrap();
    assert!(matches!(state::read(&path), Read::Unreadable));
}
