//! The wasmtime engine, module loading and the state each module instance runs with.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Store, UpdateDeadline};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::bindings::{PlatformModule, PlatformModulePre};
use crate::error::HostError;
use crate::limits;

/// Mirrors `manifest` in `wit/module.wit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub platform: String,
    pub hosts: Vec<String>,
    pub sign_in_hosts: Vec<String>,
    pub wam: Option<WamIdentity>,
    pub extra_tools: Vec<ToolDecl>,
}

/// Mirrors `wam-identity` in `wit/module.wit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WamIdentity {
    pub client_id: String,
    pub scopes: Vec<String>,
}

/// Mirrors `tool-decl` in `wit/module.wit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    pub input_schema: String,
    pub writes: bool,
}

/// Mirrors `session-state` in `wit/module.wit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Online,
    Offline,
    AuthRequired,
}

/// Mirrors `new-message` in `wit/module.wit`, plus the module that reported it (account ids are
/// unique only within a module).
#[derive(Clone, PartialEq, Eq)]
pub struct NewMessage {
    pub module: String,
    pub account: String,
    pub conversation: String,
    pub message: String,
    pub own: bool,
    pub mentions_me: bool,
    pub direct: bool,
}

// Hand-written so that a `{:?}` in a log line never prints the message id or text.
impl std::fmt::Debug for NewMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewMessage")
            .field("module", &self.module)
            .field("own", &self.own)
            .field("mentions_me", &self.mentions_me)
            .field("direct", &self.direct)
            .finish_non_exhaustive()
    }
}

/// What modules report to the daemon through the `host` imports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEvent {
    MessageArrived(NewMessage),
    Session { module: String, account: String, state: SessionState },
}

/// What the daemon gives a module when it loads it.
pub struct ModuleContext {
    /// The daemon's data folder; the module's storage lives under `modules/` in it.
    pub data_dir: PathBuf,
    pub events: mpsc::Sender<HostEvent>,
}

/// The state a module instance runs with: a WASI context with nothing inherited, plus the host
/// imports' state.
pub(crate) struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl HostState {
    fn new() -> Self {
        // No preopens, environment, arguments or sockets; stdin is empty and stdout/stderr are
        // discarded (WasiCtxBuilder's defaults, made explicit for the network).
        let wasi = WasiCtx::builder()
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            .socket_addr_check(|_, _| Box::pin(async { false }))
            .build();
        HostState { wasi, table: ResourceTable::new() }
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

/// The WebAssembly engine; one per process.
pub struct Engine {
    engine: wasmtime::Engine,
    linker: Linker<HostState>,
}

impl Engine {
    /// Creates the engine and starts its epoch ticker, which lets running modules be interrupted.
    pub fn new() -> Result<Engine, HostError> {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        let engine = wasmtime::Engine::new(&config).map_err(|e| HostError::Load(format!("engine: {e}")))?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(|e| HostError::Load(format!("WASI imports: {e}")))?;
        PlatformModule::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state).map_err(|e| HostError::Load(format!("host imports: {e}")))?;
        let weak = engine.weak();
        std::thread::Builder::new()
            .name("open-mess-epoch".to_string())
            .spawn(move || {
                // Ends once the engine is dropped.
                while let Some(engine) = weak.upgrade() {
                    engine.increment_epoch();
                    drop(engine);
                    std::thread::sleep(limits::EPOCH_TICK);
                }
            })
            .map_err(|e| HostError::Load(format!("epoch ticker: {e}")))?;
        Ok(Engine { engine, linker })
    }

    /// Compiles a module, instantiates it and reads its manifest.
    pub async fn load(&self, wasm: &Path, _ctx: ModuleContext) -> Result<ModuleHandle, HostError> {
        let engine = self.engine.clone();
        let path = wasm.to_path_buf();
        // Compiling takes a while; keep it off the async worker threads.
        let component = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&path).map_err(|e| HostError::Load(format!("cannot read {}: {e}", path.display())))?;
            Component::from_binary(&engine, &bytes).map_err(|e| HostError::Load(format!("not a WebAssembly component: {e}")))
        })
        .await
        .map_err(|e| HostError::Load(format!("compile task: {e}")))??;
        let pre = self
            .linker
            .instantiate_pre(&component)
            .and_then(PlatformModulePre::new)
            .map_err(|e| HostError::Load(format!("not a platform module: {e}")))?;
        let mut store = self.store();
        let instance = pre.instantiate_async(&mut store).await.map_err(|e| HostError::Load(format!("instantiation failed: {e}")))?;
        let manifest: Manifest = instance
            .open_mess_module_module()
            .call_manifest(&mut store)
            .await
            .map_err(|e| HostError::Load(format!("manifest call failed: {e}")))?
            .into();
        check_name(&manifest.name)?;
        Ok(ModuleHandle { manifest })
    }

    fn store(&self) -> Store<HostState> {
        let mut store = Store::new(&self.engine, HostState::new());
        // Yield to the executor on every epoch tick, so a busy module never blocks a thread.
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|_| Ok(UpdateDeadline::Yield(1)));
        store
    }
}

/// The module name becomes a file name and a credential namespace: plain lowercase identifiers
/// only.
fn check_name(name: &str) -> Result<(), HostError> {
    let plain = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_';
    if (1..=64).contains(&name.len()) && name.chars().all(plain) {
        Ok(())
    } else {
        Err(HostError::Load("the manifest name must be 1-64 characters of a-z, 0-9, '-' or '_'".to_string()))
    }
}

/// A loaded module.
pub struct ModuleHandle {
    manifest: Manifest,
}

impl ModuleHandle {
    /// The manifest read at load time; the host list it declares is the one enforced.
    pub fn manifest(&self) -> Result<Manifest, HostError> {
        Ok(self.manifest.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_name_is_a_plain_identifier() {
        for good in ["teams", "test-echo", "a_1"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in ["", "Teams", "../teams", "a/b", "a.b", "tëams", &"a".repeat(65)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }
}
