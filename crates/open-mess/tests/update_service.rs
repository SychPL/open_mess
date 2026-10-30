//! The daemon's update service (spec 2026-10-27-self-update, 5-7): when it checks, what it installs
//! on its own and what it refuses, against a fake release server and a fake helper task. The clock
//! is tokio's paused one; while the fake server answers, the tests spin on `yield_now`, which keeps
//! the paused clock still, so no network timeout fires early.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use open_mess::i18n::Lang;
use open_mess::store::Store;
use open_mess::tools::ToolError;
use open_mess::triggers::spawn::Runs;
use open_mess::triggers::{self, Counters};
use open_mess::ui::{UiState, commands};
use open_mess::update::UpdateError;
use open_mess::update::consent;
use open_mess::update::fetch::Source;
use open_mess::update::quiesce::{Activity, Launcher};
use open_mess::update::release::manifest_sha256;
use open_mess::update::service::{self, Announce, Found, NotInstallable, Phase, Setup, UpdateService, UpdateState};
use open_mess::update::state::{self, Paths, Read, Stage};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};
use tokio::time::{sleep, timeout};

const OM: &[u8] = b"fake om.exe of release 9.9.9";
const MINUTE: Duration = Duration::from_secs(60);

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A signed release 9.9.9 on a loopback server, and how often its "latest release" was asked for.
struct Release {
    source: Source,
    key: String,
    manifest_sha256: String,
    checks: Arc<AtomicUsize>,
    downloads: Arc<AtomicUsize>,
}

async fn release() -> Release {
    let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let manifest =
        format!(r#"{{"version":"9.9.9","files":[{{"name":"om.exe","kind":"exe","size":{},"sha256":"{}"}}]}}"#, OM.len(), sha(OM)).into_bytes();
    let signature = minisign::sign(None, &pair.sk, Cursor::new(&manifest), None, None).unwrap().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let asset = |name: &str| json!({"name": name, "browser_download_url": format!("http://{addr}/{name}")});
    let latest = json!({"tag_name": "v9.9.9", "assets": [asset("manifest.json"), asset("manifest.json.minisig"), asset("om.exe")]});
    let (checks, downloads) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let counted = |counter: &Arc<AtomicUsize>| {
        let counter = Arc::clone(counter);
        move || counter.fetch_add(1, Ordering::SeqCst)
    };
    let (asked, fetched) = (counted(&checks), counted(&downloads));
    let manifest_sha = manifest_sha256(&manifest);
    let app = Router::new()
        .route(
            "/latest",
            get(move || async move {
                asked();
                axum::Json(latest)
            }),
        )
        .route(
            "/manifest.json",
            get(move || async move {
                fetched();
                manifest
            }),
        )
        .route("/manifest.json.minisig", get(move || async move { signature }))
        .route("/om.exe", get(|| async { OM }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let source = Source { latest: format!("http://{addr}/latest").parse().unwrap(), allowed_hosts: vec!["127.0.0.1".into()], https_only: false };
    Release { source, key: pair.pk.to_box().unwrap().to_string(), manifest_sha256: manifest_sha, checks, downloads }
}

/// The helper's scheduled task: the helper takes `update.lock` at once, or never starts.
struct Helper {
    starts: bool,
    launched: Mutex<Vec<(PathBuf, PathBuf)>>,
    lock: Mutex<Option<File>>,
}

impl Launcher for Helper {
    fn launch_helper(&self, helper: &Path, data: &Path) -> Result<(), UpdateError> {
        self.launched.lock().unwrap().push((helper.to_path_buf(), data.to_path_buf()));
        if self.starts {
            *self.lock.lock().unwrap() = state::try_lock(&data.join("update.lock"), Duration::ZERO).unwrap();
        }
        Ok(())
    }
}

/// A daemon without modules, with its update service.
struct Harness {
    d: common::Daemon,
    _local: TempDir,
    paths: Paths,
    release: Release,
    service: UpdateService,
    helper: Arc<Helper>,
    triggers: Arc<triggers::Activity>,
    exit: Arc<Notify>,
    /// The releases announced in a toast: version and manifest SHA-256.
    toasts: Arc<Mutex<Vec<(String, String)>>>,
}

async fn harness(installable: Result<(), NotInstallable>, helper_starts: bool) -> Harness {
    let d = common::daemon(&[]).await;
    let local = tempfile::tempdir().unwrap();
    let paths = Paths { data: d.dir.path().to_path_buf(), local: local.path().to_path_buf() };
    let release = release().await;
    let triggers = Arc::new(triggers::Activity::default());
    let helper = Arc::new(Helper { starts: helper_starts, launched: Mutex::default(), lock: Mutex::default() });
    let (exit, toasts) = (Arc::new(Notify::new()), Arc::new(Mutex::new(Vec::new())));
    let announce: Announce = {
        let toasts = Arc::clone(&toasts);
        Arc::new(move |_: &UpdateService, version: &open_mess::update::release::Version, manifest_sha256: &str| {
            toasts.lock().unwrap().push((version.to_string(), manifest_sha256.to_owned()));
        })
    };
    let service = UpdateService::spawn(Setup {
        paths: paths.clone(),
        source: release.source.clone(),
        public_key: release.key.clone(),
        accounts: Arc::clone(&d.accounts),
        activity: Activity {
            gate: d.registry.gate().clone(),
            triggers: Arc::clone(&triggers),
            pending: Arc::clone(d.registry.pending().unwrap()),
            runs: Runs::new(d.dir.path().join("runs"), Duration::from_secs(60 * 60), Counters::default()),
            modules: Vec::new(),
            host_events: mpsc::channel(1).0,
        },
        launcher: Arc::clone(&helper) as Arc<dyn Launcher>,
        exit: Arc::clone(&exit),
        installable,
        announce,
    });
    Harness { d, _local: local, paths, release, service, helper, triggers, exit, toasts }
}

impl Harness {
    /// A connection of its own to the daemon's state database.
    fn store(&self) -> Store {
        common::store(&self.paths.data)
    }

    /// Every consent row: version, manifest SHA-256, launched.
    fn consents(&self) -> Vec<(String, String, bool)> {
        let store = self.store();
        let mut rows = store.conn().prepare("SELECT version, manifest_sha256, launched_ms IS NOT NULL FROM update_consent").unwrap();
        rows.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap().map(Result::unwrap).collect()
    }

    fn launched(&self) -> Vec<(PathBuf, PathBuf)> {
        self.helper.launched.lock().unwrap().clone()
    }

    fn toasts(&self) -> Vec<String> {
        self.toasts.lock().unwrap().iter().map(|(version, _)| version.clone()).collect()
    }

    /// The window, over this daemon and its update service.
    fn ui(&self) -> UiState {
        let pending = Arc::clone(self.d.registry.pending().unwrap());
        UiState {
            accounts: Arc::clone(&self.d.accounts),
            probation: pending.probation().clone(),
            pending,
            started: std::time::Instant::now(),
            port: 7341,
            token: "t0ken".into(),
            sign_ins: self.d.sign_ins.clone(),
            counters: self.d.registry.trigger_counters().clone(),
            sessions: self.d.registry.channel_sessions().clone(),
            gate: self.d.registry.gate().clone(),
            activity: Arc::clone(&self.triggers),
            runs: Runs::new(self.d.dir.path().join("runs"), Duration::from_secs(60 * 60), Counters::default()),
            host_events: mpsc::channel(1).0,
            ui_ready: Arc::default(),
            exit: Arc::clone(&self.exit),
            update: Some(self.service.clone()),
        }
    }

    /// Asks for a check and waits for its end.
    async fn check(&self) {
        self.service.check_now();
        until("the check ends", || !matches!(self.service.view().state, UpdateState::Idle | UpdateState::Checking | UpdateState::Downloading)).await;
    }
}

/// Waits, with the paused clock still, until `done` holds; at most 20 s of real time.
async fn until(what: &str, done: impl Fn() -> bool) {
    let started = std::time::Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(20), "timed out waiting until {what}");
        tokio::task::yield_now().await;
    }
}

fn found(rolled_back: bool) -> Option<Found> {
    Some(Found { version: "9.9.9".into(), rolled_back })
}

#[tokio::test(start_paused = true)]
async fn checks_five_minutes_after_start_then_daily() {
    let h = harness(Ok(()), true).await;
    tokio::time::advance(Duration::from_secs(5 * 60 - 1)).await;
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert_eq!(h.release.checks.load(Ordering::SeqCst), 0, "nothing before 5 minutes");
    tokio::time::advance(Duration::from_secs(1)).await;
    until("the first check is ready", || h.service.view().state == UpdateState::Ready).await;
    assert_eq!(h.release.checks.load(Ordering::SeqCst), 1);
    assert_eq!(h.service.view().found, found(false));
    // Announced once, installed by nobody: no click, and installing on its own is off.
    assert_eq!(h.toasts(), ["9.9.9"]);
    assert!(h.consents().is_empty());

    tokio::time::advance(Duration::from_secs(24 * 60 * 60 - 1)).await;
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert_eq!(h.release.checks.load(Ordering::SeqCst), 1, "the next check is a day later");
    tokio::time::advance(Duration::from_secs(1)).await;
    until("the second check", || h.release.checks.load(Ordering::SeqCst) == 2).await;
    until("the second check ends", || h.service.view().state == UpdateState::Ready).await;
    // The release that is ready is neither downloaded nor announced again.
    assert_eq!(h.release.downloads.load(Ordering::SeqCst), 1);
    assert_eq!(h.toasts(), ["9.9.9"]);
}

#[tokio::test(start_paused = true)]
async fn auto_install_waits_for_quiet() {
    let h = harness(Ok(()), true).await;
    h.service.set_auto_install(true).unwrap();
    assert!(h.service.view().auto_install);
    // A trigger batch is still being collected.
    h.triggers.open_batches.store(1, Ordering::SeqCst);
    h.check().await;
    assert_eq!(h.service.view().state, UpdateState::Ready);
    // It could not start at once, so the user hears of it.
    assert_eq!(h.toasts(), ["9.9.9"]);
    sleep(10 * MINUTE).await;
    assert!(h.consents().is_empty(), "no consent while a batch is open");
    assert!(h.launched().is_empty());
    assert_eq!(h.service.view().state, UpdateState::Ready);

    // Quiet: within a minute it records the consent itself and starts phase A.
    h.triggers.open_batches.store(0, Ordering::SeqCst);
    for _ in 0..61 {
        if !h.consents().is_empty() {
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(h.consents(), [("9.9.9".to_string(), h.release.manifest_sha256.clone(), false)]);
    assert_eq!(h.service.view().state, UpdateState::Phase(Phase::A));
    // Phase A's 60 s of quiet, phase B, then the helper.
    timeout(2 * MINUTE, h.exit.notified()).await.expect("the daemon hands over to the helper");
    assert_eq!(h.launched(), [(h.paths.bin().join("om.update.exe"), h.paths.data.clone())]);
    assert_eq!(std::fs::read(h.paths.bin().join("om.update.exe")).unwrap(), OM);
    assert_eq!(h.consents(), [("9.9.9".to_string(), h.release.manifest_sha256.clone(), true)], "launched only now");
    let Read::Found(file) = state::read(&h.paths.pending_file()) else { panic!("no stage file") };
    assert_eq!((file.version.as_str(), file.stage, file.update_dir), ("9.9.9", Stage::Started, h.paths.updates().join("9.9.9")));
    assert_eq!(h.service.view().state, UpdateState::Phase(Phase::Launching));
}

#[tokio::test(start_paused = true)]
async fn rolled_back_release_is_not_auto_installed() {
    let h = harness(Ok(()), true).await;
    h.service.set_auto_install(true).unwrap();
    consent::record_rolled_back(h.store().conn(), "9.9.9", &h.release.manifest_sha256).unwrap();
    h.check().await;
    assert_eq!(h.service.view().found, found(true));
    sleep(10 * MINUTE).await;
    assert!(h.consents().is_empty());
    assert!(h.launched().is_empty());
    assert!(h.toasts().is_empty(), "a release that was rolled back is not announced again");
    // The user may still install it by hand.
    h.service.install().unwrap();
    assert_eq!(h.consents(), [("9.9.9".to_string(), h.release.manifest_sha256.clone(), false)]);
    assert_eq!(h.service.view().state, UpdateState::Phase(Phase::A));
}

#[tokio::test(start_paused = true)]
async fn not_installable_from_a_dev_build() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { data: dir.path().join("data"), local: dir.path().join("local") };
    let installed = paths.bin().join("om.exe");
    let dev_exe = dir.path().join("src").join("target").join("debug").join("om.exe");
    let dev = service::installable(&paths, &dev_exe, &paths.data).unwrap_err();
    assert_eq!(dev, NotInstallable::OtherExe { running: dev_exe.clone(), installed: installed.clone() });
    let elsewhere = dir.path().join("elsewhere");
    let other_data = service::installable(&paths, &installed, &elsewhere).unwrap_err();
    assert_eq!(other_data, NotInstallable::OtherDataDir { data: paths.data.clone(), task: elsewhere.clone() });
    // In words, in the window's language, each with the way to install.
    assert_eq!(
        dev.text(Lang::Pl),
        format!("ten demon działa z {}, a nie z {}: nową wersję instaluje scripts/install.ps1", dev_exe.display(), installed.display())
    );
    assert_eq!(
        other_data.text(Lang::Pl),
        format!(
            "folder danych {} to nie ten, który dostaje zadanie open_mess ({}): nową wersję instaluje scripts/install.ps1",
            paths.data.display(),
            elsewhere.display()
        )
    );
    assert_eq!(NotInstallable::UnknownTaskDataDir.text(Lang::Pl), "nie wiadomo, jaki folder danych dostaje zadanie open_mess");
    service::installable(&paths, &installed, &paths.data).unwrap();
    // Windows paths: the same folder in other letter case is the same folder.
    service::installable(&paths, &PathBuf::from(installed.display().to_string().to_uppercase()), &paths.data).unwrap();

    let h = harness(Err(dev.clone()), true).await;
    h.service.set_auto_install(true).unwrap();
    h.check().await;
    let view = h.service.view();
    assert_eq!((view.found, view.installable), (found(false), Err(dev.text(Lang::Pl))));
    sleep(10 * MINUTE).await;
    assert_eq!(h.service.install(), Err(dev.text(Lang::Pl)));
    assert!(h.consents().is_empty());
    assert!(h.launched().is_empty());
    assert!(h.toasts().is_empty(), "nothing to click in a toast");
}

#[tokio::test(start_paused = true)]
async fn nothing_installs_while_a_new_version_is_checked() {
    let h = harness(Ok(()), true).await;
    h.service.set_auto_install(true).unwrap();
    let probation = h.d.registry.pending().unwrap().probation().clone();
    probation.set(true);
    // The check waits for the probation's end: a release found now could not be installed, and its
    // toast's Zainstaluj would be refused.
    h.service.check_now();
    sleep(10 * MINUTE).await;
    assert!(h.service.view().probation);
    assert_eq!(h.release.checks.load(Ordering::SeqCst), 0, "no check during the probation");
    assert!(h.toasts().is_empty());
    assert_eq!(h.service.install(), Err("aktualizacja w toku".to_string()));
    assert_eq!(h.service.set_auto_install(false), Err("aktualizacja w toku".to_string()));
    assert!(h.consents().is_empty());
    assert!(h.launched().is_empty());

    // Once it ends, the check runs within a minute, and the release installs on its own.
    probation.set(false);
    tokio::time::advance(MINUTE).await;
    until("the release is installed on its own", || !h.consents().is_empty()).await;
    assert_eq!(h.release.checks.load(Ordering::SeqCst), 1);
    assert_eq!(h.service.view().state, UpdateState::Phase(Phase::A));
}

#[tokio::test(start_paused = true)]
async fn turning_auto_install_on_installs_a_ready_release() {
    let h = harness(Ok(()), true).await;
    h.check().await;
    assert_eq!(h.service.view().state, UpdateState::Ready);
    h.service.set_auto_install(true).unwrap();
    // At once: the clock stands still while this waits, so no timer is needed.
    until("the release is installed on its own", || !h.consents().is_empty()).await;
    assert_eq!(h.service.view().state, UpdateState::Phase(Phase::A));
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_install_is_not_followed_by_one_on_its_own() {
    let h = harness(Ok(()), true).await;
    h.service.set_auto_install(true).unwrap();
    h.triggers.open_batches.store(1, Ordering::SeqCst);
    h.check().await;
    // Installed by hand, then cancelled while phase A waits for the batch.
    h.service.install().unwrap();
    sleep(MINUTE).await;
    h.service.cancel();
    sleep(Duration::from_secs(1)).await;
    assert_eq!(h.service.view().state, UpdateState::Ready);
    // Quiet now: still nothing starts on its own.
    h.triggers.open_batches.store(0, Ordering::SeqCst);
    sleep(10 * MINUTE).await;
    assert_eq!(h.service.view().state, UpdateState::Ready);
    assert!(h.launched().is_empty());
}

#[tokio::test(start_paused = true)]
async fn status_shows_no_local_paths() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { data: dir.path().join("data"), local: dir.path().join("local") };
    let reason = service::installable(&paths, &dir.path().join("target").join("debug").join("om.exe"), &paths.data).unwrap_err();
    let mut h = harness(Err(reason.clone()), true).await;
    h.d.registry.set_update(h.service.clone());
    // A failed rollback names its backup; a version folder holding one refuses the download.
    let backup = h.paths.updates().join("9.9.9").join("backup");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(h.paths.last_result(), format!("rollback failed: x; backup: {}\n", backup.display())).unwrap();
    h.check().await;

    // The window gets every text.
    let view = h.service.view();
    assert!(matches!(&view.state, UpdateState::Error(why) if why.contains(&h.paths.updates().display().to_string())), "{:?}", view.state);
    assert_eq!(view.last_result, Some(format!("wycofanie nieudane: x; kopia zapasowa: {}", backup.display())));
    assert_eq!(view.installable, Err(reason.text(Lang::Pl)));

    // Agents get none of them.
    let status = h.d.registry.call("status", json!({})).await.unwrap();
    assert_eq!(
        status["update"],
        json!({"current": env!("CARGO_PKG_VERSION"), "found": null, "state": "error", "installable": false, "probation": false, "auto_install": false})
    );
    let text = status["update"].to_string();
    for folder in [&h.paths.data, &h.paths.local, dir.path()] {
        let name = folder.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!text.contains(&name), "{text} names {name}");
    }
    assert!(!text.contains('\\') && !text.contains('/'), "{text}");
}

#[tokio::test(start_paused = true)]
async fn cancel_and_a_helper_that_did_not_start_go_back_to_work() {
    let h = harness(Ok(()), false).await;
    h.triggers.open_batches.store(1, Ordering::SeqCst);
    h.check().await;
    h.service.install().unwrap();
    assert_eq!(h.service.install(), Err("instalacja już trwa".to_string()));
    sleep(5 * MINUTE).await;
    assert_eq!(h.service.view().state, UpdateState::Phase(Phase::A));
    h.service.cancel();
    sleep(Duration::from_secs(1)).await;
    assert_eq!(h.service.view().state, UpdateState::Ready, "cancelled: ready to install again");
    assert!(h.launched().is_empty());

    // Installed again, but the helper never takes update.lock.
    h.triggers.open_batches.store(0, Ordering::SeqCst);
    h.service.install().unwrap();
    sleep(3 * MINUTE).await;
    assert_eq!(h.launched().len(), 1);
    let view = h.service.view();
    assert_eq!(view.state, UpdateState::Ready);
    // Written in English, as every result is; the window shows it in its language.
    assert_eq!(std::fs::read_to_string(h.paths.last_result()).unwrap(), "aborted: the update helper did not start\n");
    assert_eq!(view.last_result.as_deref(), Some("przerwano: the update helper did not start"));
    assert!(matches!(state::read(&h.paths.pending_file()), Read::Missing));
    // Tool calls are taken again.
    h.d.registry.call("status", json!({})).await.unwrap();
    assert!(timeout(Duration::ZERO, h.exit.notified()).await.is_err(), "the daemon goes on");
}

#[tokio::test(start_paused = true)]
async fn no_tool_or_cli_command_writes_consent() {
    let mut h = harness(Ok(()), true).await;
    h.d.registry.set_update(h.service.clone());
    for tool in h.d.registry.list() {
        for word in ["update", "install", "consent", "auto"] {
            assert!(!tool.name.contains(word), "tool {} may change updates", tool.name);
        }
    }
    let before = h.d.registry.call("status", json!({})).await.unwrap();
    assert_eq!(before["update"]["current"], env!("CARGO_PKG_VERSION"));
    assert_eq!(before["update"]["found"], json!(null));
    assert_eq!(before["update"]["auto_install"], json!(false));
    h.check().await;
    let after = h.d.registry.call("status", json!({})).await.unwrap();
    assert_eq!(after["update"]["found"], json!({"version": "9.9.9", "rolled_back": false}));
    assert_eq!(after["update"]["state"], json!("ready"));
    // status takes no arguments, and there is no tool to install.
    assert!(matches!(h.d.registry.call("status", json!({"auto_install": true})).await, Err(ToolError::InvalidArgs(_))));
    assert!(matches!(h.d.registry.call("install_update", json!({})).await, Err(ToolError::UnknownTool(_))));
    assert!(h.consents().is_empty());
    assert!(!consent::auto_install(h.store().conn()).unwrap());
    // The CLI's own commands: none installs or changes the option (apply-update needs a consent).
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_om")).arg("--help").output().unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("daemon"), "{help}");
    for word in ["update", "install", "consent"] {
        assert!(!help.to_lowercase().contains(word), "om --help offers {word}: {help}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_toast_installs_only_the_release_it_announced() {
    let h = harness(Ok(()), true).await;
    h.triggers.open_batches.store(1, Ordering::SeqCst);
    h.check().await;
    let announced = h.toasts.lock().unwrap()[0].1.clone();
    assert_eq!(announced, h.release.manifest_sha256);
    // A toast left in Action Center for an earlier release: what is ready now is another one.
    assert!(h.service.install_announced("the manifest of an earlier release").is_err());
    assert!(h.consents().is_empty());
    assert_eq!(h.service.view().state, UpdateState::Ready);
    h.service.install_announced(&announced).unwrap();
    assert_eq!(h.consents(), [("9.9.9".to_string(), announced, false)]);
    assert_eq!(h.service.view().state, UpdateState::Phase(Phase::A));
}

/// A module loaded while the phases run is not among those phase A paused: it would go on polling
/// through phase B and the launch (spec 7.1).
#[tokio::test(start_paused = true)]
async fn modules_are_not_changed_while_an_installation_stops_the_daemon() {
    let h = harness(Ok(()), true).await;
    h.triggers.open_batches.store(1, Ordering::SeqCst);
    h.check().await;
    let ui = h.ui();
    h.service.install().unwrap();
    assert_eq!(h.service.view().state, UpdateState::Phase(Phase::A));
    let refused = Err(commands::update_in_progress().to_string());
    assert_eq!(commands::rescan_modules(&ui).await, refused);
    assert_eq!(commands::set_module_enabled(&ui, "test-echo", true).await, refused);
    assert_eq!(commands::approve_module(&ui, "test-echo", "file", "manifest").await, refused);

    h.service.cancel();
    sleep(Duration::from_secs(1)).await;
    assert_eq!(h.service.view().state, UpdateState::Ready);
    commands::rescan_modules(&ui).await.unwrap();
}
