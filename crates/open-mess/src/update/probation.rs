//! The daemon's side of an update in progress (spec 2026-10-27-self-update, 7.2, 7.5, 7.6). At
//! start, before anything else, the daemon looks at `update-pending.json`: it makes way for a helper
//! that changes the files, starts the helper again when it died, or runs in probation while the
//! helper checks this new version. In probation nothing the rollback could undo is done: no writes,
//! no saves from the window, no trigger deliveries. When the helper is gone the daemon judges itself
//! the way the helper would. After a successful installation it cleans up what earlier ones left.

use std::fs::{self, File};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Notify;
use tokio::time::{Instant, sleep};

use super::UpdateError;
use super::apply::{Judgement, Outcome, judge};
use super::quiesce::{self, Activity, Launcher};
use super::release::Version;
use super::state::{self, Paths, Read, Stage};
use crate::tools::GateMode;

/// How often the probation looks at the stage file and the helper.
const POLL: Duration = Duration::from_secs(1);
/// What the new version gets for everything but the account restore (spec 7.2).
const READY: Duration = Duration::from_secs(60);
/// What the account restore gets, from the daemon's start.
const ACCOUNTS: Duration = Duration::from_secs(10 * 60);

/// Whether a new version is being checked: on while `update-pending.json` is. The window's saves
/// and the decisions on waiting actions are refused meanwhile. Clones share it.
#[derive(Clone, Debug, Default)]
pub struct Flag(Arc<AtomicBool>);

impl Flag {
    pub fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }
}

/// What a starting daemon does about an update in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    Normal,
    /// The helper is changing the files, or the stage file could not be read: the daemon exits
    /// before it opens anything.
    Exit,
    /// The helper checks this new version: the daemon starts in probation, [`watch`].
    Probation,
    /// The helper died: its task starts it again, and it rolls back what it finds (spec 7.6).
    LaunchHelperAndExit,
}

/// Decides, before the daemon takes its lock or opens its database (spec 7.6).
pub fn on_start(paths: &Paths) -> Start {
    match state::read(&paths.pending_file()) {
        Read::Missing => Start::Normal,
        // Not read is not broken: the helper, or the next start 5 minutes later, decides.
        Read::Error(_) => Start::Exit,
        _ if !state::helper_alive(paths) => Start::LaunchHelperAndExit,
        Read::Found(file) if file.stage == Stage::Installed => Start::Probation,
        // Changing the files, or a file only the helper knows what to do with.
        Read::Unreadable | Read::Found(_) => Start::Exit,
    }
}

/// Starts the helper's scheduled task.
pub fn launch_helper(paths: &Paths, launcher: &dyn Launcher) -> Result<(), UpdateError> {
    launcher.launch_helper(&paths.bin().join(quiesce::HELPER), &paths.data)
}

/// The probation, until it ends: while `update-pending.json` exists, writes and the window's saves
/// are refused and trigger deliveries are held. Every second it takes `update.lock` if it is free,
/// then reads the file again:
/// - gone: the helper found this version healthy; everything is let go, and the old files go;
/// - `installed`, helper alive: the helper is checking;
/// - `installed`, helper gone: the daemon judges its own `status` as the helper would, keeping
///   `update.lock` so that a helper the task starts meanwhile leaves the check to it. Healthy: it
///   deletes the file, records "zainstalowano" and lets everything go. Failed, or not there in time
///   (60 s from `started`, 10 minutes for the accounts): the daemon starts the helper's task, which
///   rolls back, and notifies `exit`;
/// - any other stage, or a file that does not parse: the helper is changing the files, or must. The
///   daemon starts the helper again if it died, and notifies `exit` so that it can work (spec 7.6).
///
/// Writes stay refused when it notifies `exit`: the daemon ends with nothing new to roll back.
pub async fn watch(paths: &Paths, activity: &Activity, launcher: &dyn Launcher, status: impl Fn() -> Value, started: Instant, exit: &Notify) {
    hold(activity, true);
    let mut lock: Option<File> = None;
    loop {
        sleep(POLL).await;
        if lock.is_none() {
            lock = state::try_lock(&paths.lock_file(), Duration::ZERO).ok().flatten();
        }
        // Read after the lock: the stage the helper left when it let go, not one seen before.
        let file = match state::read(&paths.pending_file()) {
            Read::Missing => break,
            // Not read: looked at again.
            Read::Error(_) => continue,
            Read::Found(file) if file.stage == Stage::Installed => file,
            Read::Unreadable | Read::Found(_) => return make_way(paths, launcher, lock, exit),
        };
        if lock.is_none() {
            continue; // the helper is checking
        }
        match judge(&status(), &file.version, &file.active_modules) {
            Judgement::Healthy => {
                // Not deleted: tried again in a second, the lock still held.
                if fs::remove_file(paths.pending_file()).is_ok() {
                    state::record_result(paths, &Outcome::Installed.to_string());
                    // As the helper does: a backup of a finished installation is never restored.
                    // The version is this daemon's own: `judge` compared it.
                    let update_dir = paths.updates().join(&file.version);
                    let _ = fs::rename(update_dir.join("backup"), update_dir.join(format!("backup-{}", crate::pending::now_ms())));
                    break;
                }
            }
            Judgement::Pending { accounts_only } if started.elapsed() < (if accounts_only { ACCOUNTS } else { READY }) => {}
            Judgement::Pending { .. } | Judgement::Failed(_) => return make_way(paths, launcher, lock, exit),
        }
    }
    drop(lock);
    hold(activity, false);
    let (paths, keep) = (paths.clone(), Version::current());
    let _ = tokio::task::spawn_blocking(move || cleanup(&paths, &keep)).await;
}

/// Lets go of `update.lock`, starts the helper's task unless a helper is alive, and ends the daemon.
fn make_way(paths: &Paths, launcher: &dyn Launcher, lock: Option<File>, exit: &Notify) {
    drop(lock);
    if !state::helper_alive(paths) {
        // ponytail: a failed launch is left to the task's own 5-minute trigger, as is a launch the
        // task ignores while its previous run ends (IgnoreNew).
        let _ = launch_helper(paths, launcher);
    }
    exit.notify_one();
}

/// Refuses writes and the window's saves and holds the trigger deliveries, or lets them go.
fn hold(activity: &Activity, on: bool) {
    activity.gate.set(if on { GateMode::WritesBlocked } else { GateMode::Open });
    // send_replace: a `send` changes nothing while no engine listens.
    activity.triggers.hold.send_replace(on);
    activity.pending.probation().set(on);
}

/// After a successful installation of `keep` (spec 7.5): the update folders of older versions, and
/// the programs earlier installations and `scripts/install.ps1` left in `bin`. A file in use (an old
/// `om.exe` a bridge still runs) stays until the next time. Kept: `keep`'s folder with its backup,
/// `om.update.exe` for the recovery, and every folder with a `backup` in it: a failed rollback left
/// the user's data there.
pub fn cleanup(paths: &Paths, keep: &Version) {
    for entry in fs::read_dir(paths.updates()).into_iter().flatten().flatten() {
        let older = entry.file_name().to_str().and_then(|name| Version::parse(name).ok()).is_some_and(|version| version < *keep);
        let dir = entry.path();
        if older && entry.file_type().is_ok_and(|kind| kind.is_dir()) && !dir.join("backup").exists() {
            let _ = fs::remove_dir_all(dir);
        }
    }
    for entry in fs::read_dir(paths.bin()).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let numbered = |prefix: &str| name.strip_prefix(prefix).is_some_and(|rest| rest.ends_with(".exe"));
        let left = matches!(name, "om.new.exe" | "om.old.exe") || numbered("om.old-") || numbered("om.failed-");
        if left && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}
