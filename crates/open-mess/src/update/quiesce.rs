//! Stopping the daemon for an update (spec 2026-10-27-self-update, 7.1). Phase A pauses the
//! modules and lets the work already started finish: the events in the chain to the rules'
//! actions, the batches, the programs they started, the approved writes, and the agents' tool calls
//! until 60 s of quiet. Phase B refuses every new tool call and waits for the running ones and for
//! the actions waiting for the user. Then the daemon hands over to the helper and exits once the
//! helper holds `update.lock`. A cancel, or a helper that does not start, resumes everything, unless
//! the stage file cannot be withdrawn: then the daemon exits and the recovery takes over.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use open_mess_host::{HostEvent, ModuleHandle};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{Instant, sleep};

use super::UpdateError;
use super::fetch::Downloaded;
use super::release::{Manifest, manifest_sha256};
use super::state::{self, Paths, PendingFile, Stage};
use crate::pending::{Pending, State};
use crate::tools::{Gate, GateMode};
use crate::triggers;
use crate::triggers::spawn::Runs;

/// How often the phases look again.
const POLL: Duration = Duration::from_millis(200);
/// No delivery and no tool call for this long: an agent woken by the last delivery had its chance.
const QUIET: Duration = Duration::from_secs(60);
/// Phase A gives up after this and phase B starts: an agent that never stops calling tools does
/// not hold the update back.
const GIVE_UP: Duration = Duration::from_secs(30 * 60);
/// How long the helper gets to take `update.lock`.
const HELPER_START: Duration = Duration::from_secs(30);
/// The helper program, next to `om.exe`; the scheduled task runs it.
pub(crate) const HELPER: &str = "om.update.exe";

/// What the phases wait for and stop.
pub struct Activity {
    pub gate: Gate,
    pub triggers: Arc<triggers::Activity>,
    pub pending: Arc<Pending>,
    pub runs: Arc<Runs>,
    /// Every loaded module.
    pub modules: Vec<ModuleHandle>,
    /// The modules' event queue: at full capacity it is empty.
    pub host_events: mpsc::Sender<HostEvent>,
}

/// The user cancelled; everything was resumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cancelled;

/// Phase A: pauses the modules and waits until the work they started is done and 60 s passed with
/// no delivery and no tool call, at most 30 minutes. Tool calls keep working meanwhile.
pub async fn finish_work(activity: &Activity, cancel: &watch::Receiver<bool>) -> Result<(), Cancelled> {
    for module in &activity.modules {
        // Returns once no event handler runs; a module that is gone runs none either.
        let _ = module.pause_events().await;
    }
    let started = Instant::now();
    let mut quiet_since = started;
    let mut ends = (activity.triggers.last_delivery_ms.load(Ordering::SeqCst), activity.gate.last_end_ms());
    let mut idle_before = false;
    loop {
        if *cancel.borrow() {
            resume(activity).await;
            return Err(Cancelled);
        }
        let (idle, now_ends, calls) = look(activity);
        let now = Instant::now();
        // The end times are wall-clock milliseconds; the quiet is measured on tokio's clock.
        if now_ends != ends || calls > 0 {
            (quiet_since, ends) = (now, now_ends);
        }
        let idle = idle && now.duration_since(quiet_since) >= QUIET;
        // Twice in a row: an event the bus took from the queue but has not counted yet is seen
        // the next time.
        if (idle && idle_before) || now.duration_since(started) >= GIVE_UP {
            return Ok(());
        }
        idle_before = idle;
        sleep(POLL).await;
    }
}

/// Whether the chain from the modules' events to the rules' actions is empty and no approved write
/// runs; the last delivery's and the last tool call's end; and the tool calls running.
fn look(activity: &Activity) -> (bool, (u64, u64), usize) {
    // In this order: whatever moves work from one counter to the next raises the next before it
    // lowers this one, so a reader who sees this one lowered sees the next one raised.
    let triggers = &activity.triggers;
    let queue_empty = activity.host_events.capacity() == activity.host_events.max_capacity();
    let received = triggers.received.load(Ordering::SeqCst);
    let consumed = triggers.consumed.load(Ordering::SeqCst);
    let open = triggers.open_batches.load(Ordering::SeqCst);
    let delivering = triggers.delivering.load(Ordering::SeqCst);
    let delivered = triggers.last_delivery_ms.load(Ordering::SeqCst);
    let runs = activity.runs.running() + activity.runs.queued();
    let calls = activity.gate.in_flight();
    let called = activity.gate.last_end_ms();
    let approved = activity.pending.count(State::Approved);
    let idle = queue_empty && received == consumed && open == 0 && delivering == 0 && runs == 0 && matches!(approved, Ok(0));
    (idle, (delivered, called), calls)
}

/// Phase B: refuses every new tool call and waits until none runs and no action waits for the user
/// or runs. Approving and rejecting in the window still work.
pub async fn switch(activity: &Activity, cancel: &watch::Receiver<bool>) -> Result<(), Cancelled> {
    activity.gate.set(GateMode::Closed);
    loop {
        if *cancel.borrow() {
            resume(activity).await;
            return Err(Cancelled);
        }
        // In this order: a call that ended has stored its action, and an approval moves an action
        // from waiting to approved.
        let calls = activity.gate.in_flight();
        let waiting = activity.pending.count(State::Waiting);
        let approved = activity.pending.count(State::Approved);
        if calls == 0 && matches!(waiting, Ok(0)) && matches!(approved, Ok(0)) {
            return Ok(());
        }
        sleep(POLL).await;
    }
}

/// Back to work: tool calls are taken and the modules get their events again.
pub async fn resume(activity: &Activity) {
    activity.gate.set(GateMode::Open);
    for module in &activity.modules {
        let _ = module.resume_events().await;
    }
}

/// Starts the helper: in real life the scheduled task `open_mess-update` (system.rs).
pub trait Launcher: Send + Sync {
    fn launch_helper(&self, helper: &Path, data: &Path) -> Result<(), UpdateError>;
}

/// Hands over to the helper: copies the release's verified `om.exe` to `bin\om.update.exe`, writes
/// `update-pending.json` at stage `started`, launches the helper and notifies `exit` once the helper
/// holds `update.lock`. A helper that does not take it within 30 s did not start: holding the lock,
/// so that a late one finds nothing to do, the daemon deletes the file.
///
/// On `Err` the caller goes back to work with [`resume`], except after
/// [`UpdateError::StageFileLeft`]: the file could not be deleted (or the lock taken), and a helper
/// started by the task's own trigger within 5 minutes would stop a daemon back at work. `exit` is
/// notified then, and the daemon ends instead; the recovery deals with the file (spec 7.6).
pub async fn launch(
    paths: &Paths,
    downloaded: &Downloaded,
    active_modules: Vec<String>,
    launcher: &dyn Launcher,
    exit: &Notify,
) -> Result<(), UpdateError> {
    // ponytail: blocking file work on the runtime, a few MiB once per update.
    let helper = paths.bin().join(HELPER);
    copy_helper(downloaded, &helper)?;
    let pending = PendingFile {
        version: downloaded.version.to_string(),
        manifest_sha256: downloaded.manifest_sha256.clone(),
        update_dir: downloaded.dir.clone(),
        stage: Stage::Started,
        active_modules,
        added: Vec::new(),
        replaced: Vec::new(),
    };
    state::write(&paths.pending_file(), &pending)?;
    let launched = launcher.launch_helper(&helper, &paths.data);
    if launched.is_ok() {
        let started = Instant::now();
        while started.elapsed() < HELPER_START {
            if state::helper_alive(paths) {
                exit.notify_one();
                return Ok(());
            }
            sleep(POLL).await;
        }
    }
    // Even a failed launch may have registered the task, which runs again within 5 minutes.
    let removed = match state::try_lock(&paths.lock_file(), Duration::ZERO) {
        Ok(None) => {
            exit.notify_one(); // it started just now
            return Ok(());
        }
        Ok(Some(_lock)) => remove_stage_file(paths).await,
        Err(err) => Err(err),
    };
    if let Err(err) = removed {
        exit.notify_one();
        return Err(UpdateError::StageFileLeft(err));
    }
    launched.and(Err(UpdateError::HelperNotStarted))
}

/// Deletes `update-pending.json`, 5 tries 100 ms apart: an antivirus may hold it for a moment.
async fn remove_stage_file(paths: &Paths) -> io::Result<()> {
    let mut tries = 5;
    loop {
        match std::fs::remove_file(paths.pending_file()) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) if tries > 1 => {
                tries -= 1;
                sleep(Duration::from_millis(100)).await;
            }
            result => return result,
        }
    }
}

/// Copies the release's `om.exe` beside the helper's place, checks it against the release manifest
/// and moves it there; a copy that does not match leaves the previous helper in place.
fn copy_helper(downloaded: &Downloaded, helper: &Path) -> Result<(), UpdateError> {
    let manifest = read_manifest(downloaded)?;
    let entry = manifest.files.iter().find(|file| file.name == "om.exe").ok_or_else(|| UpdateError::Rejected("release has no om.exe".into()))?;
    let mut copy = helper.as_os_str().to_owned();
    copy.push(".tmp");
    let copy = PathBuf::from(copy);
    if let Some(bin) = helper.parent() {
        std::fs::create_dir_all(bin)?;
    }
    std::fs::copy(downloaded.dir.join("om.exe"), &copy)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut File::open(&copy)?, &mut hasher)?;
    let sum: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    if sum != entry.sha256 {
        let _ = std::fs::remove_file(&copy);
        return Err(UpdateError::Rejected("om.exe in the update folder does not match the release manifest".into()));
    }
    std::fs::rename(&copy, helper)?;
    Ok(())
}

/// The release manifest kept in the update folder, if it is still the one verified at download.
fn read_manifest(downloaded: &Downloaded) -> Result<Manifest, UpdateError> {
    // The manifest and its signature are at most 64 KiB (fetch.rs).
    const MAX_MANIFEST: u64 = 64 * 1024;
    let mut bytes = Vec::new();
    File::open(downloaded.dir.join("manifest.json"))?.take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
    if manifest_sha256(&bytes) != downloaded.manifest_sha256 {
        return Err(UpdateError::Rejected("the update folder's manifest changed after the download".into()));
    }
    serde_json::from_slice(&bytes).map_err(|err| UpdateError::Rejected(format!("manifest: {err}")))
}
