//! Helpers shared by the tests that load modules.

// clippy.toml allows unwrap in #[test] functions only; these helpers are test code too. Each test
// binary compiles this module and uses only some of it.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing, dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use keyring_core::api::CredentialStore;
use open_mess::accounts::Accounts;
use open_mess::modules::{self, Modules};
use open_mess::store::Store;
use open_mess::tools::Registry;
use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{AccountInfo, Engine, HostEvent, ModuleContext, Net, limits};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// A module built by `cargo xtask ci` (the wasm build runs before the tests), found from this test
/// binary: `<target>[/<triple>]/<profile>/deps/<test>.exe`, the module
/// `<target>/wasm32-wasip2/<profile>/<name>.wasm`.
pub fn wasm(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().and_then(|deps| deps.parent()).unwrap();
    let relative = PathBuf::from("wasm32-wasip2").join(profile_dir.file_name().unwrap()).join(format!("{name}.wasm"));
    let found = profile_dir.ancestors().map(|dir| dir.join(&relative)).find(|path| path.exists());
    found.unwrap_or_else(|| panic!("{} is missing; build the modules first (cargo xtask ci)", relative.display()))
}

pub fn echo() -> Vec<u8> {
    std::fs::read(wasm("open_mess_test_echo")).unwrap()
}

/// test-echo rebuilt with one more host in its manifest: its padded host list is patched.
pub fn echo_with_another_host() -> Vec<u8> {
    let bytes = echo();
    let (from, to) = (format!("{:40}", "localhost"), format!("{:40}", "localhost extra.example.com"));
    let at: Vec<usize> = bytes.windows(from.len()).enumerate().filter(|(_, window)| *window == from.as_bytes()).map(|(at, _)| at).collect();
    assert_eq!(at.len(), 1, "the host list must occur once in test-echo");
    let mut patched = bytes;
    patched.splice(at[0]..at[0] + to.len(), to.bytes());
    patched
}

/// test-echo rebuilt with the same manifest: a custom section appended changes only the file.
pub fn echo_rebuilt() -> Vec<u8> {
    let mut bytes = echo();
    let (name, payload) = (b"rebuild".as_slice(), b"2".as_slice());
    bytes.extend([0, u8::try_from(1 + name.len() + payload.len()).unwrap(), u8::try_from(name.len()).unwrap()]);
    bytes.extend(name);
    bytes.extend(payload);
    bytes
}

/// Puts a module file into the data folder's modules folder.
pub fn install(data_dir: &Path, file: &str, bytes: &[u8]) {
    let dir = data_dir.join("modules");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(file), bytes).unwrap();
}

pub fn store(data_dir: &Path) -> Store {
    Store::open(&data_dir.join("state.sqlite3")).unwrap()
}

/// Scans the data folder's modules, sending host events to `events`.
pub async fn scan_with(store: &Store, engine: &Engine, data_dir: &Path, events: mpsc::Sender<HostEvent>) -> Modules {
    let ctx = |_: &str| ModuleContext { data_dir: data_dir.to_path_buf(), events: events.clone(), net: Net::new(AddressPolicy::Strict) };
    Modules::scan(store, engine, &data_dir.join("modules"), ctx).await.unwrap()
}

pub async fn scan(store: &Store, engine: &Engine, data_dir: &Path) -> Modules {
    scan_with(store, engine, data_dir, mpsc::channel(64).0).await
}

/// Approves every module file the scan found, as the user would in the window.
pub fn approve_all(store: &Store, modules: &Modules) {
    for info in modules.list() {
        modules::approve(store, &info.name, &info.file_sha256, &info.manifest_sha256).unwrap();
    }
}

/// Approves every module file installed in `data_dir` and loads them.
pub async fn running(data_dir: &Path, engine: &Engine, events: mpsc::Sender<HostEvent>) -> Modules {
    let store = store(data_dir);
    approve_all(&store, &scan(&store, engine, data_dir).await);
    scan_with(&store, engine, data_dir, events).await
}

pub fn accounts(data_dir: &Path, modules: Modules, credentials: Arc<CredentialStore>) -> Arc<Accounts> {
    Arc::new(Accounts::new(store(data_dir), modules, data_dir.to_path_buf(), credentials))
}

pub struct Daemon {
    pub dir: TempDir,
    pub accounts: Arc<Accounts>,
    pub registry: Registry,
    pub events: mpsc::Receiver<HostEvent>,
    pub credentials: Arc<CredentialStore>,
}

/// A daemon's accounts and tools over module files (file name, built module), all approved.
pub async fn daemon(files: &[(&str, &str)]) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    for (file, module) in files {
        install(dir.path(), file, &std::fs::read(wasm(module)).unwrap());
    }
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let (sender, events) = mpsc::channel(64);
    let modules = running(dir.path(), &engine, sender).await;
    let credentials: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
    let accounts = accounts(dir.path(), modules, Arc::clone(&credentials));
    let registry = Registry::new(Instant::now(), Arc::clone(&accounts));
    Daemon { dir, accounts, registry, events, credentials }
}

pub const ECHO: (&str, &str) = ("echo.wasm", "open_mess_test_echo");

pub fn info(id: &str) -> AccountInfo {
    AccountInfo { id: id.into(), display_name: format!("User {id}") }
}
