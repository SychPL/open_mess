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
use crate::modules::ModuleState;
use crate::pending::{State, now_ms};
use crate::ui::commands::UPDATE_IN_PROGRESS;

/// The first check after the start, and then the time between checks (spec 5).
const FIRST_CHECK: Duration = Duration::from_secs(5 * 60);
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// How often an installation on its own looks again for a quiet moment.
const QUIET_RETRY: Duration = Duration::from_secs(60);
/// `last-update.txt` holds one line; what is longer is not read.
const MAX_RESULT: u64 = 4096;

/// Shows that a release is ready (a toast on Windows); its button calls [`UpdateService::install`].
pub type Announce = Arc<dyn Fn(&UpdateService, &Version) + Send + Sync>;

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
    pub installable: Result<(), String>,
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
    /// Why this daemon does not install (null when it does).
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

/// Checks, downloads and installs releases; clones share it.
#[derive(Clone)]
pub struct UpdateService(Arc<Inner>);

struct Inner {
    setup: Setup,
    /// Where an installation runs: the toast calls [`UpdateService::install`] on a thread of its own.
    runtime: Handle,
    check_now: Notify,
    cancel: watch::Sender<bool>,
    shared: Mutex<Shared>,
}

#[derive(Default)]
struct Shared {
    state: UpdateState,
    ready: Option<Ready>,
    /// The release (its manifest SHA-256) announced, and the one installed on its own: each once,
    /// so that a cancel or a helper that did not start is not followed by the same again.
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
        self.start_install(false)
    }

    /// "Anuluj": ends phase A or B; the daemon goes back to work. The helper's launch is not
    /// cancelled.
    pub fn cancel(&self) {
        if matches!(self.shared().state, UpdateState::Phase(Phase::A | Phase::B)) {
            self.0.cancel.send_replace(true);
        }
    }

    /// "Instaluj aktualizacje automatycznie"; only the window sets it.
    pub fn set_auto_install(&self, on: bool) -> Result<(), String> {
        self.not_held()?;
        consent::set_auto_install(self.0.setup.accounts.store().conn(), on).map_err(text)
    }

    pub fn view(&self) -> UpdateView {
        let (state, found) = {
            let shared = self.shared();
            let found = shared.ready.as_ref().map(|ready| Found { version: ready.downloaded.version.to_string(), rolled_back: ready.rolled_back });
            (shared.state.clone(), found)
        };
        UpdateView {
            current: Version::current().to_string(),
            found,
            state,
            installable: self.0.setup.installable.clone(),
            probation: self.held(),
            auto_install: self.auto_install(),
            last_result: last_result(&self.0.setup.paths),
        }
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
        if self.held() { Err(UPDATE_IN_PROGRESS.into()) } else { Ok(()) }
    }

    fn auto_install(&self) -> bool {
        consent::auto_install(self.0.setup.accounts.store().conn()).unwrap_or(false)
    }

    async fn run(self, first: Instant) {
        let mut checks = interval_at(first, EVERY);
        checks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let waits = self.auto_install_due();
            tokio::select! {
                _ = checks.tick() => self.check().await,
                () = self.0.check_now.notified() => self.check().await,
                () = sleep(QUIET_RETRY), if waits => {}
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
        let result = self.fetch(earlier.as_ref()).await;
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

    /// Shows the toast for a release that is ready, once, unless the user cannot install it or it
    /// was rolled back.
    fn announce(&self) {
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
        (self.0.setup.announce)(self, &ready.downloaded.version);
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
        self.auto_install_due() && quiet() && self.start_install(true).is_ok()
    }

    fn start_install(&self, auto: bool) -> Result<(), String> {
        let setup = &self.0.setup;
        setup.installable.clone()?;
        self.not_held()?;
        let ready = {
            let mut shared = self.shared();
            let ready = match (&shared.state, &shared.ready) {
                (UpdateState::Ready, Some(ready)) => ready.clone(),
                (UpdateState::Phase(_), _) => return Err("instalacja już trwa".into()),
                (UpdateState::Checking | UpdateState::Downloading, _) => return Err("trwa sprawdzanie aktualizacji, spróbuj za chwilę".into()),
                _ => return Err("nie ma pobranej wersji do zainstalowania".into()),
            };
            if auto {
                shared.auto_started = Some(ready.downloaded.manifest_sha256.clone());
            }
            shared.state = UpdateState::Phase(Phase::A);
            ready
        };
        let downloaded = ready.downloaded;
        let clicked = consent::record_click(setup.accounts.store().conn(), &downloaded.version.to_string(), &downloaded.manifest_sha256, now_ms());
        if let Err(err) = clicked {
            self.set_state(UpdateState::Ready);
            return Err(err.to_string());
        }
        // Before the phases start, so that a cancel right away is not lost.
        self.0.cancel.send_replace(false);
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
            return self.set_state(UpdateState::Ready);
        }
        self.set_state(UpdateState::Phase(Phase::B));
        if switch(&activity, &cancel).await.is_err() {
            return self.set_state(UpdateState::Ready);
        }
        self.set_state(UpdateState::Phase(Phase::Launching));
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
                self.set_state(UpdateState::Ready);
            }
        }
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
pub fn installable(paths: &Paths, current_exe: &Path, task_data_dir: &Path) -> Result<(), String> {
    const HINT: &str = "nową wersję instaluje scripts/install.ps1";
    let installed = paths.bin().join("om.exe");
    if !same(current_exe, &installed) {
        return Err(format!("ten demon działa z {}, a nie z {}: {HINT}", current_exe.display(), installed.display()));
    }
    if !same(&paths.data, task_data_dir) {
        return Err(format!(
            "folder danych {} to nie ten, który dostaje zadanie open_mess ({}): {HINT}",
            paths.data.display(),
            task_data_dir.display()
        ));
    }
    Ok(())
}

/// The same path, letter case aside (Windows).
fn same(a: &Path, b: &Path) -> bool {
    let parts = |path: &Path| path.components().map(|part| part.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>();
    parts(a) == parts(b)
}

/// [`installable`] for the running daemon.
pub fn installable_here(paths: &Paths) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(text)?;
    let task_data_dir = task_data_dir().ok_or_else(|| "nie wiadomo, jaki folder danych dostaje zadanie open_mess".to_owned())?;
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
