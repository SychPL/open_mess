//! The daemon's side of an update in progress (spec 2026-10-27-self-update, 7.2, 7.4-7.6): what it
//! does at start with `update-pending.json`, the probation while the helper checks the new version,
//! its own check once the helper is gone, and the cleanup after a successful installation. The
//! helper is the test holding `update.lock`; its scheduled task is a fake.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use open_mess::pending::{Kind, State};
use open_mess::tools::ToolError;
use open_mess::triggers::Counters;
use open_mess::triggers::rules::{Action, Condition, Rule};
use open_mess::triggers::spawn::{RUN_TIMEOUT, Runs};
use open_mess::ui::UiState;
use open_mess::ui::commands::{self, RuleDto, UPDATE_IN_PROGRESS};
use open_mess::update::UpdateError;
use open_mess::update::probation::{self, Start};
use open_mess::update::quiesce::{Activity, Launcher};
use open_mess::update::release::Version;
use open_mess::update::state::{self, Paths, PendingFile, Read, Stage};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

const SECOND: Duration = Duration::from_secs(1);
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The update's folders, in a temp dir.
fn paths() -> (TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { data: dir.path().join("data"), local: dir.path().join("local") };
    std::fs::create_dir_all(&paths.data).unwrap();
    std::fs::create_dir_all(paths.bin()).unwrap();
    (dir, paths)
}

/// `update-pending.json` of this version's installation at `stage`.
fn write_stage(paths: &Paths, stage: Stage) -> PendingFile {
    let file = PendingFile {
        version: VERSION.into(),
        manifest_sha256: "ab".repeat(32),
        update_dir: paths.updates().join(VERSION),
        stage,
        active_modules: vec!["test-echo".into()],
        added: vec![paths.bin().join("om.new.exe")],
        replaced: Vec::new(),
    };
    state::write(&paths.pending_file(), &file).unwrap();
    file
}

fn stage_file(paths: &Paths) -> Option<PendingFile> {
    match state::read(&paths.pending_file()) {
        Read::Found(file) => Some(file),
        Read::Missing => None,
        other => panic!("{other:?}"),
    }
}

/// The helper is alive while this lives.
fn helper_alive(paths: &Paths) -> File {
    state::try_lock(&paths.lock_file(), Duration::ZERO).unwrap().unwrap()
}

/// The helper's scheduled task.
#[derive(Default)]
struct Task {
    launched: Mutex<Vec<(PathBuf, PathBuf)>>,
}

impl Task {
    fn launched(&self) -> Vec<(PathBuf, PathBuf)> {
        self.launched.lock().unwrap().clone()
    }
}

impl Launcher for Task {
    fn launch_helper(&self, helper: &Path, data: &Path) -> Result<(), UpdateError> {
        self.launched.lock().unwrap().push((helper.to_path_buf(), data.to_path_buf()));
        Ok(())
    }
}

/// What `status` of this daemon says: `version` with test-echo back, the window up, the accounts
/// restored or not, and test-echo's crashes since it was loaded.
fn status(ui_ready: bool, accounts_restored: bool, crashes: u64) -> Value {
    json!({
        "version": VERSION,
        "ui_ready": ui_ready,
        "accounts_restored": accounts_restored,
        "modules": [{"name": "test-echo", "state": "active", "crashes_since_load": crashes}],
    })
}

/// A daemon in probation: its activity, the update's folders and the helper's task.
struct Setup {
    d: common::Daemon,
    activity: Arc<Activity>,
    _dir: TempDir,
    paths: Paths,
    task: Arc<Task>,
    status: Arc<Mutex<Value>>,
    exit: Arc<Notify>,
}

fn activity(d: &common::Daemon) -> Activity {
    Activity {
        gate: d.registry.gate().clone(),
        triggers: Arc::default(),
        pending: Arc::clone(d.registry.pending().unwrap()),
        runs: Runs::new(d.dir.path().join("runs"), RUN_TIMEOUT, Counters::default()),
        modules: d.accounts.modules().handles(),
        host_events: mpsc::channel(8).0,
    }
}

async fn setup() -> Setup {
    setup_with(common::daemon(&[]).await)
}

fn setup_with(d: common::Daemon) -> Setup {
    let (dir, paths) = paths();
    let activity = Arc::new(activity(&d));
    Setup { d, activity, _dir: dir, paths, task: Arc::default(), status: Arc::new(Mutex::new(status(true, true, 0))), exit: Arc::default() }
}

impl Setup {
    /// The probation, from `started`, in its own task; it is on once this returns.
    async fn watch(&self, started: Instant) -> JoinHandle<()> {
        let (paths, activity, task, status, exit) =
            (self.paths.clone(), Arc::clone(&self.activity), Arc::clone(&self.task), Arc::clone(&self.status), Arc::clone(&self.exit));
        let watching = tokio::spawn(async move {
            let status = move || status.lock().unwrap().clone();
            probation::watch(&paths, &activity, &*task, status, started, &exit).await;
        });
        while !self.on() {
            tokio::task::yield_now().await;
        }
        watching
    }

    fn on(&self) -> bool {
        self.activity.pending.probation().get()
    }

    fn held(&self) -> bool {
        *self.activity.triggers.hold.borrow()
    }

    fn set_status(&self, value: Value) {
        *self.status.lock().unwrap() = value;
    }

    async fn write_refused(&self) -> bool {
        let sent = self.d.registry.call("send", json!({"account": "test-echo:a", "conversation": "c1", "text": "hi"})).await;
        is_update_in_progress(&sent)
    }

    fn helper_program(&self) -> (PathBuf, PathBuf) {
        (self.paths.bin().join("om.update.exe"), self.paths.data.clone())
    }
}

fn is_update_in_progress(result: &Result<Value, ToolError>) -> bool {
    matches!(result, Err(ToolError::Failed(why)) if why == "update in progress, try again shortly")
}

/// Whether `exit` was notified (its stored permit is taken).
async fn notified(exit: &Notify) -> bool {
    tokio::time::timeout(Duration::ZERO, exit.notified()).await.is_ok()
}

/// Waits for the probation's task, at most `within`.
async fn ended(watching: JoinHandle<()>, within: Duration) {
    tokio::time::timeout(within, watching).await.expect("the probation did not end").unwrap();
}

#[test]
fn daemon_exits_while_helper_changes_files() {
    let (_dir, paths) = paths();
    assert_eq!(probation::on_start(&paths), Start::Normal);
    let _helper = helper_alive(&paths);
    // No stage file: a helper with nothing to do; the daemon starts as usual.
    assert_eq!(probation::on_start(&paths), Start::Normal);
    for stage in [Stage::Started, Stage::BackedUp, Stage::RollingBack] {
        write_stage(&paths, stage);
        assert_eq!(probation::on_start(&paths), Start::Exit, "{stage:?}");
    }
    // Not a stage file: the helper knows what to do with it.
    std::fs::write(paths.pending_file(), "{\"version\": ").unwrap();
    assert_eq!(probation::on_start(&paths), Start::Exit);
    // The helper checks the new version: the daemon starts, in probation.
    write_stage(&paths, Stage::Installed);
    assert_eq!(probation::on_start(&paths), Start::Probation);
}

#[test]
fn stage_file_that_cannot_be_read_starts_held_unless_the_helper_is_alive() {
    let (_dir, paths) = paths();
    // A folder in its place fails to open on every try, like a file an antivirus holds. The helper
    // does nothing with it, so without one the daemon starts, held until the file reads again.
    std::fs::create_dir(paths.pending_file()).unwrap();
    assert_eq!(probation::on_start(&paths), Start::Probation);
    // A helper that is alive decides.
    let _helper = helper_alive(&paths);
    assert_eq!(probation::on_start(&paths), Start::Exit);
}

/// Started on a stage file it could not read: the daemon keeps everything held, lets go of
/// `update.lock` between its tries, and once the file reads again does what the stage asks.
#[tokio::test(start_paused = true)]
async fn probation_waits_for_a_stage_file_that_cannot_be_read() {
    for stage in [Stage::Installed, Stage::RollingBack] {
        let t = setup().await;
        std::fs::create_dir(t.paths.pending_file()).unwrap();
        let watching = t.watch(Instant::now()).await;
        sleep(5 * SECOND).await;
        assert!(!watching.is_finished());
        assert!(t.on() && t.held() && t.write_refused().await);
        assert!(!state::helper_alive(&t.paths), "update.lock held between tries");
        assert!(t.task.launched().is_empty());

        std::fs::remove_dir(t.paths.pending_file()).unwrap();
        let file = write_stage(&t.paths, stage);
        ended(watching, 2 * SECOND).await;
        if stage == Stage::Installed {
            // Healthy by its own status: installed.
            assert_eq!(stage_file(&t.paths), None);
            assert!(!t.on() && !notified(&t.exit).await);
        } else {
            assert_eq!(stage_file(&t.paths), Some(file));
            assert_eq!(t.task.launched(), [t.helper_program()]);
            assert!(t.on() && notified(&t.exit).await);
        }
    }
}

/// `om daemon` itself, with a helper restoring the files: it exits at once, before it opens the
/// state database or takes its lock, and says why.
#[cfg(windows)]
#[test]
fn om_daemon_exits_before_opening_anything_while_helper_restores() {
    use std::process::{Command, Stdio};

    let (dir, paths) = paths();
    let file = write_stage(&paths, Stage::RollingBack);
    let _helper = helper_alive(&paths);
    // Never the real Task Scheduler, even if this start went wrong: without USERNAME the daemon's
    // launcher fails before it writes or runs anything, and with SystemRoot here there is no
    // schtasks.exe to run. LOCALAPPDATA keeps the programs and updates folders in the temp dir.
    let no_system = dir.path().join("no-system");
    std::fs::create_dir_all(&no_system).unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_om"))
        .arg("daemon")
        .env("OPEN_MESS_DATA_DIR", &paths.data)
        .env("LOCALAPPDATA", dir.path())
        .env("SystemRoot", &no_system)
        .env_remove("USERNAME")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let exited = loop {
        if let Some(exited) = daemon.try_wait().unwrap() {
            break exited;
        }
        if std::time::Instant::now() > deadline {
            daemon.kill().unwrap();
            panic!("the daemon did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(exited.success(), "{exited:?}");
    assert!(!paths.data.join("state.sqlite3").exists());
    assert!(!paths.data.join("daemon.lock").exists());
    assert!(!paths.data.join("update-task.xml").exists(), "the helper's task was registered");
    assert_eq!(stage_file(&paths), Some(file));
    let why = std::fs::read_to_string(paths.data.join("last-start-error.txt")).unwrap();
    assert!(why.contains("being installed or rolled back"), "{why}");
}

#[tokio::test(start_paused = true)]
async fn daemon_launches_helper_when_it_died() {
    let (_dir, paths) = paths();
    for stage in [Stage::Started, Stage::BackedUp, Stage::Installed, Stage::RollingBack] {
        write_stage(&paths, stage);
        assert_eq!(probation::on_start(&paths), Start::LaunchHelperAndExit, "{stage:?}");
    }
    std::fs::write(paths.pending_file(), "{\"version\": ").unwrap();
    assert_eq!(probation::on_start(&paths), Start::LaunchHelperAndExit);
    let task = Task::default();
    probation::launch_helper(&paths, &task).unwrap();
    assert_eq!(task.launched(), [(paths.bin().join("om.update.exe"), paths.data.clone())]);

    // In probation the same: a helper that died while it changed the files is started again, and
    // the daemon exits so that it can work; the file stays as the helper left it.
    for stage in [Stage::Started, Stage::BackedUp, Stage::RollingBack] {
        let t = setup().await;
        let file = write_stage(&t.paths, stage);
        let watching = t.watch(Instant::now()).await;
        ended(watching, 2 * SECOND).await;
        assert_eq!(t.task.launched(), [t.helper_program()], "{stage:?}");
        assert!(notified(&t.exit).await);
        assert_eq!(stage_file(&t.paths), Some(file));
        assert!(!t.paths.last_result().exists());
        // Writes stay blocked until the daemon is gone.
        assert!(t.on() && t.held() && t.write_refused().await);
    }
}

/// A helper whose rollback could not stop the daemon leaves the stage at `rolling-back` and lets go
/// of `update.lock`. The daemon reads the file again then: it never judges itself on the stage it
/// saw while the helper was alive.
#[tokio::test(start_paused = true)]
async fn watch_reads_the_stage_again_once_the_helper_lets_go() {
    let t = setup().await;
    write_stage(&t.paths, Stage::Installed);
    let helper = helper_alive(&t.paths);
    let watching = t.watch(Instant::now()).await;
    sleep(5 * SECOND).await;
    assert!(!watching.is_finished());
    // The new version is healthy by its own status, but the helper decided otherwise.
    let file = write_stage(&t.paths, Stage::RollingBack);
    drop(helper);
    ended(watching, 2 * SECOND).await;
    assert_eq!(t.task.launched(), [t.helper_program()]);
    assert!(notified(&t.exit).await);
    assert_eq!(stage_file(&t.paths), Some(file));
    assert!(!t.paths.last_result().exists());
    assert!(!state::helper_alive(&t.paths));
}

/// A helper that is rolling back stops the daemon; the daemon exits by itself first.
#[tokio::test(start_paused = true)]
async fn daemon_exits_for_a_helper_rolling_back() {
    let t = setup().await;
    write_stage(&t.paths, Stage::Installed);
    let _helper = helper_alive(&t.paths);
    let watching = t.watch(Instant::now()).await;
    sleep(5 * SECOND).await;
    let file = write_stage(&t.paths, Stage::RollingBack);
    ended(watching, 2 * SECOND).await;
    // Alive: it needs no new start.
    assert!(t.task.launched().is_empty());
    assert!(notified(&t.exit).await);
    assert_eq!(stage_file(&t.paths), Some(file));
}

#[tokio::test]
async fn probation_blocks_writes_and_window_saves() {
    let d = common::daemon(&[common::ECHO]).await;
    d.accounts.add("test-echo", common::info("a")).unwrap();
    let ui = ui_state(&d);
    // Waits for the user's approval: the default write permission.
    let waiting = ui.pending.create(Kind::Send, "test-echo:a", "c1", json!({"conversation": "c1", "text": "hi"})).await.unwrap();
    assert_eq!(waiting.state, State::Waiting);
    let t = setup_with(d);
    write_stage(&t.paths, Stage::Installed);
    let helper = helper_alive(&t.paths);
    let watching = t.watch(Instant::now()).await;
    assert!(ui.probation.get());

    // The write tools are refused; the read tools work.
    assert!(t.write_refused().await);
    t.d.registry.call("status", json!({})).await.unwrap();
    // Every save in the window is refused, and the window reads as usual.
    let refused = |result: Result<(), String>| assert_eq!(result, Err(UPDATE_IN_PROGRESS.to_string()));
    refused(commands::remove_account(&ui, "test-echo:a").await);
    refused(commands::approve_module(&ui, "test-echo", "00", "00").await);
    refused(commands::set_module_enabled(&ui, "test-echo", false).await);
    refused(commands::rescan_modules(&ui).await);
    refused(commands::set_permissions(&ui, "test-echo:a", None, true, "autonomous"));
    refused(commands::clear_permission_override(&ui, "test-echo:a", "c1"));
    refused(commands::delete_rule(&ui, "r1"));
    refused(commands::put_rule(&ui, rule("r1")).map(drop));
    refused(commands::sign_in_start(&ui, "test-echo", None).await.map(drop));
    refused(commands::decide(&ui, &waiting.action, true).await.map(drop));
    refused(commands::decide(&ui, &waiting.action, false).await.map(drop));
    commands::status(&ui).unwrap();
    assert_eq!(commands::accounts(&ui).unwrap().len(), 1);
    assert_eq!(commands::modules(&ui).unwrap()[0].status, "active");
    commands::permissions(&ui, "test-echo:a").unwrap();
    commands::rules(&ui).unwrap();
    assert_eq!(commands::waiting(&ui).len(), 1);
    // A decision from the toast is refused too, and the action keeps waiting.
    let toast = ui.pending.decide(&waiting.action, true).await;
    assert!(matches!(toast, Err(open_mess::Error::UpdateInProgress)), "{toast:?}");
    assert_eq!(ui.pending.state(&waiting.action).unwrap(), State::Waiting);

    // The helper found the new version healthy: everything works again.
    std::fs::remove_file(t.paths.pending_file()).unwrap();
    drop(helper);
    ended(watching, 5 * SECOND).await;
    assert!(!ui.probation.get());
    assert!(!t.write_refused().await);
    commands::set_permissions(&ui, "test-echo:a", None, true, "confirm").unwrap();
    assert_eq!(commands::decide(&ui, &waiting.action, true).await.unwrap(), "done");
}

#[tokio::test(start_paused = true)]
async fn probation_holds_trigger_deliveries() {
    let t = setup().await;
    write_stage(&t.paths, Stage::Installed);
    let helper = helper_alive(&t.paths);
    // As the engine listens to it.
    let mut hold = t.activity.triggers.hold.subscribe();
    let watching = t.watch(Instant::now()).await;
    assert!(*hold.borrow_and_update());
    sleep(30 * SECOND).await;
    assert!(t.held());
    std::fs::remove_file(t.paths.pending_file()).unwrap();
    drop(helper);
    ended(watching, 2 * SECOND).await;
    // Released: the engine hands over what it collected at once.
    assert!(hold.has_changed().unwrap());
    assert!(!*hold.borrow_and_update());
}

#[tokio::test(start_paused = true)]
async fn probation_ends_when_helper_deletes_the_file() {
    let t = setup().await;
    let bin = t.paths.bin();
    for program in ["om.exe", "om.update.exe", "om.old-1700000000000.exe"] {
        std::fs::write(bin.join(program), b"MZ").unwrap();
    }
    let backup = t.paths.updates().join(VERSION).join("backup");
    std::fs::create_dir_all(&backup).unwrap();
    write_stage(&t.paths, Stage::Installed);
    let helper = helper_alive(&t.paths);
    // The helper does not judge from this: the daemon leaves the check to it while it is alive.
    t.set_status(status(true, true, 3));
    let watching = t.watch(Instant::now()).await;
    sleep(30 * SECOND).await;
    assert!(!watching.is_finished());
    assert!(t.on() && t.held() && t.write_refused().await);

    std::fs::remove_file(t.paths.pending_file()).unwrap();
    drop(helper);
    let deleted = Instant::now();
    ended(watching, 2 * SECOND).await;
    assert!(deleted.elapsed() <= SECOND, "{:?}", deleted.elapsed());
    assert!(!t.on() && !t.held() && !t.write_refused().await);
    assert!(!notified(&t.exit).await);
    assert!(t.task.launched().is_empty());
    // The helper writes the result, not the daemon.
    assert!(!t.paths.last_result().exists());
    // The cleanup ran: the current version's folder and the programs in use stay.
    assert!(!bin.join("om.old-1700000000000.exe").exists());
    assert!(bin.join("om.exe").exists() && bin.join("om.update.exe").exists());
    assert!(backup.is_dir());
}

#[tokio::test(start_paused = true)]
async fn daemon_finishes_check_when_helper_dies() {
    let t = setup().await;
    let update_dir = t.paths.updates().join(VERSION);
    std::fs::create_dir_all(update_dir.join("backup")).unwrap();
    write_stage(&t.paths, Stage::Installed);
    let helper = helper_alive(&t.paths);
    // While the helper is alive the daemon does not judge itself, whatever its status says.
    t.set_status(status(true, true, 1));
    let watching = t.watch(Instant::now()).await;
    sleep(10 * SECOND).await;
    assert!(!watching.is_finished() && t.task.launched().is_empty());

    t.set_status(status(true, true, 0));
    drop(helper);
    ended(watching, 2 * SECOND).await;
    assert_eq!(stage_file(&t.paths), None);
    assert_eq!(std::fs::read_to_string(t.paths.last_result()).unwrap(), "zainstalowano\n");
    assert!(!t.on() && !t.held() && !t.write_refused().await);
    assert!(!notified(&t.exit).await);
    assert!(t.task.launched().is_empty());
    assert!(!state::helper_alive(&t.paths));
    // As the helper does after a success: its backup is set aside, never restored by a later run.
    let set_aside: Vec<String> = std::fs::read_dir(&update_dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
    assert!(matches!(set_aside.as_slice(), [name] if name.starts_with("backup-")), "{set_aside:?}");
}

/// Its own check failed: the daemon starts the helper's task, which rolls back, and exits.
#[tokio::test(start_paused = true)]
async fn daemon_whose_own_check_fails_launches_the_helper_and_exits() {
    let t = setup().await;
    let file = write_stage(&t.paths, Stage::Installed);
    t.set_status(status(true, false, 1));
    let watching = t.watch(Instant::now()).await;
    ended(watching, 2 * SECOND).await;
    assert_eq!(t.task.launched(), [t.helper_program()]);
    assert!(notified(&t.exit).await);
    assert_eq!(stage_file(&t.paths), Some(file));
    assert!(!t.paths.last_result().exists());
    // Writes stay blocked until the daemon is gone; the helper can take its lock.
    assert!(t.on() && t.write_refused().await);
    assert!(!state::helper_alive(&t.paths));
}

/// Not ready 60 s after the start, or the accounts not restored after 10 minutes: the check fails.
/// Meanwhile the daemon holds `update.lock`, so a helper the task starts leaves the check to it.
#[tokio::test(start_paused = true)]
async fn daemon_check_gives_up_at_its_deadline() {
    for (status, deadline) in [(status(false, false, 0), 60 * SECOND), (status(true, false, 0), 600 * SECOND)] {
        let t = setup().await;
        write_stage(&t.paths, Stage::Installed);
        t.set_status(status);
        let started = Instant::now();
        let watching = t.watch(started).await;
        sleep(deadline - SECOND).await;
        assert!(!watching.is_finished() && t.task.launched().is_empty());
        assert!(state::helper_alive(&t.paths));
        ended(watching, 2 * SECOND).await;
        assert!((deadline..deadline + SECOND).contains(&started.elapsed()), "{:?}", started.elapsed());
        assert_eq!(t.task.launched(), [t.helper_program()]);
        assert!(notified(&t.exit).await);
    }
}

#[test]
fn cleanup_keeps_current_version_and_files_in_use() {
    let (_dir, paths) = paths();
    let folder = |version: &str, inside: &[&str]| {
        let dir = paths.updates().join(version);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("om.exe"), b"MZ").unwrap();
        for name in inside {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        dir
    };
    let older = folder("1.1.0", &[]);
    // Set aside after a successful install: it goes with its version.
    let set_aside = folder("1.0.5", &["backup-1700000000000"]);
    // A failed rollback keeps the user's data there.
    let failed_rollback = folder("1.0.0", &["backup"]);
    let current = folder("1.2.0", &["backup"]);
    let newer = folder("1.3.0", &[]);
    let not_a_version = folder("notes", &[]);
    let bin = paths.bin();
    let programs = ["om.exe", "om.update.exe", "om.old-1.exe", "om.failed-2.exe", "om.new.exe", "om.old.exe", "om.old-3.exe", "readme.txt"];
    for program in programs {
        std::fs::write(bin.join(program), b"MZ").unwrap();
    }
    // A bridge still runs this one.
    #[cfg(windows)]
    let in_use = {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new().read(true).share_mode(0).open(bin.join("om.old-3.exe")).unwrap()
    };

    probation::cleanup(&paths, &Version(1, 2, 0));
    assert!(!older.exists() && !set_aside.exists());
    assert!(failed_rollback.join("backup").is_dir() && current.join("backup").is_dir());
    assert!(newer.exists() && not_a_version.exists());
    let left = || -> Vec<&str> { programs.into_iter().filter(|program| bin.join(program).exists()).collect() };
    // Elsewhere a file in use can be deleted.
    let kept: &[&str] =
        if cfg!(windows) { &["om.exe", "om.update.exe", "om.old-3.exe", "readme.txt"] } else { &["om.exe", "om.update.exe", "readme.txt"] };
    assert_eq!(left(), kept);
    #[cfg(windows)]
    drop(in_use);
    // Gone the next time.
    probation::cleanup(&paths, &Version(1, 2, 0));
    assert_eq!(left(), ["om.exe", "om.update.exe", "readme.txt"]);
}

#[tokio::test]
async fn approving_a_side_by_side_module_deletes_the_predecessor() {
    let d = common::daemon(&[common::ECHO]).await;
    // Placed beside the running module by an update, its manifest changed: it waits for approval.
    // A rebuild of the running version waits too, and is not what the approval replaces.
    common::install(d.dir.path(), "echo-9.9.9.wasm", &common::echo_with_version("9.9.9"));
    common::install(d.dir.path(), "echo-rebuilt.wasm", &common::echo_rebuilt());
    let ui = ui_state(&d);
    commands::rescan_modules(&ui).await.unwrap();
    assert_eq!(approve(&ui, "echo-9.9.9.wasm").await, "replacement");
    let modules = d.dir.path().join("modules");
    assert!(!modules.join("echo.wasm").exists());
    assert!(modules.join("echo-rebuilt.wasm").exists());
    let list: Vec<(String, &str)> = commands::modules(&ui).unwrap().into_iter().map(|module| (module.file, module.status)).collect();
    assert_eq!(list, [("echo-9.9.9.wasm".to_string(), "active"), ("echo-rebuilt.wasm".to_string(), "replacement")]);
    assert_eq!(d.accounts.modules().handle("test-echo").unwrap().manifest().unwrap().version, "9.9.9");
}

/// Only a file an update placed beside an approved module, which is still the file it was, takes
/// that file's place on disk.
#[tokio::test]
async fn approving_a_hand_added_module_deletes_nothing() {
    let echo = |d: &common::Daemon| d.dir.path().join("modules").join("echo.wasm");

    // Added by hand beside the approved module: not an update's name.
    let d = common::daemon(&[common::ECHO]).await;
    common::install(d.dir.path(), "echo-dev.wasm", &common::echo_with_version("9.9.9"));
    let ui = ui_state(&d);
    commands::rescan_modules(&ui).await.unwrap();
    assert_eq!(approve(&ui, "echo-dev.wasm").await, "replacement");
    assert!(echo(&d).exists());
    assert_eq!(d.accounts.modules().handle("test-echo").unwrap().manifest().unwrap().version, "9.9.9");

    // An update's file beside a module file that is not approved: it changed while the module was
    // off.
    let d = common::daemon(&[common::ECHO]).await;
    let ui = ui_state(&d);
    commands::set_module_enabled(&ui, "test-echo", false).await.unwrap();
    common::install(d.dir.path(), "echo.wasm", &common::echo_rebuilt());
    common::install(d.dir.path(), "echo-9.9.9.wasm", &common::echo_with_version("9.9.9"));
    commands::rescan_modules(&ui).await.unwrap();
    let list = commands::modules(&ui).unwrap();
    assert_eq!(list.iter().find(|module| module.file == "echo.wasm").unwrap().status, "awaiting-approval");
    approve(&ui, "echo-9.9.9.wasm").await;
    assert!(echo(&d).exists());

    // An update's file beside the running module, whose file on disk changed meanwhile: what is
    // there now is not what was approved.
    let d = common::daemon(&[common::ECHO]).await;
    let ui = ui_state(&d);
    common::install(d.dir.path(), "echo-9.9.9.wasm", &common::echo_with_version("9.9.9"));
    commands::rescan_modules(&ui).await.unwrap();
    common::install(d.dir.path(), "echo.wasm", &common::echo_rebuilt());
    approve(&ui, "echo-9.9.9.wasm").await;
    assert_eq!(std::fs::read(echo(&d)).unwrap(), common::echo_rebuilt());
}

/// Approves `file` as the window shows it; returns its status before.
async fn approve(ui: &UiState, file: &str) -> &'static str {
    let list = commands::modules(ui).unwrap();
    let module = list.iter().find(|module| module.file == file).unwrap();
    commands::approve_module(ui, &module.name, &module.file_sha256, &module.manifest_sha256).await.unwrap();
    module.status
}

fn ui_state(d: &common::Daemon) -> UiState {
    let pending = Arc::clone(d.registry.pending().unwrap());
    UiState {
        accounts: Arc::clone(&d.accounts),
        probation: pending.probation().clone(),
        pending,
        started: std::time::Instant::now(),
        port: 7341,
        token: "t0ken".into(),
        sign_ins: d.sign_ins.clone(),
        counters: d.registry.trigger_counters().clone(),
        sessions: d.registry.channel_sessions().clone(),
        gate: d.registry.gate().clone(),
        activity: Arc::default(),
        runs: Runs::new(d.dir.path().join("runs"), RUN_TIMEOUT, Counters::default()),
        host_events: mpsc::channel(1).0,
        ui_ready: Arc::default(),
        exit: Arc::default(),
    }
}

fn rule(id: &str) -> RuleDto {
    RuleDto {
        id: id.into(),
        account: "test-echo:a".into(),
        enabled: true,
        rule: Rule::new(id, "test-echo:a", Condition::Mention, Action::Channel { fallback_spawn: None }),
    }
}
