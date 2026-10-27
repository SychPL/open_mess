//! The update helper `apply-update` (spec 2026-10-27-self-update, 7.2, 7.3, 7.6): the stage machine
//! in `update-pending.json`, over temp data and program folders with a fake Windows (no scheduled
//! task, no daemon) and test-echo built with other manifests.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::time::Duration;

use open_mess::modules::{self, ModuleInfo, ModuleState};
use open_mess::store::Store;
use open_mess::update::UpdateError;
use open_mess::update::apply::{self, Health, Judgement, Outcome, System, judge};
use open_mess::update::consent;
use open_mess::update::release::manifest_sha256;
use open_mess::update::state::{self, Paths, PendingFile, Read, Stage};
use open_mess_host::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// The release under test has the helper's own version: an unreadable stage file is matched to its
/// backup by it.
const RELEASE: &str = env!("CARGO_PKG_VERSION");
/// The wall clock of every run; the consent was given a second before.
const NOW: i64 = 1_000_000_000;
const OLD_OM: &[u8] = b"om.exe as installed";
const NEW_OM: &[u8] = b"om.exe of the release";

/// Windows, faked: the daemon runs or not, and starts and stops on request.
struct Machine {
    running: Cell<bool>,
    health: Health,
    /// Runs when the new daemon's health is asked: what that daemon does meanwhile.
    meanwhile: Box<dyn Fn()>,
    /// A stop panics: the helper dies in the middle of a rollback.
    crash_on_stop: bool,
    /// A stop leaves the daemon running: something keeps holding its lock.
    stop_fails: bool,
    log: RefCell<Vec<&'static str>>,
}

impl Machine {
    fn new(running: bool, health: Health) -> Machine {
        Machine { running: Cell::new(running), health, meanwhile: Box::new(|| {}), crash_on_stop: false, stop_fails: false, log: RefCell::default() }
    }

    /// A machine on which the helper dies while the new daemon's health is asked: after the
    /// installation, at stage `installed`.
    fn crashing() -> Machine {
        Machine { meanwhile: Box::new(|| panic!("power loss")), ..Machine::new(false, Health::Healthy) }
    }

    fn log(&self) -> Vec<&'static str> {
        self.log.borrow().clone()
    }
}

impl System for Machine {
    fn wait_daemon_lock_free(&self, _wait: Duration) -> bool {
        self.log.borrow_mut().push("wait");
        !self.running.get()
    }

    fn stop_daemon(&self) -> Result<(), UpdateError> {
        self.log.borrow_mut().push("stop");
        assert!(!self.crash_on_stop, "the helper died");
        self.running.set(self.running.get() && self.stop_fails);
        Ok(())
    }

    fn start_daemon(&self) -> Result<(), UpdateError> {
        self.log.borrow_mut().push("start");
        self.running.set(true);
        Ok(())
    }

    fn health(&self, version: &str, active_modules: &[String]) -> Health {
        self.log.borrow_mut().push("health");
        assert_eq!((version, active_modules), (RELEASE, &["test-echo".to_string()][..]));
        (self.meanwhile)();
        self.health.clone()
    }
}

/// An installed open_mess with a downloaded release, a consent for it and the daemon's hand-over.
struct Setup {
    _root: TempDir,
    paths: Paths,
    public_key: String,
    secret_key: minisign::SecretKey,
    update_dir: PathBuf,
    manifest_sha256: String,
}

impl Setup {
    /// `installed` module files (all approved) and the release's module files.
    fn new(installed: &[(&str, Vec<u8>)], release: &[(&str, Vec<u8>)]) -> Setup {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths { data: root.path().join("data"), local: root.path().join("local") };
        std::fs::create_dir_all(&paths.data).unwrap();
        let store = common::store(&paths.data);
        for (file, bytes) in installed {
            common::install(&paths.data, file, bytes);
        }
        block_on(async {
            let engine = Engine::new().unwrap();
            common::approve_all(&store, &common::scan(&store, &engine, &paths.data).await);
        });
        std::fs::create_dir_all(paths.bin()).unwrap();
        std::fs::write(paths.bin().join("om.exe"), OLD_OM).unwrap();

        let update_dir = paths.updates().join(RELEASE);
        std::fs::create_dir_all(&update_dir).unwrap();
        let mut files = vec![("om.exe", "exe", NEW_OM.to_vec())];
        files.extend(release.iter().map(|(name, bytes)| (*name, "module", bytes.clone())));
        let entries: Vec<String> = files
            .iter()
            .map(|(name, kind, bytes)| {
                std::fs::write(update_dir.join(name), bytes).unwrap();
                format!(r#"{{"name":"{name}","kind":"{kind}","size":{},"sha256":"{}"}}"#, bytes.len(), sha(bytes))
            })
            .collect();
        let manifest = format!(r#"{{"version":"{RELEASE}","files":[{}]}}"#, entries.join(",")).into_bytes();
        let keys = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let signature = minisign::sign(None, &keys.sk, Cursor::new(&manifest), None, None).unwrap().to_string();
        std::fs::write(update_dir.join("manifest.json"), &manifest).unwrap();
        std::fs::write(update_dir.join("manifest.json.minisig"), signature).unwrap();
        let manifest_sha256 = manifest_sha256(&manifest);

        consent::record_click(store.conn(), RELEASE, &manifest_sha256, NOW - 2_000).unwrap();
        consent::mark_launched(store.conn(), RELEASE, &manifest_sha256, NOW - 1_000).unwrap();
        let pending = PendingFile {
            version: RELEASE.into(),
            manifest_sha256: manifest_sha256.clone(),
            update_dir: update_dir.clone(),
            stage: Stage::Started,
            active_modules: vec!["test-echo".into()],
            added: Vec::new(),
            replaced: Vec::new(),
        };
        state::write(&paths.pending_file(), &pending).unwrap();
        Setup { _root: root, paths, public_key: keys.pk.to_box().unwrap().to_string(), secret_key: keys.sk, update_dir, manifest_sha256 }
    }

    fn run(&self, machine: &Machine) -> Outcome {
        apply::run(&self.paths, machine, &self.public_key, || NOW)
    }

    /// A run during which the helper dies.
    fn crash(&self, machine: &Machine) {
        assert!(catch_unwind(AssertUnwindSafe(|| self.run(machine))).is_err());
    }

    fn module(&self, file: &str) -> PathBuf {
        self.paths.data.join("modules").join(file)
    }

    fn om(&self) -> Vec<u8> {
        std::fs::read(self.paths.bin().join("om.exe")).unwrap()
    }

    fn pending(&self) -> PendingFile {
        match state::read(&self.paths.pending_file()) {
            Read::Found(pending) => pending,
            other => panic!("no stage file: {other:?}"),
        }
    }

    fn set_stage(&self, stage: Stage) {
        state::write(&self.paths.pending_file(), &PendingFile { stage, ..self.pending() }).unwrap();
    }

    fn last_result(&self) -> Option<String> {
        std::fs::read_to_string(self.paths.last_result()).ok()
    }

    fn store(&self) -> Store {
        common::store(&self.paths.data)
    }

    fn consent_valid(&self) -> bool {
        consent::valid(self.store().conn(), RELEASE, &self.manifest_sha256, NOW).unwrap()
    }

    fn rolled_back(&self) -> bool {
        consent::rolled_back(self.store().conn(), RELEASE, &self.manifest_sha256).unwrap()
    }

    /// The modules folder as the next daemon finds it.
    fn modules(&self) -> Vec<ModuleInfo> {
        block_on(async { common::scan(&self.store(), &Engine::new().unwrap(), &self.paths.data).await.list() })
    }

    fn backup(&self) -> PathBuf {
        self.update_dir.join("backup")
    }

    /// The backup of a successful installation, kept for recovery by hand.
    fn kept_backup(&self) -> PathBuf {
        self.update_dir.join(format!("backup-{NOW}"))
    }

    /// Nothing changed but the stage file and the consent.
    fn assert_untouched(&self) {
        assert_eq!(self.om(), OLD_OM);
        assert_eq!(std::fs::read(self.module("echo.wasm")).unwrap(), common::echo());
        assert!(!self.backup().exists());
        assert_eq!(std::fs::read_dir(self.paths.bin()).unwrap().count(), 1, "only om.exe in bin");
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn echo_installed() -> Vec<(&'static str, Vec<u8>)> {
    vec![("echo.wasm", common::echo())]
}

fn state_of(modules: &[ModuleInfo], file: &str) -> ModuleState {
    modules.iter().find(|info| info.file == file).unwrap_or_else(|| panic!("no {file}")).state.clone()
}

#[test]
fn no_pending_file_touches_nothing() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    std::fs::remove_file(s.paths.pending_file()).unwrap();
    let machine = Machine::new(true, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::Nothing);
    assert!(machine.log().is_empty());
    assert_eq!(s.last_result(), None);
    assert!(s.consent_valid());
    s.assert_untouched();

    // A stage file that can't be read now (an antivirus holding it, say) is left for the next run.
    std::fs::create_dir(s.paths.pending_file()).unwrap();
    assert_eq!(s.run(&machine), Outcome::Nothing);
    assert!(s.paths.pending_file().is_dir());
    assert!(machine.log().is_empty());
    assert_eq!(s.last_result(), None);
}

#[test]
fn started_without_consent_aborts_and_starts_daemon() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    // Clicked, but the daemon never launched the helper for it: say, an agent ran `om apply-update`.
    consent::record_click(s.store().conn(), RELEASE, &s.manifest_sha256, NOW).unwrap();
    let machine = Machine::new(false, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::Aborted("brak ważnej zgody".into()));
    assert_eq!(machine.log(), ["wait", "start"]);
    assert!(!s.paths.pending_file().exists());
    assert_eq!(s.last_result().as_deref(), Some("przerwano: brak ważnej zgody\n"));
    s.assert_untouched();

    // A daemon that runs is neither stopped nor started.
    state::write(&s.paths.pending_file(), &PendingFile { stage: Stage::Started, ..pending_of(&s) }).unwrap();
    let machine = Machine::new(true, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::Aborted("brak ważnej zgody".into()));
    assert_eq!(machine.log(), ["wait"]);
    s.assert_untouched();
}

/// The stage file the daemon writes at hand-over.
fn pending_of(s: &Setup) -> PendingFile {
    PendingFile {
        version: RELEASE.into(),
        manifest_sha256: s.manifest_sha256.clone(),
        update_dir: s.update_dir.clone(),
        stage: Stage::Started,
        active_modules: vec!["test-echo".into()],
        added: Vec::new(),
        replaced: Vec::new(),
    }
}

#[test]
fn second_helper_exits_while_lock_is_held() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    let _first = state::try_lock(&s.paths.lock_file(), Duration::ZERO).unwrap().unwrap();
    let machine = Machine::new(true, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::Nothing);
    assert!(machine.log().is_empty());
    assert_eq!(s.pending(), pending_of(&s));
    assert_eq!(s.last_result(), None);
    assert!(s.consent_valid());
    s.assert_untouched();
}

#[test]
fn install_swaps_exe_and_approves_version_only_module() {
    let new_echo = common::echo_with_version("9.9.9");
    let s = Setup::new(&echo_installed(), &[("echo.wasm", new_echo.clone())]);
    let machine = Machine::new(false, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::Installed);
    assert_eq!(machine.log(), ["wait", "start", "health"]);
    assert!(!s.paths.pending_file().exists());
    assert_eq!(s.last_result().as_deref(), Some("zainstalowano\n"));
    assert!(!s.consent_valid(), "the consent is spent");

    assert_eq!(s.om(), NEW_OM);
    assert_eq!(std::fs::read(s.paths.bin().join(format!("om.old-{NOW}.exe"))).unwrap(), OLD_OM);
    assert!(!s.paths.bin().join("om.new.exe").exists());
    assert_eq!(std::fs::read(s.module("echo.wasm")).unwrap(), new_echo);
    assert!(!s.module("echo.wasm.new").exists());
    // Approved as if the user had clicked: the next daemon runs it.
    let modules = s.modules();
    assert_eq!(modules.len(), 1);
    assert_eq!((modules[0].state.clone(), modules[0].version.as_str()), (ModuleState::Active, "9.9.9"));
    assert_eq!(modules::approved_manifest(&s.store(), "test-echo").unwrap().unwrap()["version"], "9.9.9");
    // The backup stays until the next update (spec 7.5), under a name no rollback looks for.
    assert!(!s.backup().exists());
    assert_eq!(std::fs::read(s.kept_backup().join("bin").join("om.exe")).unwrap(), OLD_OM);
    assert_eq!(std::fs::read(s.kept_backup().join("data").join("modules").join("echo.wasm")).unwrap(), common::echo());
}

#[test]
fn earlier_backup_is_moved_aside() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    // Left by an earlier attempt whose rollback failed: maybe the only copy of the user's data.
    std::fs::create_dir_all(s.backup()).unwrap();
    std::fs::write(s.backup().join("state.sqlite3"), b"an earlier attempt's").unwrap();
    assert_eq!(s.run(&Machine::new(false, Health::Healthy)), Outcome::Installed);
    let earlier = s.update_dir.join(format!("backup-{NOW}-earlier"));
    assert_eq!(std::fs::read(earlier.join("state.sqlite3")).unwrap(), b"an earlier attempt's");
    // Never mixed into this installation's backup.
    assert!(!s.kept_backup().join("state.sqlite3").exists());
    assert_eq!(std::fs::read(s.kept_backup().join("bin").join("om.exe")).unwrap(), OLD_OM);
}

#[test]
fn changed_manifest_goes_alongside_and_waits() {
    let changed = common::echo_with_another_host();
    let s = Setup::new(&echo_installed(), &[("echo.wasm", changed.clone())]);
    let machine = Machine::new(true, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::Installed);
    assert_eq!(machine.log(), ["wait", "stop", "wait", "start", "health"]);
    assert_eq!(std::fs::read(s.module("echo.wasm")).unwrap(), common::echo());
    assert_eq!(std::fs::read(s.module(&format!("echo-{RELEASE}.wasm"))).unwrap(), changed);
    let modules = s.modules();
    assert_eq!(state_of(&modules, "echo.wasm"), ModuleState::Active);
    assert_eq!(state_of(&modules, &format!("echo-{RELEASE}.wasm")), ModuleState::PendingReplacement);
}

#[test]
fn occupied_name_gets_a_numbered_file() {
    let changed = common::echo_with_another_host();
    let spin = std::fs::read(common::wasm("open_mess_test_spin")).unwrap();
    // Not modules: a name is taken by any file.
    let taken = format!("echo-{RELEASE}.wasm");
    let installed = [("echo.wasm", common::echo()), (taken.as_str(), b"someone else's".to_vec()), ("spin.wasm", b"not test-spin".to_vec())];
    let s = Setup::new(&installed, &[("echo.wasm", changed.clone()), ("spin.wasm", spin.clone())]);
    assert_eq!(s.run(&Machine::new(false, Health::Healthy)), Outcome::Installed);
    assert_eq!(std::fs::read(s.module("echo.wasm")).unwrap(), common::echo());
    assert_eq!(std::fs::read(s.module(&format!("echo-{RELEASE}.wasm"))).unwrap(), b"someone else's");
    assert_eq!(std::fs::read(s.module(&format!("echo-{RELEASE}-1.wasm"))).unwrap(), changed);
    // A new module without a predecessor takes its own name, or a numbered one.
    assert_eq!(std::fs::read(s.module("spin.wasm")).unwrap(), b"not test-spin");
    assert_eq!(std::fs::read(s.module(&format!("spin-{RELEASE}-1.wasm"))).unwrap(), spin);
}

#[test]
fn failed_health_rolls_back_everything() {
    let spin = std::fs::read(common::wasm("open_mess_test_spin")).unwrap();
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9")), ("spin.wasm", spin)]);
    let mut machine = Machine::new(false, Health::Failed("module test-echo crashed".into()));
    // The new daemon migrates and writes its database.
    let data = s.paths.data.clone();
    machine.meanwhile = Box::new(move || common::store(&data).set_config("written_by", "the new daemon").unwrap());
    assert_eq!(s.run(&machine), Outcome::RolledBack("module test-echo crashed".into()));
    assert_eq!(machine.log(), ["wait", "start", "health", "stop", "wait", "start"]);
    assert_eq!(s.last_result().as_deref(), Some("przywrócono: module test-echo crashed\n"));
    assert!(!s.paths.pending_file().exists());

    assert_eq!(s.om(), OLD_OM);
    assert!(!s.paths.bin().join("om.new.exe").exists());
    assert_eq!(std::fs::read(s.module("echo.wasm")).unwrap(), common::echo());
    assert!(!s.module("spin.wasm").exists(), "added by the update");
    assert!(!s.module("echo.wasm.new").exists());
    assert_eq!(s.store().get_config("written_by").unwrap(), None);
    let modules = s.modules();
    assert_eq!(modules.len(), 1);
    assert_eq!((modules[0].state.clone(), modules[0].version.as_str()), (ModuleState::Active, RELEASE));
    assert!(s.rolled_back(), "not offered again unchanged");
    assert!(!s.backup().exists());
}

#[test]
fn crash_after_backup_rolls_back_on_next_run() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    s.crash(&Machine::crashing());
    // Whatever the helper had done after the backup, the stage says what is left to undo.
    s.set_stage(Stage::BackedUp);

    let machine = Machine::new(true, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::RolledBack("the update was interrupted".into()));
    assert_eq!(machine.log(), ["stop", "wait", "start"]);
    assert_eq!(s.om(), OLD_OM);
    assert_eq!(std::fs::read(s.module("echo.wasm")).unwrap(), common::echo());
    assert_eq!(s.modules()[0].version, RELEASE);
    assert!(!s.paths.pending_file().exists());
    assert!(s.rolled_back());
}

#[test]
fn interrupted_rollback_finishes_on_next_run() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    let mut machine = Machine::new(false, Health::Failed("not ready within 60 s".into()));
    machine.crash_on_stop = true;
    s.crash(&machine);
    assert_eq!(s.pending().stage, Stage::RollingBack);
    assert_eq!(s.om(), NEW_OM);

    let machine = Machine::new(false, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::RolledBack("the update was interrupted".into()));
    assert_eq!(s.om(), OLD_OM);
    assert_eq!(std::fs::read(s.module("echo.wasm")).unwrap(), common::echo());
    assert_eq!(s.modules()[0].version, RELEASE);
    assert!(!s.paths.pending_file().exists());
    assert_eq!(machine.log(), ["stop", "wait", "start"]);
}

#[test]
fn rollback_waits_for_the_daemon_to_let_go() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    s.crash(&Machine::crashing());
    // The new daemon holds its lock, and its files, through two stops.
    let machine = Machine { stop_fails: true, ..Machine::new(true, Health::Healthy) };
    let Outcome::RollbackFailed(why) = s.run(&machine) else { panic!("rolled back under a running daemon") };
    assert!(why.contains("tried again"), "{why}");
    assert_eq!(machine.log(), ["stop", "wait", "stop", "wait"], "nothing restored, nothing started");
    assert_eq!(s.pending().stage, Stage::RollingBack);
    assert_eq!(s.om(), NEW_OM);
    assert_eq!(std::fs::read(s.module("echo.wasm")).unwrap(), common::echo_with_version("9.9.9"));

    let machine = Machine::new(true, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::RolledBack("the update was interrupted".into()));
    assert_eq!(s.om(), OLD_OM);
    assert_eq!(std::fs::read(s.module("echo.wasm")).unwrap(), common::echo());
}

#[test]
fn failed_rollback_keeps_and_names_the_backup() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    s.crash(&Machine::crashing());
    std::fs::remove_file(s.backup().join("bin").join("om.exe")).unwrap();
    let machine = Machine::new(false, Health::Healthy);
    let Outcome::RollbackFailed(why) = s.run(&machine) else { panic!("rolled back without om.exe in the backup") };
    assert!(why.contains(&format!("kopia zapasowa: {}", s.backup().display())), "{why}");
    assert_eq!(machine.log(), ["stop", "wait", "start"], "started all the same");
    assert!(!s.paths.pending_file().exists(), "not tried again at every logon");
    assert!(s.backup().join("data").join("state.sqlite3").exists(), "the backup is kept");
    assert!(s.last_result().unwrap().starts_with("wycofanie nieudane: "));
    // The rest is restored all the same.
    assert_eq!(std::fs::read(s.module("echo.wasm")).unwrap(), common::echo());
    assert!(!s.rolled_back());
}

#[test]
fn unreadable_stage_file_with_a_backup_rolls_back() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    s.crash(&Machine::crashing());
    std::fs::write(s.paths.pending_file(), "{\"version\": ").unwrap();
    let machine = Machine::new(false, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::RolledBack("update-pending.json is unreadable".into()));
    assert_eq!(machine.log(), ["stop", "wait", "start"]);
    assert_eq!(s.om(), OLD_OM);
    assert_eq!(std::fs::read(s.module("echo.wasm")).unwrap(), common::echo());
    assert_eq!(s.modules()[0].version, RELEASE);
    assert!(!s.paths.pending_file().exists());
    assert!(!s.backup().exists());
}

#[cfg(windows)]
#[test]
fn busy_om_exe_is_moved_aside() {
    use std::process::{Command, Stdio};

    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    s.crash(&Machine::crashing());
    // A bridge runs the installed om.exe: Windows lets it be renamed, not replaced.
    let om = s.paths.bin().join("om.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_open-mess-record-run"), &om).unwrap();
    let started = s.paths.data.join("bridge-started");
    let mut bridge = Command::new(&om).arg(&started).args(["--sleep", "60000"]).stdin(Stdio::null()).spawn().unwrap();
    let wait = std::time::Instant::now();
    while !started.exists() && wait.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let outcome = s.run(&Machine::new(false, Health::Healthy));
    bridge.kill().unwrap();
    bridge.wait().unwrap();
    assert!(matches!(outcome, Outcome::RolledBack(_)), "{outcome:?}");
    assert_eq!(s.om(), OLD_OM);
    assert_eq!(
        std::fs::read(s.paths.bin().join(format!("om.failed-{NOW}.exe"))).unwrap(),
        std::fs::read(env!("CARGO_BIN_EXE_open-mess-record-run")).unwrap()
    );
}

#[test]
fn missing_om_exe_after_crash_is_restored() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    s.crash(&Machine::crashing());
    // Power lost between the two renames: om.exe moved aside, om.new.exe not in its place yet.
    std::fs::rename(s.paths.bin().join("om.exe"), s.paths.bin().join("om.new.exe")).unwrap();
    s.set_stage(Stage::BackedUp);

    // The daemon's task can't start without om.exe.
    let machine = Machine::new(false, Health::Healthy);
    assert!(matches!(s.run(&machine), Outcome::RolledBack(_)));
    assert_eq!(s.om(), OLD_OM);
    assert!(!s.paths.bin().join("om.new.exe").exists());
    assert_eq!(machine.log(), ["stop", "wait", "start"]);
}

#[test]
fn restore_puts_back_journal_and_removes_new_ones() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    // The old daemon was killed in the middle of a write in the module's storage.
    std::fs::write(s.module("test-echo.sqlite3"), b"module database").unwrap();
    std::fs::write(s.module("test-echo.sqlite3-journal"), b"hot journal").unwrap();
    let mut machine = Machine::new(false, Health::Failed("module test-echo crashed".into()));
    let (modules, data) = (s.paths.data.join("modules"), s.paths.data.clone());
    machine.meanwhile = Box::new(move || {
        std::fs::write(modules.join("test-echo.sqlite3"), b"new database").unwrap();
        std::fs::remove_file(modules.join("test-echo.sqlite3-journal")).unwrap();
        for journal in ["test-echo.sqlite3-wal", "test-echo.sqlite3-shm"] {
            std::fs::write(modules.join(journal), b"new journal").unwrap();
        }
        for journal in ["state.sqlite3-wal", "state.sqlite3-shm"] {
            std::fs::write(data.join(journal), b"new journal").unwrap();
        }
    });
    assert!(matches!(s.run(&machine), Outcome::RolledBack(_)));
    assert_eq!(std::fs::read(s.module("test-echo.sqlite3")).unwrap(), b"module database");
    assert_eq!(std::fs::read(s.module("test-echo.sqlite3-journal")).unwrap(), b"hot journal");
    for journal in ["test-echo.sqlite3-wal", "test-echo.sqlite3-shm"] {
        assert!(!s.module(journal).exists(), "{journal} is left");
    }
    for journal in ["state.sqlite3-wal", "state.sqlite3-shm"] {
        assert!(!s.paths.data.join(journal).exists(), "{journal} is left");
    }
    assert!(s.rolled_back(), "the restored state database opens");
}

#[test]
fn tampered_file_after_download_is_caught_by_reverify() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    std::fs::write(s.update_dir.join("echo.wasm"), common::echo_with_version("6.6.6")).unwrap();
    let machine = Machine::new(true, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::Aborted("release rejected: echo.wasm does not match the release manifest".into()));
    // Stopped before the check, and started again.
    assert_eq!(machine.log(), ["wait", "stop", "wait", "wait", "start"]);
    assert!(!s.paths.pending_file().exists());
    assert!(!s.consent_valid(), "the consent is spent before the check");
    s.assert_untouched();
}

#[test]
fn tampered_signature_or_manifest_is_caught_by_reverify() {
    // Signed by someone else.
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    let other = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let manifest = std::fs::read(s.update_dir.join("manifest.json")).unwrap();
    let forged = minisign::sign(None, &other.sk, Cursor::new(&manifest), None, None).unwrap().to_string();
    std::fs::write(s.update_dir.join("manifest.json.minisig"), forged).unwrap();
    let machine = Machine::new(false, Health::Healthy);
    let Outcome::Aborted(why) = s.run(&machine) else { panic!("installed a forged signature") };
    assert!(why.starts_with("release rejected: signature"), "{why}");
    assert_eq!(machine.log(), ["wait", "wait", "start"]);
    s.assert_untouched();

    // Signed by the release key, but not the release the user accepted.
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    let other_release =
        format!(r#"{{"version":"{RELEASE}","files":[{{"name":"om.exe","kind":"exe","size":{},"sha256":"{}"}}]}}"#, NEW_OM.len(), sha(NEW_OM));
    let signature = minisign::sign(None, &s.secret_key, Cursor::new(other_release.as_bytes()), None, None).unwrap().to_string();
    std::fs::write(s.update_dir.join("manifest.json"), &other_release).unwrap();
    std::fs::write(s.update_dir.join("manifest.json.minisig"), signature).unwrap();
    let machine = Machine::new(false, Health::Healthy);
    assert_eq!(s.run(&machine), Outcome::Aborted("release rejected: the update folder holds another release than the one accepted".into()));
    s.assert_untouched();
}

#[test]
fn daemon_that_does_not_stop_changes_nothing_and_is_started() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    let machine = Machine { stop_fails: true, ..Machine::new(true, Health::Healthy) };
    assert_eq!(s.run(&machine), Outcome::Aborted("the daemon did not stop".into()));
    // Never left stopped: a start is harmless to a daemon that runs.
    assert_eq!(machine.log(), ["wait", "stop", "wait", "start"]);
    assert!(!s.paths.pending_file().exists());
    assert!(s.consent_valid(), "nothing was tried, the consent is not spent");
    s.assert_untouched();
}

#[test]
fn stage_file_naming_other_files_is_unreadable() {
    let s = Setup::new(&echo_installed(), &[("echo.wasm", common::echo_with_version("9.9.9"))]);
    let victim = s.paths.data.join("state.sqlite3");
    // A file outside the folders an installation adds to, or a backup outside the update folders.
    let elsewhere = s.paths.data.join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("backup").join("bin")).unwrap();
    std::fs::write(elsewhere.join("backup").join("bin").join("om.exe"), b"planted").unwrap();
    for tampered in [
        PendingFile { stage: Stage::RollingBack, added: vec![victim.clone()], ..pending_of(&s) },
        PendingFile { stage: Stage::RollingBack, replaced: vec![s.paths.data.join("modules").join("..").join("state.sqlite3")], ..pending_of(&s) },
        PendingFile { stage: Stage::RollingBack, update_dir: elsewhere.clone(), ..pending_of(&s) },
        PendingFile { stage: Stage::RollingBack, version: "../9.9.9".into(), ..pending_of(&s) },
    ] {
        state::write(&s.paths.pending_file(), &tampered).unwrap();
        let machine = Machine::new(false, Health::Healthy);
        // No backup of this version: deleted, and the daemon started.
        assert_eq!(s.run(&machine), Outcome::Aborted("update-pending.json is unreadable".into()), "{tampered:?}");
        assert_eq!(machine.log(), ["wait", "start"]);
        assert!(victim.exists());
        assert!(elsewhere.join("backup").exists());
        assert!(!s.paths.pending_file().exists());
        s.assert_untouched();
    }
}

#[test]
fn judge_waits_for_accounts_but_fails_on_a_crash() {
    let active = ["test-echo".to_string()];
    let status = |ui_ready: bool, accounts_restored: bool, modules: serde_json::Value| json!({"version": "9.9.9", "modules": modules, "ui_ready": ui_ready, "accounts_restored": accounts_restored});
    let echo = |state: &str, crashes: serde_json::Value| json!([{"name": "test-echo", "state": state, "crashes_since_load": crashes}]);
    let back = || echo("active", json!(0));
    let judge = |status: &serde_json::Value, active: &[String]| judge(status, "9.9.9", active);

    assert_eq!(judge(&json!({}), &active), Judgement::Pending { accounts_only: false });
    assert_eq!(judge(&status(false, true, back()), &active), Judgement::Pending { accounts_only: false });
    assert_eq!(judge(&status(true, true, json!([])), &active), Judgement::Pending { accounts_only: false }, "not loaded yet");
    assert_eq!(judge(&status(true, true, echo("awaiting approval (new module)", json!(null))), &active), Judgement::Pending { accounts_only: false });
    assert_eq!(judge(&status(true, false, back()), &active), Judgement::Pending { accounts_only: true });
    assert_eq!(judge(&status(true, true, back()), &active), Judgement::Healthy);
    // Another version answering is not the new daemon, however well it is.
    let mut old = status(true, true, back());
    old["version"] = json!("9.9.8");
    assert_eq!(judge(&old, &active), Judgement::Pending { accounts_only: false });

    // A crash fails at once, even while the accounts are restored.
    assert!(matches!(judge(&status(true, false, echo("active", json!(1))), &active), Judgement::Failed(_)));
    assert!(matches!(judge(&status(false, false, echo("stopped after repeated crashes", json!(null))), &active), Judgement::Failed(_)));
    assert!(matches!(judge(&status(true, false, echo("disabled", json!(null))), &active), Judgement::Failed(_)));

    // A file waiting to replace the module is not the module; a module not active before is not judged.
    let both = json!([
        {"name": "test-echo", "state": "pending replacement", "crashes_since_load": null},
        {"name": "test-echo", "state": "active", "crashes_since_load": 0},
        {"name": "other", "state": "stopped after repeated crashes", "crashes_since_load": null},
    ]);
    assert_eq!(judge(&status(true, true, both), &active), Judgement::Healthy);
}
