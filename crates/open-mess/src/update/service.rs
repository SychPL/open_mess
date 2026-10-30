//! The daemon's side of self-update (spec 2026-10-27-self-update, 5-7). It asks GitHub for a newer
//! release 5 minutes after the start, then every 24 hours and whenever the user asks, downloads and
//! verifies it, and announces it in a toast and in the window. Only the user's click there, or the
//! option to install on its own, starts an installation: the click is recorded as the consent, the
//! two phases of 7.1 run, and the helper takes over. Nothing here is a tool: no agent reaches it,
//! and `status` only shows [`UpdateService::view`].

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Serialize, Serializer};
use tokio::runtime::Handle;
use tokio::sync::{Notify, watch};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep};

use super::UpdateError;
use super::consent;
use super::fetch::{Downloaded, Source, client, download, latest};
use super::quiesce::{Activity, Launcher, finish_work, launch, resume, switch};
use super::release::Version;
use super::state::{self, Paths};
use crate::accounts::Accounts;
use crate::i18n::{self, Lang};
use crate::modules::ModuleState;
use crate::pending::{State, now_ms};
use crate::ui::commands::update_in_progress;

/// The first check after the start, and then the time between checks (spec 5).
const FIRST_CHECK: Duration = Duration::from_secs(5 * 60);
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// How often an installation on its own looks again for a quiet moment.
const QUIET_RETRY: Duration = Duration::from_secs(60);
/// A whole check, downloads included. Each request has a limit of its own (fetch.rs); this one
/// makes sure the service never waits for ever.
const CHECK_LIMIT: Duration = Duration::from_secs(60 * 60);
/// `last-update.txt` holds one line; what is longer is not read.
const MAX_RESULT: u64 = 4096;

/// Shows that a release is ready (a toast on Windows), with its manifest SHA-256; its button calls
/// [`UpdateService::install_announced`].
pub type Announce = Arc<dyn Fn(&UpdateService, &Version, &str) + Send + Sync>;

/// What the service works with.
pub struct Setup {
    pub paths: Paths,
    pub source: Source,
    pub public_key: String,
    pub accounts: Arc<Accounts>,
    /// What the phases wait for and stop; its modules are taken from `accounts` right before
    /// phase A, so the list is current.
    pub activity: Activity,
    pub launcher: Arc<dyn Launcher>,
    /// Ends the daemon once the helper took over.
    pub exit: Arc<Notify>,
    /// [`installable`] for this daemon.
    pub installable: Result<(), NotInstallable>,
    pub announce: Announce,
}

/// The phases of an installation (spec 7.1), then the helper's launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    A,
    B,
    Launching,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "lowercase")]
pub enum UpdateState {
    /// No newer release found.
    #[default]
    Idle,
    Checking,
    Downloading,
    /// A newer release is downloaded and verified.
    Ready,
    Error(String),
    Phase(Phase),
}

/// The release that is ready.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Found {
    pub version: String,
    /// It was installed once and rolled back: never installed on its own again (spec 5).
    pub rolled_back: bool,
}

/// What the window and `status` show.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateView {
    pub current: String,
    pub found: Option<Found>,
    pub state: UpdateState,
    /// Why this daemon does not install (null when it does), in the window's language.
    #[serde(rename = "not_installable", serialize_with = "why_not")]
    pub installable: Result<(), String>,
    /// A new version is being checked: nothing is installed or saved meanwhile (spec 7.2).
    pub probation: bool,
    pub auto_install: bool,
    /// The last installation's result, from `last-update.txt`.
    pub last_result: Option<String>,
}

fn why_not<S: Serializer>(installable: &Result<(), String>, serializer: S) -> Result<S::Ok, S::Error> {
    installable.as_ref().err().serialize(serializer)
}

/// Why this daemon does not install an update itself; `scripts/install.ps1` installs then. Put in
/// words when read, so the words follow the window's language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotInstallable {
    /// It runs from another `om.exe` than the installed one (a development build, say).
    OtherExe { running: PathBuf, installed: PathBuf },
    /// Its data folder is not the one the scheduled task `open_mess` gives the daemon it starts.
    OtherDataDir { data: PathBuf, task: PathBuf },
    /// It is not known which data folder that task gives.
    UnknownTaskDataDir,
    /// Windows did not say where `om.exe` is.
    UnknownExe(String),
}

impl NotInstallable {
    pub fn text(&self, lang: Lang) -> String {
        let hint = lang.pick("nową wersję instaluje scripts/install.ps1", "scripts/install.ps1 installs new versions");
        match (self, lang) {
            (NotInstallable::OtherExe { running, installed }, Lang::Pl) => {
                format!("ten demon działa z {}, a nie z {}: {hint}", running.display(), installed.display())
            }
            (NotInstallable::OtherExe { running, installed }, Lang::En) => {
                format!("this daemon runs from {}, not from {}: {hint}", running.display(), installed.display())
            }
            (NotInstallable::OtherDataDir { data, task }, Lang::Pl) => {
                format!("folder danych {} to nie ten, który dostaje zadanie open_mess ({}): {hint}", data.display(), task.display())
            }
            (NotInstallable::OtherDataDir { data, task }, Lang::En) => {
                format!("the data folder {} is not the one the open_mess task gives the daemon ({}): {hint}", data.display(), task.display())
            }
            (NotInstallable::UnknownTaskDataDir, _) => lang
                .pick("nie wiadomo, jaki folder danych dostaje zadanie open_mess", "it is not known which data folder the open_mess task gives")
                .into(),
            (NotInstallable::UnknownExe(why), _) => why.clone(),
        }
    }
}

/// What `status` shows agents: [`UpdateView`] without its texts, which can name local folders and so
/// the user's name (an error, the reason it does not install, a failed rollback's backup).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusView {
    pub current: String,
    pub found: Option<Found>,
    /// `idle`, `checking`, `downloading`, `ready`, `error` or `phase`.
    pub state: &'static str,
    pub installable: bool,
    pub probation: bool,
    pub auto_install: bool,
}

impl UpdateState {
    fn kind(&self) -> &'static str {
        match self {
            UpdateState::Idle => "idle",
            UpdateState::Checking => "checking",
            UpdateState::Downloading => "downloading",
            UpdateState::Ready => "ready",
            UpdateState::Error(_) => "error",
            UpdateState::Phase(_) => "phase",
        }
    }
}

/// Checks, downloads and installs releases; clones share it.
#[derive(Clone)]
pub struct UpdateService(Arc<Inner>);

struct Inner {
    setup: Setup,
    /// Where an installation runs: the toast calls [`UpdateService::install`] on a thread of its own.
    runtime: Handle,
    check_now: Notify,
    /// Looks again whether to install on its own: the option was turned on, or an installation went
    /// back to ready.
    wake: Notify,
    cancel: watch::Sender<bool>,
    shared: Mutex<Shared>,
}

#[derive(Default)]
struct Shared {
    state: UpdateState,
    ready: Option<Ready>,
    /// The release (its manifest SHA-256) announced, and the one started while installing on its own
    /// was on: each once, so that a cancel or a helper that did not start is not followed by the
    /// same again.
    announced: Option<String>,
    auto_started: Option<String>,
}

#[derive(Clone)]
struct Ready {
    downloaded: Downloaded,
    rolled_back: bool,
}

fn text(err: impl std::fmt::Display) -> String {
    err.to_string()
}

impl UpdateService {
    /// Starts the checks on the current runtime: the first 5 minutes from now, then every 24 hours.
    pub fn spawn(setup: Setup) -> UpdateService {
        let service = UpdateService(Arc::new(Inner {
            setup,
            runtime: Handle::current(),
            check_now: Notify::new(),
            wake: Notify::new(),
            cancel: watch::channel(false).0,
            shared: Mutex::default(),
        }));
        // Counted from now, not from the task's first turn.
        let first = Instant::now().checked_add(FIRST_CHECK).unwrap_or_else(Instant::now);
        tokio::spawn(service.clone().run(first));
        service
    }

    /// "Sprawdź teraz": checks at once, unless a check or an installation runs.
    pub fn check_now(&self) {
        self.0.check_now.notify_one();
    }

    /// "Zainstaluj": records the user's consent for the release that is ready and installs it in
    /// the background (spec 6, 7.1). Refused, with the reason, when this daemon cannot install,
    /// while a new version is checked, and when nothing is ready.
    pub fn install(&self) -> Result<(), String> {
        self.start_install(false, None)
    }

    /// "Zainstaluj" in the toast that announced the release with `manifest_sha256`: the user agreed
    /// to that release only, so it is refused when another one is ready now (spec 6).
    pub fn install_announced(&self, manifest_sha256: &str) -> Result<(), String> {
        self.start_install(false, Some(manifest_sha256))
    }

    /// Whether an installation stops the daemon now (phase A, B or the helper's launch).
    pub fn installing(&self) -> bool {
        matches!(self.shared().state, UpdateState::Phase(_))
    }

    /// "Anuluj": ends phase A or B; the daemon goes back to work. The helper's launch is not
    /// cancelled.
    pub fn cancel(&self) {
        let shared = self.shared();
        if matches!(shared.state, UpdateState::Phase(Phase::A | Phase::B)) {
            self.0.cancel.send_replace(true);
        }
    }

    /// "Instaluj aktualizacje automatycznie"; only the window sets it.
    pub fn set_auto_install(&self, on: bool) -> Result<(), String> {
        self.not_held()?;
        consent::set_auto_install(self.0.setup.accounts.store().conn(), on).map_err(text)?;
        self.0.wake.notify_one();
        Ok(())
    }

    /// Everything, for the window.
    pub fn view(&self) -> UpdateView {
        let (state, found) = self.state_and_found();
        UpdateView {
            current: Version::current().to_string(),
            found,
            state,
            installable: self.installable(),
            probation: self.held(),
            auto_install: self.auto_install(),
            last_result: last_result(&self.0.setup.paths),
        }
    }

    /// For `status`, which agents read.
    pub fn status(&self) -> StatusView {
        let (state, found) = self.state_and_found();
        StatusView {
            current: Version::current().to_string(),
            found,
            state: state.kind(),
            installable: self.0.setup.installable.is_ok(),
            probation: self.held(),
            auto_install: self.auto_install(),
        }
    }

    /// Why this daemon does not install, in the current language.
    fn installable(&self) -> Result<(), String> {
        self.0.setup.installable.as_ref().map_err(|why| why.text(i18n::current())).copied()
    }

    fn state_and_found(&self) -> (UpdateState, Option<Found>) {
        let shared = self.shared();
        let found = shared.ready.as_ref().map(|ready| Found { version: ready.downloaded.version.to_string(), rolled_back: ready.rolled_back });
        (shared.state.clone(), found)
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.0.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_state(&self, state: UpdateState) {
        self.shared().state = state;
    }

    fn held(&self) -> bool {
        self.0.setup.activity.pending.probation().get()
    }

    fn not_held(&self) -> Result<(), String> {
        if self.held() { Err(update_in_progress().into()) } else { Ok(()) }
    }

    fn auto_install(&self) -> bool {
        consent::auto_install(self.0.setup.accounts.store().conn()).unwrap_or(false)
    }

    async fn run(self, first: Instant) {
        let mut checks = interval_at(first, EVERY);
        checks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // A check due while a new version is checked waits for the probation's end: nothing found
        // then could be installed.
        let mut check_due = false;
        loop {
            let retry = check_due || self.auto_install_due();
            tokio::select! {
                _ = checks.tick() => check_due = true,
                () = self.0.check_now.notified() => check_due = true,
                () = self.0.wake.notified() => {}
                () = sleep(QUIET_RETRY), if retry => {}
            }
            if check_due && !self.held() {
                check_due = false;
                self.check().await;
            }
            if !self.try_auto_install() {
                self.announce();
            }
        }
    }

    /// Looks for a newer release and downloads it; a release already ready is not downloaded again.
    async fn check(&self) {
        let earlier = {
            let mut shared = self.shared();
            if matches!(shared.state, UpdateState::Checking | UpdateState::Downloading | UpdateState::Phase(_)) {
                return;
            }
            shared.state = UpdateState::Checking;
            shared.ready.clone()
        };
        let result = tokio::time::timeout(CHECK_LIMIT, self.fetch(earlier.as_ref()))
            .await
            .unwrap_or_else(|_| Err(text(UpdateError::Network("the check did not end within an hour".into()))));
        let mut shared = self.shared();
        match result {
            Ok(Some(ready)) => (shared.state, shared.ready) = (UpdateState::Ready, Some(ready)),
            Ok(None) => (shared.state, shared.ready) = (UpdateState::Idle, None),
            // A failed later check says nothing about the release that is ready.
            Err(_) if shared.ready.is_some() => shared.state = UpdateState::Ready,
            Err(err) => shared.state = UpdateState::Error(err),
        }
    }

    async fn fetch(&self, earlier: Option<&Ready>) -> Result<Option<Ready>, String> {
        let setup = &self.0.setup;
        let client = client(&setup.source).map_err(text)?;
        let Some(candidate) = latest(&client, &setup.source, Version::current()).await.map_err(text)? else {
            return Ok(None);
        };
        if let Some(earlier) = earlier.filter(|earlier| earlier.downloaded.version == candidate.version) {
            return Ok(Some(earlier.clone()));
        }
        self.set_state(UpdateState::Downloading);
        // Into `updates\<version>`: the helper refuses an update folder anywhere else.
        let downloaded =
            download(&client, &candidate, &setup.public_key, &setup.paths.updates()).await.map_err(|err| format!("{}: {err}", candidate.version))?;
        let rolled_back =
            consent::rolled_back(setup.accounts.store().conn(), &downloaded.version.to_string(), &downloaded.manifest_sha256).map_err(text)?;
        Ok(Some(Ready { downloaded, rolled_back }))
    }

    /// Shows the toast for a release that is ready, once, unless the user cannot install it, it was
    /// rolled back, or a new version is being checked (Zainstaluj would be refused).
    fn announce(&self) {
        if self.held() {
            return;
        }
        let ready = {
            let mut shared = self.shared();
            let Some(ready) = shared.ready.clone().filter(|ready| shared.state == UpdateState::Ready && !ready.rolled_back) else {
                return;
            };
            if shared.announced.as_deref() == Some(ready.downloaded.manifest_sha256.as_str()) || self.0.setup.installable.is_err() {
                return;
            }
            shared.announced = Some(ready.downloaded.manifest_sha256.clone());
            ready
        };
        (self.0.setup.announce)(self, &ready.downloaded.version, &ready.downloaded.manifest_sha256);
    }

    /// Whether an installation on its own waits for a quiet moment: the option is on, and a release
    /// is ready that was neither rolled back nor started on its own before.
    fn auto_install_due(&self) -> bool {
        let due = {
            let shared = self.shared();
            match (&shared.state, &shared.ready) {
                (UpdateState::Ready, Some(ready)) => {
                    !ready.rolled_back && shared.auto_started.as_deref() != Some(ready.downloaded.manifest_sha256.as_str())
                }
                _ => false,
            }
        };
        due && self.0.setup.installable.is_ok() && !self.held() && self.auto_install()
    }

    /// Installs on its own when it is due and nothing is going on (spec 6): no action waits for the
    /// user or runs, no tool call, no program a rule started, no batch collected or delivered.
    fn try_auto_install(&self) -> bool {
        let activity = &self.0.setup.activity;
        let quiet = || {
            matches!(activity.pending.count(State::Waiting), Ok(0))
                && matches!(activity.pending.count(State::Approved), Ok(0))
                && activity.gate.in_flight() == 0
                && activity.runs.running() + activity.runs.queued() == 0
                && activity.triggers.open_batches.load(Ordering::SeqCst) == 0
                && activity.triggers.delivering.load(Ordering::SeqCst) == 0
        };
        self.auto_install_due() && quiet() && self.start_install(true, None).is_ok()
    }

    /// Installs the release that is ready; with `announced`, only if it is that one.
    fn start_install(&self, auto: bool, announced: Option<&str>) -> Result<(), String> {
        let setup = &self.0.setup;
        self.installable()?;
        self.not_held()?;
        let auto = auto || self.auto_install();
        let ready = {
            let mut shared = self.shared();
            let ready = match (&shared.state, &shared.ready) {
                (UpdateState::Ready, Some(ready)) => ready.clone(),
                (UpdateState::Phase(_), _) => return Err(i18n::pick("instalacja już trwa", "an installation is already running").into()),
                (UpdateState::Checking | UpdateState::Downloading, _) => {
                    return Err(i18n::pick("trwa sprawdzanie aktualizacji, spróbuj za chwilę", "checking for updates, try again in a moment").into());
                }
                _ => return Err(i18n::pick("nie ma pobranej wersji do zainstalowania", "no downloaded version to install").into()),
            };
            if announced.is_some_and(|announced| announced != ready.downloaded.manifest_sha256) {
                return Err(i18n::pick(
                    "gotowa jest inna wersja niż ta z powiadomienia",
                    "a different version than the one in the notification is ready",
                )
                .into());
            }
            if auto {
                shared.auto_started = Some(ready.downloaded.manifest_sha256.clone());
            }
            // With the phase reserved, so that a cancel right away is not lost.
            self.0.cancel.send_replace(false);
            shared.state = UpdateState::Phase(Phase::A);
            ready
        };
        let downloaded = ready.downloaded;
        let clicked = consent::record_click(setup.accounts.store().conn(), &downloaded.version.to_string(), &downloaded.manifest_sha256, now_ms());
        if let Err(err) = clicked {
            self.set_state(UpdateState::Ready);
            return Err(err.to_string());
        }
        self.0.runtime.spawn(self.clone().run_install(downloaded));
        Ok(())
    }

    /// Phases A and B, then the helper (spec 7.1). A cancel, or a launch that failed, goes back to
    /// work; the release stays ready.
    async fn run_install(self, downloaded: Downloaded) {
        let setup = &self.0.setup;
        let activity = Activity { modules: setup.accounts.modules().handles(), ..setup.activity.clone() };
        let cancel = self.0.cancel.subscribe();
        // Both phases resume everything themselves when cancelled.
        if finish_work(&activity, &cancel).await.is_err() {
            return self.back_to_ready();
        }
        self.set_state(UpdateState::Phase(Phase::B));
        if switch(&activity, &cancel).await.is_err() {
            return self.back_to_ready();
        }
        // Under the lock a cancel takes: one that came after phase B looked is not lost.
        let launching = {
            let mut shared = self.shared();
            let go = !*cancel.borrow();
            if go {
                shared.state = UpdateState::Phase(Phase::Launching);
            }
            go
        };
        if !launching {
            resume(&activity).await;
            return self.back_to_ready();
        }
        let active = setup.accounts.modules().list().into_iter().filter(|info| info.state == ModuleState::Active).map(|info| info.name).collect();
        // The consent's 10 minutes start now, not at the click (spec 6).
        let launched = consent::mark_launched(setup.accounts.store().conn(), &downloaded.version.to_string(), &downloaded.manifest_sha256, now_ms());
        let launched = match launched {
            Ok(()) => launch(&setup.paths, &downloaded, active, &*setup.launcher, &setup.exit).await,
            Err(err) => Err(err),
        };
        match launched {
            Ok(()) => {} // the daemon ends
            // The daemon ends too; the recovery at its next start deals with the stage file.
            Err(err @ UpdateError::StageFileLeft(_)) => self.set_state(UpdateState::Error(err.to_string())),
            Err(err) => {
                resume(&activity).await;
                state::record_result(&setup.paths, &format!("przerwano: {err}"));
                self.back_to_ready();
            }
        }
    }

    /// The installation stopped and the release is ready again; installing on its own may now be due.
    fn back_to_ready(&self) {
        self.set_state(UpdateState::Ready);
        self.0.wake.notify_one();
    }
}

/// The first line of `last-update.txt`.
fn last_result(paths: &Paths) -> Option<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(paths.last_result()).ok()?.take(MAX_RESULT).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    text.lines().next().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned)
}

/// Whether this daemon can install an update (spec 7): it runs as `<local>\bin\om.exe`, and its data
/// folder is the one the scheduled task `open_mess` gives the daemon it starts. Otherwise (a
/// development build, a data folder set only in one shell) `scripts/install.ps1` installs.
pub fn installable(paths: &Paths, current_exe: &Path, task_data_dir: &Path) -> Result<(), NotInstallable> {
    let installed = paths.bin().join("om.exe");
    if !same(current_exe, &installed) {
        return Err(NotInstallable::OtherExe { running: current_exe.to_path_buf(), installed });
    }
    if !same(&paths.data, task_data_dir) {
        return Err(NotInstallable::OtherDataDir { data: paths.data.clone(), task: task_data_dir.to_path_buf() });
    }
    Ok(())
}

/// The same path, letter case aside (Windows).
fn same(a: &Path, b: &Path) -> bool {
    let parts = |path: &Path| path.components().map(|part| part.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>();
    parts(a) == parts(b)
}

/// [`installable`] for the running daemon.
pub fn installable_here(paths: &Paths) -> Result<(), NotInstallable> {
    let exe = std::env::current_exe().map_err(|err| NotInstallable::UnknownExe(err.to_string()))?;
    let task_data_dir = task_data_dir().ok_or(NotInstallable::UnknownTaskDataDir)?;
    installable(paths, &exe, &task_data_dir)
}

/// The data folder of the daemon the task `open_mess` starts, as scripts/install.ps1 reads it: the
/// user's persistent `OPEN_MESS_DATA_DIR` (`HKCU\Environment`), else `%APPDATA%\open_mess`.
#[cfg(windows)]
fn task_data_dir() -> Option<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let persistent = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Environment")
        .and_then(|environment| environment.get_value::<String, _>("OPEN_MESS_DATA_DIR"))
        .ok()
        .filter(|dir| !dir.is_empty());
    match persistent {
        Some(dir) => Some(PathBuf::from(dir)),
        None => std::env::var_os("APPDATA").filter(|dir| !dir.is_empty()).map(|dir| PathBuf::from(dir).join("open_mess")),
    }
}

#[cfg(not(windows))]
fn task_data_dir() -> Option<PathBuf> {
    None
}
