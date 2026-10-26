//! The modules folder (spec 8.3): every `.wasm` file in `<data_dir>/modules/`, the user's approval of
//! each module, and a running module for each approved one.
//!
//! A module runs only when both its file and its manifest are the ones the user approved (SHA-256 of
//! the file and of the canonical manifest JSON). Its manifest is read to find its name, which the
//! host does with no network, storage or secrets. One file per manifest name is the module; another
//! file with the same name waits as its replacement (spec 8.3, R6 and R7).

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read as _;
use std::path::Path;

use open_mess_host::{Engine, Manifest, ModuleContext, ModuleHandle};
use rusqlite::OptionalExtension;
use serde_json::json;
use sha2::{Digest, Sha256};

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
}

/// The module files found at start, and the running modules.
#[derive(Default)]
pub struct Modules {
    entries: Vec<Entry>,
}

/// A file that loaded as a module.
struct Candidate {
    file: String,
    file_sha256: String,
    manifest: Manifest,
    manifest_sha256: String,
    handle: ModuleHandle,
}

impl Modules {
    /// Reads every `.wasm` file in `dir` (created if missing) and keeps the approved modules
    /// running. `ctx` gives the context for loading a file, by file name.
    pub async fn scan(store: &Store, engine: &Engine, dir: &Path, ctx: impl Fn(&str) -> ModuleContext) -> Result<Modules, Error> {
        std::fs::create_dir_all(dir)?;
        let mut paths: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("wasm")) && path.is_file())
            .collect();
        paths.sort();

        let mut entries = Vec::new();
        let mut by_name: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
        for path in paths {
            let file = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            // The bytes that are hashed are the bytes that are loaded: a file swapped in between
            // never runs under the other file's approval.
            let bytes = match read_capped(&path) {
                Ok(bytes) => bytes,
                Err(why) => {
                    entries.push(invalid(file, String::new(), why));
                    continue;
                }
            };
            let file_sha256 = sha256_hex(&bytes);
            match engine.load_bytes(bytes, ctx(&file)).await {
                Ok(handle) => match handle.manifest() {
                    Ok(manifest) => {
                        let manifest_sha256 = manifest_sha256(&manifest);
                        by_name.entry(manifest.name.clone()).or_default().push(Candidate { file, file_sha256, manifest, manifest_sha256, handle });
                    }
                    Err(err) => entries.push(invalid(file, file_sha256, err.to_string())),
                },
                Err(err) => entries.push(invalid(file, file_sha256, err.to_string())),
            }
        }

        for (name, candidates) in by_name {
            let row = Row::get(store, &name)?;
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
                    name: manifest.name,
                    file,
                    version: manifest.version,
                    platform: manifest.platform,
                    hosts: manifest.hosts,
                    state,
                    file_sha256,
                    manifest_sha256,
                };
                entries.push(Entry { info, handle });
            }
        }
        Ok(Modules { entries })
    }

    /// The module's file, not counting files waiting to replace it.
    fn current(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.info.name == name && entry.info.state != ModuleState::PendingReplacement)
    }

    pub fn state(&self, name: &str) -> Option<ModuleState> {
        self.current(name).map(Entry::state)
    }

    /// The running module, only when it is active.
    pub fn handle(&self, name: &str) -> Option<&ModuleHandle> {
        self.current(name).filter(|entry| entry.state() == ModuleState::Active).and_then(|entry| entry.handle.as_ref())
    }

    pub fn list(&self) -> Vec<ModuleInfo> {
        self.entries.iter().map(|entry| ModuleInfo { state: entry.state(), ..entry.info.clone() }).collect()
    }
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

fn invalid(file: String, file_sha256: String, why: String) -> Entry {
    let info = ModuleInfo {
        name: file.clone(),
        file,
        version: String::new(),
        platform: String::new(),
        hosts: Vec::new(),
        state: ModuleState::Invalid(why),
        file_sha256,
        manifest_sha256: String::new(),
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
    fn get(store: &Store, name: &str) -> Result<Option<Row>, Error> {
        let row = store
            .conn()
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

/// Approves a module file and manifest; the next scan runs the file with these hashes. For the
/// approval UI (milestones C and E) and tests: no MCP or CLI tool approves anything.
pub fn approve(store: &Store, name: &str, file_sha256: &str, manifest_sha256: &str) -> Result<(), Error> {
    store.conn().execute(
        "INSERT INTO modules (name, file, file_sha256, manifest_sha256, approved_file_sha256, approved_manifest_sha256)
         VALUES (?1, '', ?2, ?3, ?2, ?3)
         ON CONFLICT (name) DO UPDATE SET approved_file_sha256 = excluded.approved_file_sha256,
             approved_manifest_sha256 = excluded.approved_manifest_sha256",
        [name, file_sha256, manifest_sha256],
    )?;
    Ok(())
}

/// Turns a module on or off from the next scan. For the configuration window (milestone E) and
/// tests.
pub fn set_enabled(store: &Store, name: &str, enabled: bool) -> Result<(), Error> {
    store.conn().execute(
        "INSERT INTO modules (name, file, file_sha256, manifest_sha256, enabled) VALUES (?1, '', '', '', ?2)
         ON CONFLICT (name) DO UPDATE SET enabled = excluded.enabled",
        (name, enabled),
    )?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// SHA-256 of the manifest as JSON with sorted keys (serde_json's map is ordered by key).
fn manifest_sha256(manifest: &Manifest) -> String {
    let tools: Vec<_> = manifest
        .extra_tools
        .iter()
        .map(|tool| json!({"description": tool.description, "input_schema": tool.input_schema, "name": tool.name, "writes": tool.writes}))
        .collect();
    let canonical = json!({
        "extra_tools": tools,
        "hosts": manifest.hosts,
        "name": manifest.name,
        "platform": manifest.platform,
        "sign_in_hosts": manifest.sign_in_hosts,
        "version": manifest.version,
        "wam": manifest.wam.as_ref().map(|wam| json!({"client_id": wam.client_id, "scopes": wam.scopes})),
    });
    sha256_hex(canonical.to_string().as_bytes())
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
