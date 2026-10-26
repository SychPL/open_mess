//! Each loaded module runs in one tokio task that owns its store (an actor). Calls into a module
//! are serialized, run under the time and memory limits, and a trap or timeout drops that module's
//! instance only; the next call instantiates it again, until `MAX_CRASHES` crashes in a row
//! disable it (spec 8.3).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use tokio::sync::{mpsc, oneshot, watch};
use wasmtime::{Store, UpdateDeadline};

use crate::bindings::{PlatformModule, PlatformModulePre};
use crate::error::{HostError, ModuleError};
use crate::limits::{self, Limits};
use crate::runtime::{AccountInfo, HostState, ModuleEnv};

/// Requests waiting for a module beyond this are refused with backpressure on the sender.
pub(crate) const QUEUE: usize = 32;

pub(crate) type Reply<T> = oneshot::Sender<Result<T, HostError>>;

/// A call into one of the module's exports, sent by a `ModuleHandle`.
pub(crate) enum Request {
    SignIn(Reply<AccountInfo>),
    Restore(String, Reply<()>),
    SignOut(String, Reply<()>),
    Call { account: String, tool: String, args: String, reply: Reply<String> },
}

/// Whether a module is serving calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleState {
    Running,
    /// The last call trapped or timed out; the next call loads the module again.
    Crashed {
        count: u32,
        last: String,
    },
    /// `MAX_CRASHES` crashes in a row; the module is not loaded again until the daemon reloads it.
    Disabled,
}

/// The deadline of the call in progress, shared by the actor and the store's epoch callback.
#[derive(Clone, Default)]
pub(crate) struct Deadline(Arc<Mutex<Option<Instant>>>);

impl Deadline {
    fn set(&self, at: Option<Instant>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = at;
    }

    fn at(&self) -> Option<Instant> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn expired(&self) -> bool {
        self.at().is_some_and(|at| Instant::now() >= at)
    }
}

/// One export call on an instance; boxed because it borrows the store it runs in.
type Export<'a, R> = Pin<Box<dyn Future<Output = wasmtime::Result<R>> + Send + 'a>>;

/// Runs `call` until it finishes or the deadline passes. Dropping the call cancels it wherever it
/// is: in WebAssembly (which yields on every epoch tick) or in a host import.
async fn within<R>(deadline: &Deadline, call: impl Future<Output = wasmtime::Result<R>>) -> Result<R, HostError> {
    tokio::pin!(call);
    loop {
        let Some(at) = deadline.at() else { return Err(HostError::Timeout) };
        tokio::select! {
            result = &mut call => {
                // The root cause is the trap itself; the outer error is a WebAssembly backtrace.
                return result.map_err(|e| if deadline.expired() { HostError::Timeout } else { HostError::Crashed(e.root_cause().to_string()) });
            }
            () = tokio::time::sleep_until(at.into()) => {
                if deadline.expired() {
                    return Err(HostError::Timeout);
                }
            }
        }
    }
}

pub(crate) struct Actor {
    env: Arc<ModuleEnv>,
    engine: wasmtime::Engine,
    pre: PlatformModulePre<HostState>,
    limits: Limits,
    deadline: Deadline,
    instance: Option<(Store<HostState>, PlatformModule)>,
    crashes: u32,
    state: watch::Sender<ModuleState>,
}

impl Actor {
    pub(crate) fn new(
        engine: wasmtime::Engine,
        pre: PlatformModulePre<HostState>,
        limits: Limits,
        env: Arc<ModuleEnv>,
    ) -> (Actor, watch::Receiver<ModuleState>) {
        let (state, watch) = watch::channel(ModuleState::Running);
        let actor = Actor { env, engine, pre, limits, deadline: Deadline::default(), instance: None, crashes: 0, state };
        (actor, watch)
    }

    fn store(&self) -> Store<HostState> {
        let mut store = Store::new(&self.engine, HostState::new(self.deadline.clone(), self.env.clone()));
        store.limiter(|state| &mut state.limiter);
        // Yield to the executor on every epoch tick, so a busy module never holds a thread, and
        // stop it once its call is past the deadline.
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|state| Ok(if state.data().deadline.expired() { UpdateDeadline::Interrupt } else { UpdateDeadline::Yield(1) }));
        store
    }

    /// Gives the module a new environment; the current instance is dropped so the next call
    /// runs with it.
    pub(crate) fn reset(&mut self, env: Arc<ModuleEnv>) {
        self.env = env;
        self.instance = None;
    }

    /// Runs one export under the call limits, instantiating the module first if the last call
    /// dropped it.
    pub(crate) async fn run<R>(
        &mut self,
        export: impl for<'a> FnOnce(&'a PlatformModule, &'a mut Store<HostState>) -> Export<'a, R>,
    ) -> Result<R, HostError> {
        if *self.state.borrow() == ModuleState::Disabled {
            return Err(HostError::Closed);
        }
        self.deadline.set(Some(Instant::now() + self.limits.call_timeout));
        let result = self.run_on_instance(export).await;
        self.deadline.set(None);
        match result {
            Ok(value) => {
                if self.crashes > 0 {
                    self.crashes = 0;
                    self.state.send_replace(ModuleState::Running);
                }
                Ok(value)
            }
            Err(error) => Err(self.crashed(error)),
        }
    }

    async fn run_on_instance<R>(
        &mut self,
        export: impl for<'a> FnOnce(&'a PlatformModule, &'a mut Store<HostState>) -> Export<'a, R>,
    ) -> Result<R, HostError> {
        // Taken out, and put back only after a clean return: a trapped or cancelled store is dropped.
        let (mut store, module) = match self.instance.take() {
            Some(instance) => instance,
            None => {
                let mut store = self.store();
                let module = within(&self.deadline, self.pre.instantiate_async(&mut store)).await?;
                (store, module)
            }
        };
        let value = within(&self.deadline, export(&module, &mut store)).await?;
        self.instance = Some((store, module));
        Ok(value)
    }

    fn crashed(&mut self, error: HostError) -> HostError {
        self.crashes = self.crashes.saturating_add(1);
        tracing::warn!(module = %self.env.name, crashes = self.crashes, "module stopped: {error}");
        self.state.send_replace(if self.crashes >= limits::MAX_CRASHES {
            ModuleState::Disabled
        } else {
            ModuleState::Crashed { count: self.crashes, last: error.to_string() }
        });
        error
    }

    /// Serves requests until every handle is dropped or the module is disabled.
    pub(crate) async fn serve(mut self, mut requests: mpsc::Receiver<Request>) {
        while let Some(request) = requests.recv().await {
            match request {
                Request::SignIn(reply) => {
                    let result = self.run(|m, s| Box::pin(m.open_mess_module_module().call_sign_in(s))).await;
                    reply.send(module_result(result).map(Into::into)).ok();
                }
                Request::Restore(account, reply) => {
                    let result = self.run(move |m, s| Box::pin(async move { m.open_mess_module_module().call_restore(s, &account).await })).await;
                    reply.send(module_result(result)).ok();
                }
                Request::SignOut(account, reply) => {
                    let result = self.run(move |m, s| Box::pin(async move { m.open_mess_module_module().call_sign_out(s, &account).await })).await;
                    reply.send(result).ok();
                }
                Request::Call { account, tool, args, reply } => {
                    let result =
                        self.run(move |m, s| Box::pin(async move { m.open_mess_module_module().call_call(s, &account, &tool, &args).await })).await;
                    reply.send(module_result(result)).ok();
                }
            }
            if *self.state.borrow() == ModuleState::Disabled {
                break;
            }
        }
    }
}

/// A call that ran but returned the module's own error.
fn module_result<T, E: Into<ModuleError>>(result: Result<Result<T, E>, HostError>) -> Result<T, HostError> {
    result.and_then(|r| r.map_err(|e| HostError::Module(e.into())))
}
