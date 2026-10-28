//! The wasmtime engine, module loading and the state each module instance runs with.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::{mpsc, oneshot, watch};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::actor::{Actor, Deadline, GuestEvent, ModuleState, QUEUE, Reply, Request};
use crate::bindings::{PlatformModule, PlatformModulePre};
use crate::error::HostError;
use crate::hosts::HostList;
use crate::limits::{self, Limits, ModuleLimiter};
use crate::net::Net;
use crate::sign_in::{Caller, CodeShown, SignInPrompts};
use crate::storage::Storage;
use crate::timers::Timers;
use crate::wam::WamAccount;
use crate::ws::Sockets;

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

/// Mirrors `account-info` in `wit/module.wit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountInfo {
    pub id: String,
    pub display_name: String,
}

/// Mirrors `attachment` in `wit/module.wit`.
#[derive(Clone, PartialEq, Eq)]
pub struct Attachment {
    pub file_name: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

// Hand-written so that a `{:?}` never prints the file name or the bytes.
impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment").field("content_type", &self.content_type).field("len", &self.data.len()).finish_non_exhaustive()
    }
}

/// Mirrors `participant` in `wit/module.wit`.
#[derive(Clone, PartialEq, Eq)]
pub struct Participant {
    pub id: String,
    pub display_name: String,
}

/// Mirrors `target` in `wit/module.wit`.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub conversation_title: String,
    pub participants: Vec<Participant>,
    pub target_text: Option<String>,
}

// Hand-written so that a `{:?}` never prints names or text.
impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target").field("participants", &self.participants.len()).finish_non_exhaustive()
    }
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
    /// The sender's platform id.
    pub sender: String,
    pub own: bool,
    pub mentions_me: bool,
    pub direct: bool,
}

// Hand-written so that a `{:?}` in a log line never prints the message or sender id.
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
    /// The network modules reach the outside world through.
    pub net: Net,
    /// Where the module's sign-in prompts go.
    pub sign_in: SignInPrompts,
}

/// What a module's imports work with; shared by all its instances.
pub(crate) struct ModuleEnv {
    /// Empty while the manifest is read at load; storage and secrets are refused then.
    pub(crate) name: String,
    pub(crate) data_dir: PathBuf,
    pub(crate) hosts: HostList,
    pub(crate) net: Net,
    pub(crate) events: mpsc::Sender<HostEvent>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fake_wam: Option<crate::FakeWam>,
    /// Test support: the longest delay a timer gets, in milliseconds.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) timer_cap: Option<u32>,
    pub(crate) sign_in: SignInPrompts,
    /// The manifest's Windows account identity, the only one `wam-token` asks for.
    pub(crate) wam: Option<WamIdentity>,
}

/// The state a module instance runs with: a WASI context with nothing inherited, plus the host
/// imports' state.
pub(crate) struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    pub(crate) limiter: ModuleLimiter,
    pub(crate) deadline: Deadline,
    pub(crate) env: Arc<ModuleEnv>,
    pub(crate) sockets: Sockets,
    pub(crate) timers: Timers,
    pub(crate) storage: Option<Storage>,
    /// The largest HTTP body `http.fetch` takes now: `MAX_ATTACHMENT` during `get-attachment`.
    pub(crate) max_body: usize,
    /// Where this instance's sockets and timers send what they deliver.
    pub(crate) guest_events: mpsc::Sender<GuestEvent>,
    /// Whose call runs now, set by the actor before each call.
    pub(crate) caller: Caller,
    /// The Windows account picked during the `sign-in` call that runs.
    pub(crate) pick: Option<WamAccount>,
    /// A device or QR code on screen, which the `sign-in` call that runs polls for.
    pub(crate) code_shown: Option<CodeShown>,
}

impl HostState {
    pub(crate) fn new(deadline: Deadline, env: Arc<ModuleEnv>, guest_events: mpsc::Sender<GuestEvent>) -> Self {
        // No preopens, environment, arguments or sockets; stdin is empty and stdout/stderr are
        // discarded (WasiCtxBuilder's defaults, made explicit for the network).
        let wasi = WasiCtx::builder()
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            .socket_addr_check(|_, _| Box::pin(async { false }))
            .build();
        HostState {
            wasi,
            table: ResourceTable::new(),
            limiter: ModuleLimiter::default(),
            deadline,
            env,
            sockets: Sockets::default(),
            timers: Timers::default(),
            storage: None,
            max_body: limits::MAX_BODY,
            guest_events,
            caller: Caller::default(),
            pick: None,
            code_shown: None,
        }
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

/// The WebAssembly engine; one per process. Clones share it.
#[derive(Clone)]
pub struct Engine {
    engine: wasmtime::Engine,
    linker: Linker<HostState>,
    limits: Limits,
    #[cfg(any(test, feature = "test-support"))]
    fake_wam: Option<crate::FakeWam>,
    #[cfg(any(test, feature = "test-support"))]
    timer_cap: Option<u32>,
}

impl Engine {
    /// Creates the engine and starts its epoch ticker, which lets running modules be interrupted.
    pub fn new() -> Result<Engine, HostError> {
        Self::build(Limits::default())
    }

    /// An engine with other limits, for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_limits(limits: Limits) -> Result<Engine, HostError> {
        Self::build(limits)
    }

    /// Test support: modules loaded from now on get their `wam-token` answers from `wam`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_fake_wam(mut self, wam: crate::FakeWam) -> Engine {
        self.fake_wam = Some(wam);
        self
    }

    /// Test support: timers of modules loaded from now on fire after at most `max_ms`, so a test
    /// need not wait for a module's real interval.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_timer_cap(mut self, max_ms: u32) -> Engine {
        self.timer_cap = Some(max_ms);
        self
    }

    fn build(limits: Limits) -> Result<Engine, HostError> {
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
        Ok(Engine {
            engine,
            linker,
            limits,
            #[cfg(any(test, feature = "test-support"))]
            fake_wam: None,
            #[cfg(any(test, feature = "test-support"))]
            timer_cap: None,
        })
    }

    /// Compiles a module file, reads its manifest (under the call limits) and starts its actor.
    pub async fn load(&self, wasm: &Path, ctx: ModuleContext) -> Result<ModuleHandle, HostError> {
        let path = wasm.to_path_buf();
        let bytes =
            tokio::task::spawn_blocking(move || std::fs::read(&path).map_err(|e| HostError::Load(format!("cannot read {}: {e}", path.display()))))
                .await
                .map_err(|e| HostError::Load(format!("read task: {e}")))??;
        self.load_bytes(bytes, ctx).await
    }

    /// [`Engine::load`] from bytes already read, so the bytes a caller checked (hashed, approved)
    /// are the bytes that run.
    pub async fn load_bytes(&self, bytes: Vec<u8>, ctx: ModuleContext) -> Result<ModuleHandle, HostError> {
        let engine = self.engine.clone();
        // Compiling takes a while; keep it off the async worker threads.
        let component = tokio::task::spawn_blocking(move || {
            Component::from_binary(&engine, &bytes).map_err(|e| HostError::Load(format!("not a WebAssembly component: {e}")))
        })
        .await
        .map_err(|e| HostError::Load(format!("compile task: {e}")))??;
        let pre = self
            .linker
            .instantiate_pre(&component)
            .and_then(PlatformModulePre::new)
            .map_err(|e| HostError::Load(format!("not a platform module: {e}")))?;
        // The manifest runs before the module has a name or hosts: its imports reach nothing, and
        // its host events go nowhere.
        let ModuleContext { data_dir, events, net, sign_in } = ctx;
        let (nowhere, _) = mpsc::channel(1);
        let loading = ModuleEnv {
            name: String::new(),
            data_dir: data_dir.clone(),
            hosts: HostList::default(),
            net: net.clone(),
            events: nowhere,
            #[cfg(any(test, feature = "test-support"))]
            fake_wam: None,
            #[cfg(any(test, feature = "test-support"))]
            timer_cap: None,
            sign_in: sign_in.clone(),
            wam: None,
        };
        let (mut actor, state) = Actor::new(self.engine.clone(), pre, self.limits, Arc::new(loading));
        let manifest: Manifest = actor
            .run(|m, s| Box::pin(m.open_mess_module_module().call_manifest(s)))
            .await
            .map_err(|e| HostError::Load(format!("manifest call failed: {e}")))?
            .into();
        check_name(&manifest.name)?;
        let hosts = HostList::parse(&manifest.hosts)?;
        // Not used until the sign-in window (milestone E), but a bad manifest is refused now.
        HostList::parse(&manifest.sign_in_hosts)?;
        actor.reset(Arc::new(ModuleEnv {
            name: manifest.name.clone(),
            data_dir,
            hosts,
            net,
            events,
            #[cfg(any(test, feature = "test-support"))]
            fake_wam: self.fake_wam.clone(),
            #[cfg(any(test, feature = "test-support"))]
            timer_cap: self.timer_cap,
            sign_in,
            wam: manifest.wam.clone(),
        }));
        let (requests, queue) = mpsc::channel(QUEUE);
        let crashes_since_load = actor.crashes_since_load.clone();
        tokio::spawn(actor.serve(queue));
        Ok(ModuleHandle { manifest, requests, state, crashes_since_load })
    }
}

/// The module name becomes a file name and a credential namespace: plain lowercase identifiers
/// only.
fn check_name(name: &str) -> Result<(), HostError> {
    let plain = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_';
    if !(1..=64).contains(&name.len()) || !name.chars().all(plain) {
        return Err(HostError::Load("the manifest name must be 1-64 characters of a-z, 0-9, '-' or '_'".to_string()));
    }
    // Windows device names: `con.sqlite3` would open the console, not a file.
    let device = ["con", "prn", "aux", "nul"].contains(&name)
        || ["com", "lpt"]
            .iter()
            .any(|prefix| name.strip_prefix(prefix).is_some_and(|n| n.len() == 1 && n != "0" && n.chars().all(|c| c.is_ascii_digit())));
    if device {
        return Err(HostError::Load(format!("the manifest name {name:?} is a Windows device name")));
    }
    Ok(())
}

/// A loaded module. Clones share the module; calls from all of them run one at a time.
#[derive(Clone)]
pub struct ModuleHandle {
    manifest: Manifest,
    requests: mpsc::Sender<Request>,
    state: watch::Receiver<ModuleState>,
    crashes_since_load: Arc<AtomicU32>,
}

impl ModuleHandle {
    /// The manifest read at load time; the host list it declares is the one enforced.
    pub fn manifest(&self) -> Result<Manifest, HostError> {
        Ok(self.manifest.clone())
    }

    pub fn state(&self) -> ModuleState {
        self.state.borrow().clone()
    }

    /// How many times the module crashed since it was loaded. A good call does not reset it (unlike
    /// the crashes in a row that disable a module); only loading the module again does.
    pub fn crashes_since_load(&self) -> u32 {
        self.crashes_since_load.load(Ordering::Relaxed)
    }

    /// Stops the module's timers and sockets from running its event handlers; returns once none is
    /// running. Tool calls still work. Events wait in their queue (a full one holds timers and
    /// sockets back), and [`ModuleHandle::resume_events`] delivers them.
    pub async fn pause_events(&self) -> Result<(), HostError> {
        self.request(Request::PauseEvents).await
    }

    pub async fn resume_events(&self) -> Result<(), HostError> {
        self.request(Request::ResumeEvents).await
    }

    /// Whether both handles are the same loaded module (clones of one handle), not two loads.
    pub fn same(&self, other: &ModuleHandle) -> bool {
        self.requests.same_channel(&other.requests)
    }

    async fn request<T>(&self, request: impl FnOnce(Reply<T>) -> Request) -> Result<T, HostError> {
        let (reply, answer) = oneshot::channel();
        self.requests.send(request(reply)).await.map_err(|_| HostError::Closed)?;
        answer.await.map_err(|_| HostError::Closed)?
    }

    /// Runs the module's interactive sign-in and returns the account it signed in.
    pub async fn sign_in(&self) -> Result<AccountInfo, HostError> {
        self.request(Request::SignIn).await
    }

    /// Restores an account's session from its stored secrets (at daemon start).
    pub async fn restore(&self, account: &str) -> Result<(), HostError> {
        self.request(|reply| Request::Restore(account.to_string(), reply)).await
    }

    pub async fn sign_out(&self, account: &str) -> Result<(), HostError> {
        self.request(|reply| Request::SignOut(account.to_string(), reply)).await
    }

    /// Runs a tool: a shared one (spec 8.5) or one of the manifest's extra tools.
    pub async fn call(&self, account: &str, tool: &str, args_json: &str) -> Result<String, HostError> {
        self.request(|reply| Request::Call { account: account.to_string(), tool: tool.to_string(), args: args_json.to_string(), reply }).await
    }

    /// The bytes of an attachment, for the core to save (the `get_attachment` tool).
    pub async fn get_attachment(&self, account: &str, conversation: &str, message: &str, attachment: &str) -> Result<Attachment, HostError> {
        let ids = [account, conversation, message, attachment].map(str::to_string);
        self.request(|reply| Request::GetAttachment { ids, reply }).await
    }

    /// What a write in `conversation` would be about (`message`: the one replied to, edited or
    /// reacted to), for its approval.
    pub async fn describe_target(&self, account: &str, conversation: &str, message: Option<&str>) -> Result<Target, HostError> {
        let ids = [account, conversation].map(str::to_string);
        let message = message.map(str::to_string);
        self.request(|reply| Request::DescribeTarget { ids, message, reply }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_name_is_a_plain_identifier() {
        for good in ["teams", "test-echo", "a_1", "com", "com10", "console"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in ["", "Teams", "../teams", "a/b", "a.b", "tëams", &"a".repeat(65), "con", "nul", "com1", "lpt9"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }
}
