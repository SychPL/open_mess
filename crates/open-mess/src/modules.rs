//! The modules folder (spec 8.3): every `.wasm` file in `<data_dir>/modules/`, the user's approval of
//! each module, and a running module for each approved one.
//!
//! A module runs only when both its file and its manifest are the ones the user approved (SHA-256 of
//! the file and of the canonical manifest JSON). Its manifest is read to find its name, which the
//! host does with no network, storage or secrets. One file per manifest name is the module; another
//! file with the same name waits as its replacement (spec 8.3, R6 and R7). The folder is read at
//! daemon start and again on request from the configuration window; a running module keeps running
//! until the file that replaces it is approved.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{Engine, Manifest, ModuleContext, ModuleHandle, Net, SignInPrompts};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::Error;
use crate::store::Store;

/// Why a module waits for the user's approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalReason {
    /// Never approved.
    New,
    /// The manifest is not the approved one: other hosts, sign-in identity or tools.
    ManifestChanged,
    /// The approved manifest in another file: a rebuild, a downgrade or a changed file.
    FileChanged,
}

/// What a module file is to the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleState {
    Active,
    AwaitingApproval {
        reason: ApprovalReason,
    },
    /// Another file with the same manifest name is the module; this one waits to replace it.
    PendingReplacement,
    /// Turned off by the user.
    Disabled,
    /// Stopped by the host after repeated crashes.
    Crashed,
    /// Not a platform module; the entry is named after its file.
    Invalid(String),
}

impl fmt::Display for ModuleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleState::Active => f.write_str("active"),
            ModuleState::AwaitingApproval { reason: ApprovalReason::New } => f.write_str("awaiting approval (new module)"),
            ModuleState::AwaitingApproval { reason: ApprovalReason::ManifestChanged } => f.write_str("awaiting approval (manifest changed)"),
            ModuleState::AwaitingApproval { reason: ApprovalReason::FileChanged } => f.write_str("awaiting approval (file changed)"),
            ModuleState::PendingReplacement => f.write_str("pending replacement"),
            ModuleState::Disabled => f.write_str("disabled"),
            ModuleState::Crashed => f.write_str("stopped after repeated crashes"),
            ModuleState::Invalid(why) => write!(f, "not a valid module: {why}"),
        }
    }
}

/// One module file, as `status` and the configuration window show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleInfo {
    /// The manifest name; the file name for an invalid file.
    pub name: String,
    pub file: String,
    pub version: String,
    pub platform: String,
    pub hosts: Vec<String>,
    pub state: ModuleState,
    /// What [`approve`] takes to approve this file.
    pub file_sha256: String,
    pub manifest_sha256: String,
    /// All of it, for the approval; None for an invalid file.
    pub manifest: Option<Manifest>,
}

struct Entry {
    info: ModuleInfo,
    /// Kept only for the active file of an approved module.
    handle: Option<ModuleHandle>,
}

impl Entry {
    fn state(&self) -> ModuleState {
        match &self.handle {
            Some(handle) if handle.state() == open_mess_host::ModuleState::Disabled => ModuleState::Crashed,
            _ => self.info.state.clone(),
        }
    }

    /// The module's file, not a file waiting to replace it.
    fn is_current(&self, name: &str) -> bool {
        self.info.name == name && self.info.state != ModuleState::PendingReplacement
    }
}

/// The module files and the running modules, and how to read the folder again.
pub struct Modules {
    entries: RwLock<Vec<Entry>>,
    engine: Engine,
    dir: PathBuf,
    /// The context for loading a file, by file name.
    ctx: Box<dyn Fn(&str) -> ModuleContext + Send + Sync>,
}

/// A file that loaded as a module.
struct Candidate {
    file: String,
    file_sha256: String,
    manifest: Manifest,
    manifest_sha256: String,
    handle: ModuleHandle,
}

/// What one reading of the folder found: the modules by manifest name, and the files that are not
/// modules.
pub(crate) struct Found {
    invalid: Vec<Entry>,
    by_name: BTreeMap<String, Vec<Candidate>>,
}

impl Modules {
    /// Reads every `.wasm` file in `dir` (created if missing) and keeps the approved modules
    /// running. `ctx` gives the context for loading a file, by file name.
    pub async fn scan(
        store: &Store,
        engine: &Engine,
        dir: &Path,
        ctx: impl Fn(&str) -> ModuleContext + Send + Sync + 'static,
    ) -> Result<Modules, Error> {
        let found = load(engine, dir, &ctx).await?;
        let entries = decide(store, found)?;
        Ok(Modules { entries: RwLock::new(entries), engine: engine.clone(), dir: dir.to_path_buf(), ctx: Box::new(ctx) })
    }

    fn read(&self) -> RwLockReadGuard<'_, Vec<Entry>> {
        self.entries.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Vec<Entry>> {
        self.entries.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn state(&self, name: &str) -> Option<ModuleState> {
        self.read().iter().find(|entry| entry.is_current(name)).map(Entry::state)
    }

    /// The running module, only when it is active.
    pub fn handle(&self, name: &str) -> Option<ModuleHandle> {
        self.read()
            .iter()
            .find(|entry| entry.is_current(name))
            .filter(|entry| entry.state() == ModuleState::Active)
            .and_then(|entry| entry.handle.clone())
    }

    /// Every loaded module: the active ones.
    pub fn handles(&self) -> Vec<ModuleHandle> {
        self.read().iter().filter_map(|entry| entry.handle.clone()).collect()
    }

    pub fn list(&self) -> Vec<ModuleInfo> {
        self.read().iter().map(|entry| ModuleInfo { state: entry.state(), ..entry.info.clone() }).collect()
    }

    /// SHA-256 of a module file as it is on disk now; None when it can't be read. Blocking.
    pub fn file_sha256(&self, file: &str) -> Option<String> {
        read_capped(&self.dir.join(file)).ok().map(|bytes| sha256_hex(&bytes))
    }

    /// Reads the folder again, without changing what runs ([`Modules::adopt`] does).
    pub(crate) async fn load(&self) -> Result<Found, Error> {
        load(&self.engine, &self.dir, &*self.ctx).await
    }

    /// Takes what [`Modules::load`] found. A loaded module goes on as it is, sessions and all,
    /// until the user turns it off or approves a file that replaces it: a new file waiting for
    /// approval, a file that is gone or one that does not load (still being written, say) leave it
    /// running (spec 8.3). A module stopped after repeated crashes starts again only when `restart`
    /// names it. Returns the modules that stopped, started or were replaced.
    pub(crate) fn adopt(&self, store: &Store, found: Found, restart: Option<&str>) -> Result<Vec<String>, Error> {
        let mut fresh = decide(store, found)?;
        let mut entries = self.write();
        let mut changed = BTreeSet::new();
        let mut kept = BTreeSet::new();
        for old in entries.drain(..).filter(|old| old.handle.is_some()) {
            let name = old.info.name.clone();
            let enabled = Row::get(store.conn(), &name)?.is_some_and(|row| row.enabled);
            let crashed = old.state() == ModuleState::Crashed;
            let at = fresh.iter().position(|entry| entry.is_current(&name) && entry.info.manifest.is_some());
            let keep = match at.and_then(|at| fresh.get(at)) {
                _ if !enabled => false,
                _ if crashed && restart == Some(name.as_str()) => false,
                Some(current) if current.info.state == ModuleState::Active => {
                    let same = current.info.file_sha256 == old.info.file_sha256 && current.info.manifest_sha256 == old.info.manifest_sha256;
                    // Another approved file replaces it; a crashed one stays stopped.
                    same || crashed
                }
                _ => true,
            };
            if !keep {
                changed.insert(name);
                continue;
            }
            kept.insert(name);
            match at.and_then(|at| fresh.get_mut(at).map(|current| (at, current))) {
                // The copy loaded now is dropped.
                Some((_, current)) if current.info.state == ModuleState::Active => *current = old,
                Some((at, current)) => {
                    current.info.state = ModuleState::PendingReplacement;
                    fresh.insert(at, old);
                }
                None => fresh.push(old),
            }
        }
        for entry in fresh.iter().filter(|entry| entry.info.state == ModuleState::Active && !kept.contains(&entry.info.name)) {
            changed.insert(entry.info.name.clone());
        }
        *entries = fresh;
        Ok(changed.into_iter().collect())
    }
}

/// Loads every `.wasm` file in `dir`, reading each module's manifest.
async fn load(engine: &Engine, dir: &Path, ctx: &(dyn Fn(&str) -> ModuleContext + Send + Sync)) -> Result<Found, Error> {
    std::fs::create_dir_all(dir)?;
    let paths = module_files(dir)?;

    let mut invalid = Vec::new();
    let mut by_name: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    for path in paths {
        let file = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        match load_file(engine, &path, ctx(&file)).await {
            (file_sha256, Ok((handle, manifest))) => {
                let manifest_sha256 = manifest_sha256(&manifest);
                by_name.entry(manifest.name.clone()).or_default().push(Candidate { file, file_sha256, manifest, manifest_sha256, handle });
            }
            (file_sha256, Err(why)) => invalid.push(invalid_entry(file, file_sha256, why)),
        }
    }
    Ok(Found { invalid, by_name })
}

/// The `.wasm` files in `dir`, sorted.
pub(crate) fn module_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("wasm")) && path.is_file())
        .collect();
    paths.sort();
    Ok(paths)
}

/// Loads one module file: its SHA-256 (empty when it cannot be read), and the module with its
/// manifest or why it is not one.
async fn load_file(engine: &Engine, path: &Path, ctx: ModuleContext) -> (String, Result<(ModuleHandle, Manifest), String>) {
    // The bytes that are hashed are the bytes that are loaded: a file swapped in between never
    // runs under the other file's approval.
    let bytes = match read_capped(path) {
        Ok(bytes) => bytes,
        Err(why) => return (String::new(), Err(why)),
    };
    let file_sha256 = sha256_hex(&bytes);
    let loaded = match engine.load_bytes(bytes, ctx).await {
        Ok(handle) => handle.manifest().map(|manifest| (handle, manifest)).map_err(|err| err.to_string()),
        Err(err) => Err(err.to_string()),
    };
    (file_sha256, loaded)
}

/// A module file as the approvals see it.
pub(crate) struct Inspected {
    pub(crate) file_sha256: String,
    pub(crate) manifest: Manifest,
    pub(crate) manifest_sha256: String,
}

/// Reads a module file's manifest and hashes, without running the module: it gets no network,
/// storage or events, and is dropped at once. For the update helper (spec 2026-10-27-self-update,
/// 7.2).
pub(crate) async fn inspect(engine: &Engine, path: &Path) -> Result<Inspected, Error> {
    let ctx = ModuleContext {
        // Never opened: storage opens on a call, and no call reaches this module.
        data_dir: PathBuf::new(),
        events: mpsc::channel(1).0,
        net: Net::new(AddressPolicy::Strict),
        sign_in: SignInPrompts::new(mpsc::channel(1).0),
    };
    let (file_sha256, loaded) = load_file(engine, path, ctx).await;
    let (_, manifest) = loaded.map_err(|why| Error::Io(std::io::Error::other(format!("{}: {why}", path.display()))))?;
    Ok(Inspected { manifest_sha256: manifest_sha256(&manifest), file_sha256, manifest })
}

/// Whether two manifests differ at most in `version` (spec 2026-10-27-self-update, 7.4).
pub(crate) fn version_only_change(a: &Manifest, b: &Manifest) -> bool {
    let without_version = |manifest: &Manifest| {
        let mut json = canonical(manifest);
        if let Some(fields) = json.as_object_mut() {
            fields.remove("version");
        }
        json
    };
    without_version(a) == without_version(b)
}

/// Whether this is the file and manifest the user approved for its module.
pub(crate) fn is_approved(conn: &Connection, inspected: &Inspected) -> Result<bool, Error> {
    Ok(Row::get(conn, &inspected.manifest.name)?.is_some_and(|row| {
        row.approved_file_sha256.as_deref() == Some(inspected.file_sha256.as_str())
            && row.approved_manifest_sha256.as_deref() == Some(inspected.manifest_sha256.as_str())
    }))
}

/// Which file is each module and what state it is in, by the approvals in `store`.
fn decide(store: &Store, found: Found) -> Result<Vec<Entry>, Error> {
    let Found { mut invalid, by_name } = found;
    let mut entries = Vec::new();
    entries.append(&mut invalid);
    for (name, candidates) in by_name {
        let row = Row::get(store.conn(), &name)?;
        let approved = |c: &Candidate| {
            row.as_ref().is_some_and(|row| {
                row.approved_file_sha256.as_deref() == Some(c.file_sha256.as_str())
                    && row.approved_manifest_sha256.as_deref() == Some(c.manifest_sha256.as_str())
            })
        };
        // The approved file is the module; else the file it was last time; else the first.
        let current = candidates
            .iter()
            .position(approved)
            .or_else(|| candidates.iter().position(|c| row.as_ref().is_some_and(|row| row.file == c.file)))
            .unwrap_or(0);
        for (index, candidate) in candidates.into_iter().enumerate() {
            let state = if index != current {
                ModuleState::PendingReplacement
            } else if approved(&candidate) {
                if row.as_ref().is_some_and(|row| row.enabled) { ModuleState::Active } else { ModuleState::Disabled }
            } else {
                let reason = match row.as_ref().and_then(|row| row.approved_manifest_sha256.as_deref()) {
                    None => ApprovalReason::New,
                    Some(approved) if approved != candidate.manifest_sha256 => ApprovalReason::ManifestChanged,
                    Some(_) => ApprovalReason::FileChanged,
                };
                ModuleState::AwaitingApproval { reason }
            };
            if index == current {
                record(store, &name, &candidate)?;
            }
            let Candidate { file, file_sha256, manifest, manifest_sha256, handle } = candidate;
            // Dropping the only handle stops the module.
            let handle = (state == ModuleState::Active).then_some(handle);
            let info = ModuleInfo {
                name: manifest.name.clone(),
                file,
                version: manifest.version.clone(),
                platform: manifest.platform.clone(),
                hosts: manifest.hosts.clone(),
                state,
                file_sha256,
                manifest_sha256,
                manifest: Some(manifest),
            };
            entries.push(Entry { info, handle });
        }
    }
    Ok(entries)
}

/// The largest module file read; the Teams module is a few MiB.
const MAX_MODULE_FILE: u64 = 64 * 1024 * 1024;

fn read_capped(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).and_then(|file| file.take(MAX_MODULE_FILE + 1).read_to_end(&mut bytes)).map_err(|err| err.to_string())?;
    if bytes.len() as u64 > MAX_MODULE_FILE {
        return Err(format!("the file is larger than {} MiB", MAX_MODULE_FILE / 1024 / 1024));
    }
    Ok(bytes)
}

fn invalid_entry(file: String, file_sha256: String, why: String) -> Entry {
    let info = ModuleInfo {
        name: file.clone(),
        file,
        version: String::new(),
        platform: String::new(),
        hosts: Vec::new(),
        state: ModuleState::Invalid(why),
        file_sha256,
        manifest_sha256: String::new(),
        manifest: None,
    };
    Entry { info, handle: None }
}

/// A module's row in the `modules` table.
struct Row {
    file: String,
    approved_file_sha256: Option<String>,
    approved_manifest_sha256: Option<String>,
    enabled: bool,
}

impl Row {
    fn get(conn: &Connection, name: &str) -> Result<Option<Row>, Error> {
        let row = conn
            .query_row("SELECT file, approved_file_sha256, approved_manifest_sha256, enabled FROM modules WHERE name = ?1", [name], |row| {
                Ok(Row { file: row.get(0)?, approved_file_sha256: row.get(1)?, approved_manifest_sha256: row.get(2)?, enabled: row.get(3)? })
            })
            .optional()?;
        Ok(row)
    }
}

/// Records which file is the module now, keeping the approval and the enabled flag.
fn record(store: &Store, name: &str, current: &Candidate) -> Result<(), Error> {
    store.conn().execute(
        "INSERT INTO modules (name, file, file_sha256, manifest_sha256) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (name) DO UPDATE SET file = excluded.file, file_sha256 = excluded.file_sha256, manifest_sha256 = excluded.manifest_sha256",
        [name, &current.file, &current.file_sha256, &current.manifest_sha256],
    )?;
    Ok(())
}

/// Approves a module file and manifest; the next scan runs the file with these hashes. Only the
/// configuration window, the update helper (a version-only change from a signed release, spec
/// 2026-10-27-self-update 7.4) and tests call it: no MCP or CLI tool approves anything. Takes a plain
/// connection: the helper never migrates the database.
pub fn approve(conn: &Connection, name: &str, file_sha256: &str, manifest_sha256: &str) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO modules (name, file, file_sha256, manifest_sha256, approved_file_sha256, approved_manifest_sha256)
         VALUES (?1, '', ?2, ?3, ?2, ?3)
         ON CONFLICT (name) DO UPDATE SET approved_file_sha256 = excluded.approved_file_sha256,
             approved_manifest_sha256 = excluded.approved_manifest_sha256",
        [name, file_sha256, manifest_sha256],
    )?;
    Ok(())
}

/// Turns a module on or off from the next scan. For the configuration window and tests.
pub fn set_enabled(store: &Store, name: &str, enabled: bool) -> Result<(), Error> {
    store.conn().execute(
        "INSERT INTO modules (name, file, file_sha256, manifest_sha256, enabled) VALUES (?1, '', '', '', ?2)
         ON CONFLICT (name) DO UPDATE SET enabled = excluded.enabled",
        (name, enabled),
    )?;
    Ok(())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// SHA-256 of the manifest as JSON with sorted keys (serde_json's map is ordered by key).
fn manifest_sha256(manifest: &Manifest) -> String {
    sha256_hex(canonical(manifest).to_string().as_bytes())
}

/// The manifest as JSON, as it is hashed and remembered.
pub fn canonical(manifest: &Manifest) -> Value {
    let tools: Vec<_> = manifest
        .extra_tools
        .iter()
        .map(|tool| json!({"description": tool.description, "input_schema": tool.input_schema, "name": tool.name, "writes": tool.writes}))
        .collect();
    json!({
        "extra_tools": tools,
        "hosts": manifest.hosts,
        "name": manifest.name,
        "platform": manifest.platform,
        "sign_in_hosts": manifest.sign_in_hosts,
        "version": manifest.version,
        "wam": manifest.wam.as_ref().map(|wam| json!({"client_id": wam.client_id, "scopes": wam.scopes})),
    })
}

fn approved_manifest_key(name: &str) -> String {
    format!("approved_manifest:{name}")
}

/// Remembers the manifest the user approved for its module, so that the approval of a later file
/// can show what changed even when no version of the module runs.
pub fn remember_approved(conn: &Connection, manifest: &Manifest) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO config (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [approved_manifest_key(&manifest.name), canonical(manifest).to_string()],
    )?;
    Ok(())
}

fn sign_in_hosts_key(name: &str) -> String {
    format!("sign_in_hosts:{name}")
}

/// The hosts the user allowed for module `name`'s sign-in pages during a sign-in (spec 8.3, R10).
fn allowed_sign_in_hosts(store: &Store, name: &str) -> Result<Vec<String>, Error> {
    Ok(store.get_config(&sign_in_hosts_key(name))?.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default())
}

/// The approved sign-in hosts of a module: its manifest's and the ones the user allowed.
pub fn approved_sign_in_hosts(store: &Store, manifest: &Manifest) -> Result<Vec<String>, Error> {
    let mut hosts = manifest.sign_in_hosts.clone();
    hosts.extend(allowed_sign_in_hosts(store, &manifest.name)?);
    Ok(hosts)
}

/// Adds a host to module `name`'s approved sign-in hosts, when the user allows its sign-in page to
/// go there (spec 8.3, R10). Only the configuration window calls it.
pub fn allow_sign_in_host(store: &Store, name: &str, host: &str) -> Result<(), Error> {
    let mut hosts = allowed_sign_in_hosts(store, name)?;
    if !hosts.iter().any(|known| known == host) {
        hosts.push(host.to_owned());
        store.set_config(&sign_in_hosts_key(name), &Value::from(hosts).to_string())?;
    }
    Ok(())
}

/// The manifest last approved in the window for module `name`, as [`canonical`] gives it.
pub fn approved_manifest(store: &Store, name: &str) -> Result<Option<Value>, Error> {
    Ok(store.get_config(&approved_manifest_key(name))?.and_then(|json| serde_json::from_str(&json).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_files_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.wasm");
        std::fs::File::create(&path).unwrap().set_len(MAX_MODULE_FILE + 1).unwrap();
        assert!(read_capped(&path).unwrap_err().contains("larger than 64 MiB"));
        std::fs::File::create(&path).unwrap().set_len(16).unwrap();
        assert_eq!(read_capped(&path).unwrap().len(), 16);
    }
}
