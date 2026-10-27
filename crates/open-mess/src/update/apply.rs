//! The update helper, `om.update.exe apply-update` (spec 2026-10-27-self-update, 7.2, 7.3, 7.6). The
//! scheduled task `open_mess-update` runs it when the daemon hands an update over, at logon and
//! every 5 minutes. It drives the stage machine in `update-pending.json`: with the user's consent it
//! stops the daemon, backs up the databases, the modules and `om.exe`, swaps `om.exe`, places the
//! modules, starts the new daemon and judges its health. A failed step, a failed health check or an
//! interrupted run found later rolls everything back from the backup. Every way out starts the
//! daemon again: open_mess is never left stopped.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use open_mess_host::Engine;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::UpdateError;
use super::consent;
use super::release::{Kind, Manifest, Version, manifest_sha256, verify};
use super::state::{self, Paths, PendingFile, Read, Stage};
use crate::modules::{self, ModuleState};

/// How long a helper tries for `update.lock` before it leaves the work to the one that holds it.
const LOCK_WAIT: Duration = Duration::from_secs(2);
/// How long the daemon gets to exit on its own once it launched the helper.
const DAEMON_EXIT: Duration = Duration::from_secs(60);
/// How long a stopped daemon gets to let go of `daemon.lock`.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// The manifest and its signature are at most 64 KiB (fetch.rs).
const MAX_MANIFEST: u64 = 64 * 1024;
/// How many `<name>-<version>-<n>.wasm` names are tried for a module file.
const MAX_NUMBERED: u32 = 100;
/// The backup's folder, in the update folder.
const BACKUP: &str = "backup";
const NO_CONSENT: &str = "brak ważnej zgody";
const UNREADABLE: &str = "update-pending.json is unreadable";

/// What the helper needs from Windows; tests fake it.
pub trait System {
    /// Whether the daemon's instance lock (`daemon.lock`) is free, or gets free within `wait`.
    fn wait_daemon_lock_free(&self, wait: Duration) -> bool;
    fn stop_daemon(&self) -> Result<(), UpdateError>;
    fn start_daemon(&self) -> Result<(), UpdateError>;
    /// Waits until the new daemon is up with every module of `active_modules` back (spec 7.2), or
    /// gives up.
    fn health(&self, active_modules: &[String]) -> Health;
}

/// How the new daemon came up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Failed(String),
}

/// How a run ended. All but `Nothing` go to `last-update.txt`, for the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No update to install or to finish, or another helper's turn: nothing was touched.
    Nothing,
    Installed,
    /// The previous version is back, for this reason.
    RolledBack(String),
    /// Stopped before anything was changed.
    Aborted(String),
    /// The rollback failed too; the reason names the backup folder.
    RollbackFailed(String),
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Nothing => Ok(()),
            Outcome::Installed => f.write_str("zainstalowano"),
            Outcome::RolledBack(why) => write!(f, "przywrócono: {why}"),
            Outcome::Aborted(why) => write!(f, "przerwano: {why}"),
            Outcome::RollbackFailed(why) => write!(f, "wycofanie nieudane: {why}"),
        }
    }
}

/// What one `status` answer says about a new daemon (spec 7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Judgement {
    Healthy,
    /// Not there yet. `accounts_only` when only the account restore is left: it gets longer.
    Pending {
        accounts_only: bool,
    },
    Failed(String),
}

/// Judges a `status` answer: the window and tray are ready and every module of `active_modules` is
/// active without a crash since it was loaded; then the account restore has finished. A crash, or a
/// module stopped or turned off, fails at once. The daemon's own check during the probation uses it
/// too.
pub fn judge(status: &Value, active_modules: &[String]) -> Judgement {
    let listed = status.get("modules").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let [active, waiting, crashed, disabled] =
        [ModuleState::Active, ModuleState::PendingReplacement, ModuleState::Crashed, ModuleState::Disabled].map(|state| state.to_string());
    let mut ready = status.get("ui_ready").and_then(Value::as_bool) == Some(true);
    for name in active_modules {
        let text = |module: &Value, key: &str| module.get(key).and_then(Value::as_str).map(str::to_owned).unwrap_or_default();
        // A file waiting to replace the module is not the module.
        let current = listed.iter().filter(|module| text(module, "name") == *name && text(module, "state") != waiting);
        let mut back = false;
        for module in current {
            let state = text(module, "state");
            if module.get("crashes_since_load").and_then(Value::as_u64).is_some_and(|crashes| crashes > 0) {
                return Judgement::Failed(format!("module {name} crashed"));
            }
            if state == crashed || state == disabled {
                return Judgement::Failed(format!("module {name}: {state}"));
            }
            back |= state == active;
        }
        ready &= back;
    }
    if !ready {
        Judgement::Pending { accounts_only: false }
    } else if status.get("accounts_restored").and_then(Value::as_bool) != Some(true) {
        Judgement::Pending { accounts_only: true }
    } else {
        Judgement::Healthy
    }
}

/// One run of the helper. `now` is the wall clock in milliseconds.
pub fn run(paths: &Paths, system: &dyn System, public_key: &str, now: fn() -> i64) -> Outcome {
    // The first thing, held to the end: "the helper is alive" (spec 7.2). Not taken: another helper
    // works, and this one touches nothing.
    let Ok(Some(_lock)) = state::try_lock(&paths.lock_file(), LOCK_WAIT) else {
        return Outcome::Nothing;
    };
    let outcome = match read(paths) {
        // Not read is not broken: everything stays as it is for the next run.
        Read::Missing | Read::Error(_) => return Outcome::Nothing,
        Read::Unreadable => unreadable(paths, system, now),
        Read::Found(pending) if pending.stage == Stage::Started => install(paths, system, pending, public_key, now),
        // Never finished forwards (spec 7.6).
        Read::Found(pending) => roll_back(paths, system, pending, "the update was interrupted".into(), now),
    };
    state::record_result(paths, &outcome.to_string());
    outcome
}

/// `update-pending.json`, as unreadable when it names a file outside the two folders an
/// installation adds files to: a rollback deletes every file in `added`.
fn read(paths: &Paths) -> Read {
    let (modules, bin) = (paths.data.join("modules"), paths.bin());
    let inside = |path: &PathBuf| path.file_name().is_some() && (path.parent() == Some(&*modules) || path.parent() == Some(&*bin));
    match state::read(&paths.pending_file()) {
        Read::Found(pending) if !pending.added.iter().chain(&pending.replaced).all(inside) => Read::Unreadable,
        read => read,
    }
}

/// A stage file that is not one (spec 7.2): rolled back like `installed` when there is a backup,
/// else deleted. The update it belonged to is this helper's own version, since the daemon puts the
/// release's `om.exe` in place of the helper before it writes the file; its backup is looked for
/// there.
fn unreadable(paths: &Paths, system: &dyn System, now: fn() -> i64) -> Outcome {
    let version = Version::current().to_string();
    let update_dir = paths.updates().join(&version);
    if !update_dir.join(BACKUP).is_dir() {
        return abort(paths, system, UNREADABLE.into());
    }
    let pending = PendingFile {
        version,
        // Unknown: the release is not remembered as rolled back.
        manifest_sha256: String::new(),
        update_dir,
        stage: Stage::RollingBack,
        active_modules: Vec::new(),
        added: Vec::new(),
        replaced: Vec::new(),
    };
    roll_back(paths, system, pending, UNREADABLE.into(), now)
}

/// Stage `started`: installs the update the user accepted.
fn install(paths: &Paths, system: &dyn System, mut pending: PendingFile, public_key: &str, now: fn() -> i64) -> Outcome {
    let consented = open_db(paths).and_then(|conn| consent::valid(&conn, &pending.version, &pending.manifest_sha256, now()));
    if !matches!(consented, Ok(true)) {
        return abort(paths, system, NO_CONSENT.into());
    }
    if !system.wait_daemon_lock_free(DAEMON_EXIT) {
        // Whether it worked shows in the lock.
        let _ = system.stop_daemon();
        if !system.wait_daemon_lock_free(STOP_WAIT) {
            return abort(paths, system, "the daemon did not stop".into());
        }
    }
    // The daemon is stopped: from here every way out starts it again.
    let backed_up = open_db(paths)
        .and_then(|conn| consent::mark_used(&conn, &pending.version, &pending.manifest_sha256, now()))
        .and_then(|()| reverify(&pending, public_key))
        .and_then(|manifest| back_up(paths, &pending.update_dir, now()).map(|()| manifest));
    let manifest = match backed_up {
        Ok(manifest) => manifest,
        Err(err) => return abort(paths, system, err.to_string()),
    };
    pending.stage = Stage::BackedUp;
    if let Err(err) = state::write(&paths.pending_file(), &pending) {
        return abort(paths, system, err.to_string());
    }
    if let Err(err) = put_in_place(paths, system, &mut pending, &manifest, now()) {
        return roll_back(paths, system, pending, err.to_string(), now);
    }
    match system.health(&pending.active_modules) {
        Health::Healthy => {
            // One left behind ends the daemon's probation all the same (spec 7.2).
            let _ = fs::remove_file(paths.pending_file());
            Outcome::Installed
        }
        Health::Failed(why) => roll_back(paths, system, pending, why, now),
    }
}

/// Swaps `om.exe`, places the modules and starts the new daemon (spec 7.2).
fn put_in_place(paths: &Paths, system: &dyn System, pending: &mut PendingFile, manifest: &Manifest, ms: i64) -> Result<(), UpdateError> {
    swap_exe(paths, pending, ms)?;
    place_modules(paths, pending, manifest)?;
    pending.stage = Stage::Installed;
    state::write(&paths.pending_file(), pending)?;
    system.start_daemon()
}

/// Ends a run that changed nothing: the stage file goes, and the daemon starts unless it runs.
fn abort(paths: &Paths, system: &dyn System, why: String) -> Outcome {
    let _ = fs::remove_file(paths.pending_file());
    Outcome::Aborted(if system.wait_daemon_lock_free(Duration::ZERO) { start(system, why) } else { why })
}

/// Starts the daemon; a failure is added to `why`, there is nothing else to do about it.
fn start(system: &dyn System, why: String) -> String {
    match system.start_daemon() {
        Ok(()) => why,
        Err(err) => format!("{why}; the daemon did not start: {err}"),
    }
}

/// Puts back everything the backup holds (spec 7.3). The stage is `rolling-back` first, so that a
/// run cut short starts over. Then the stage file goes and the daemon starts, whatever came of it.
fn roll_back(paths: &Paths, system: &dyn System, mut pending: PendingFile, why: String, now: fn() -> i64) -> Outcome {
    pending.stage = Stage::RollingBack;
    // Not written, the stage on disk means a rollback all the same.
    let _ = state::write(&paths.pending_file(), &pending);
    let _ = system.stop_daemon();
    // A file the daemon still holds fails to restore, and the result says so.
    let _ = system.wait_daemon_lock_free(STOP_WAIT);
    let backup = pending.update_dir.join(BACKUP);
    match restore(paths, &pending, &backup, now()) {
        Ok(()) => {
            if !pending.manifest_sha256.is_empty() {
                // Best effort: without it the release is only offered again.
                let _ = open_db(paths).and_then(|conn| consent::record_rolled_back(&conn, &pending.version, &pending.manifest_sha256));
            }
            let _ = fs::remove_file(paths.pending_file());
            // After the stage file, never before: a later unreadable stage file must not find this
            // backup and restore it again.
            let _ = fs::remove_dir_all(&backup);
            Outcome::RolledBack(start(system, why))
        }
        Err(failed) => {
            let _ = fs::remove_file(paths.pending_file());
            Outcome::RollbackFailed(start(system, format!("{why}; {failed}; kopia zapasowa: {}", backup.display())))
        }
    }
}

/// Deletes what the update added, copies every backed-up file back, and deletes the journals of the
/// restored databases that the backup does not have: SQLite would replay the new version's journal
/// onto the old database. Goes on past a failure, so that as much as can be is restored, and
/// returns the failures.
fn restore(paths: &Paths, pending: &PendingFile, backup: &Path, ms: i64) -> Result<(), String> {
    if !backup.is_dir() {
        return Err(format!("no backup in {}", backup.display()));
    }
    let mut failures = Vec::new();
    let mut check = |path: &Path, result: io::Result<()>| {
        if let Err(err) = result {
            failures.push(format!("{}: {err}", path.display()));
        }
    };
    // First, so that a backed-up file is always there at the end.
    for path in &pending.added {
        check(path, remove_if_present(path));
    }
    let exe = paths.bin().join("om.exe");
    check(&exe, put_back(&backup.join("bin").join("om.exe"), &exe, Some(&paths.bin().join(format!("om.failed-{ms}.exe")))));
    let data = backup.join("data");
    for (from, to) in [(data.clone(), paths.data.clone()), (data.join("modules"), paths.data.join("modules"))] {
        let names = match backed_up(&from) {
            Ok(names) => names,
            Err(err) => {
                check(&from, Err(err));
                continue;
            }
        };
        for name in &names {
            check(&to.join(name), put_back(&from.join(name), &to.join(name), None));
        }
        for database in names.iter().filter(|name| has_suffix(name, ".sqlite3")) {
            for suffix in ["-journal", "-wal", "-shm"] {
                let mut journal = database.clone();
                journal.push(suffix);
                if !names.contains(&journal) {
                    let path = to.join(&journal);
                    check(&path, remove_if_present(&path));
                }
            }
        }
    }
    if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
}

/// Copies the databases with their journals, the module files and `om.exe` to `<update>\backup`
/// (spec 7), through `backup.partial`: a backup folder is always a whole one.
fn back_up(paths: &Paths, update_dir: &Path, ms: i64) -> Result<(), UpdateError> {
    let backup = update_dir.join(BACKUP);
    if backup.exists() {
        // An earlier attempt's, perhaps the only copy after a failed rollback: kept aside, never
        // mixed into this one.
        fs::rename(&backup, update_dir.join(format!("{BACKUP}-{ms}")))?;
    }
    let partial = update_dir.join("backup.partial");
    if partial.exists() {
        fs::remove_dir_all(&partial)?;
    }
    fs::create_dir_all(partial.join("bin"))?;
    fs::copy(paths.bin().join("om.exe"), partial.join("bin").join("om.exe"))?;
    let data = partial.join("data");
    for (from, to) in [(paths.data.clone(), data.clone()), (paths.data.join("modules"), data.join("modules"))] {
        fs::create_dir_all(&to)?;
        let names = match backed_up(&from) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            names => names?,
        };
        for name in names {
            fs::copy(from.join(&name), to.join(&name))?;
        }
    }
    fs::rename(&partial, &backup)?;
    Ok(())
}

/// The files of `dir` a backup keeps: the databases with their journals, and the module files.
fn backed_up(dir: &Path) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_file() && [".sqlite3", ".sqlite3-journal", ".sqlite3-wal", ".wasm"].iter().any(|end| has_suffix(&name, end)) {
            names.push(name);
        }
    }
    Ok(names)
}

fn has_suffix(name: &OsStr, suffix: &str) -> bool {
    name.to_string_lossy().to_ascii_lowercase().ends_with(suffix)
}

/// Copies `from` next to `to` and renames it over `to`. A program in use can be renamed but not
/// replaced: with `aside`, a `to` that refuses is first renamed there.
fn put_back(from: &Path, to: &Path, aside: Option<&Path>) -> io::Result<()> {
    let temp = with_suffix(to, ".tmp");
    fs::copy(from, &temp)?;
    match (fs::rename(&temp, to), aside) {
        (Err(_), Some(aside)) if to.exists() => {
            fs::rename(to, aside)?;
            fs::rename(&temp, to)
        }
        (renamed, _) => renamed,
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The release in the update folder, checked again (spec 7.2): signed with `public_key`, the one
/// the user accepted, and every file as the manifest lists it.
fn reverify(pending: &PendingFile, public_key: &str) -> Result<Manifest, UpdateError> {
    let bytes = read_capped(&pending.update_dir.join("manifest.json"))?;
    let signature = String::from_utf8(read_capped(&pending.update_dir.join("manifest.json.minisig"))?)
        .map_err(|_| UpdateError::Rejected("signature is not text".into()))?;
    let manifest = verify(&bytes, &signature, public_key)?;
    if manifest_sha256(&bytes) != pending.manifest_sha256 || manifest.version != pending.version {
        return Err(UpdateError::Rejected("the update folder holds another release than the one accepted".into()));
    }
    for file in &manifest.files {
        let mut hasher = Sha256::new();
        let size = io::copy(&mut File::open(pending.update_dir.join(&file.name))?.take(file.size.saturating_add(1)), &mut hasher)?;
        let sum: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        if size != file.size || sum != file.sha256 {
            return Err(UpdateError::Rejected(format!("{} does not match the release manifest", file.name)));
        }
    }
    Ok(manifest)
}

fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(io::Error::other(format!("{} is larger than 64 KiB", path.display())));
    }
    Ok(bytes)
}

/// The state database as it is: never created, never migrated (spec 7.2).
fn open_db(paths: &Paths) -> Result<Connection, UpdateError> {
    let conn = Connection::open_with_flags(paths.data.join("state.sqlite3"), OpenFlags::default().difference(OpenFlags::SQLITE_OPEN_CREATE))?;
    // A daemon that has not exited yet may be writing.
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// Records a file the installation is about to add, before it is there: a rollback deletes it.
fn add(paths: &Paths, pending: &mut PendingFile, path: PathBuf) -> Result<(), UpdateError> {
    pending.added.push(path);
    state::write(&paths.pending_file(), pending)?;
    Ok(())
}

/// Puts the release's `om.exe` in place. A bridge (`om mcp-stdio`) may still run the current one,
/// which Windows lets rename but not replace: it moves aside under a name of its own.
fn swap_exe(paths: &Paths, pending: &mut PendingFile, ms: i64) -> Result<(), UpdateError> {
    let bin = paths.bin();
    let new = bin.join("om.new.exe");
    add(paths, pending, new.clone())?;
    fs::copy(pending.update_dir.join("om.exe"), &new)?;
    fs::rename(bin.join("om.exe"), bin.join(format!("om.old-{ms}.exe")))?;
    fs::rename(&new, bin.join("om.exe"))?;
    Ok(())
}

/// Puts each module of the release in the modules folder (spec 7.2, 7.4). Its predecessor is the
/// approved file with the same manifest name. When only the manifest's `version` changed, the new
/// file replaces it under its file name and is approved as the user would; else it goes next to it
/// as `<name>-<version>.wasm`, or as `<name>.wasm` without a predecessor, and waits for the user's
/// approval. A new file never takes a name already there: `<name>-<version>-<n>.wasm` then.
fn place_modules(paths: &Paths, pending: &mut PendingFile, manifest: &Manifest) -> Result<(), UpdateError> {
    let names: Vec<&str> = manifest.files.iter().filter(|file| file.kind == Kind::Module).map(|file| file.name.as_str()).collect();
    if names.is_empty() {
        return Ok(());
    }
    let dir = paths.data.join("modules");
    fs::create_dir_all(&dir)?;
    let conn = open_db(paths)?;
    let engine = Engine::new().map_err(other)?;
    // Reading a manifest runs the module under the host, which needs a runtime.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut approved = Vec::new();
        for path in modules::module_files(&dir)? {
            // A file that is no module, or not the approved one, is nobody's predecessor.
            if let Ok(found) = modules::inspect(&engine, &path).await
                && modules::is_approved(&conn, &found).map_err(other)?
            {
                approved.push((path, found));
            }
        }
        for name in names {
            let source = pending.update_dir.join(name);
            let new = modules::inspect(&engine, &source).await.map_err(|err| UpdateError::Rejected(err.to_string()))?;
            let predecessor = approved.iter().find(|(_, old)| old.manifest.name == new.manifest.name);
            if let Some((path, _)) = predecessor.filter(|(_, old)| modules::version_only_change(&old.manifest, &new.manifest)) {
                // Copied next to it and renamed over it: atomic within the folder.
                pending.replaced.push(path.clone());
                let temp = with_suffix(path, ".new");
                add(paths, pending, temp.clone())?;
                fs::copy(&source, &temp)?;
                fs::rename(&temp, path)?;
                modules::approve(&conn, &new.manifest.name, &new.file_sha256, &new.manifest_sha256).map_err(other)?;
                modules::remember_approved(&conn, &new.manifest).map_err(other)?;
            } else {
                let stem = name.strip_suffix(".wasm").unwrap_or(name);
                let first = if predecessor.is_some() { format!("{stem}-{}.wasm", pending.version) } else { name.to_owned() };
                let target = free_name(&dir, first, stem, &pending.version)?;
                add(paths, pending, target.clone())?;
                fs::copy(&source, &target)?;
            }
        }
        Ok(())
    })
}

/// `first` in `dir` when it is free, else the first free `<stem>-<version>-<n>.wasm`.
fn free_name(dir: &Path, first: String, stem: &str, version: &str) -> Result<PathBuf, UpdateError> {
    std::iter::once(first)
        .chain((1..=MAX_NUMBERED).map(|n| format!("{stem}-{version}-{n}.wasm")))
        .map(|name| dir.join(name))
        .find(|path| !path.exists())
        .ok_or_else(|| UpdateError::Rejected(format!("no free file name for {stem}.wasm")))
}

fn other(err: impl fmt::Display) -> UpdateError {
    UpdateError::Io(io::Error::other(err.to_string()))
}
