//! Each loaded module runs in one tokio task that owns its store (an actor). Calls into a module
//! are serialized, run under the time and memory limits, and a trap or timeout drops that module's
//! instance only; the next call instantiates it again, until `MAX_CRASHES` crashes in a row
//! disable it (spec 8.3).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use wasmtime::{Store, UpdateDeadline};

use crate::bindings::{PlatformModule, PlatformModulePre};
use crate::error::{HostError, ModuleError};
use crate::limits::{self, Limits};
use crate::runtime::{AccountInfo, HostState, ModuleEnv};

/// Requests waiting for a module beyond this are refused with backpressure on the sender.
pub(crate) const QUEUE: usize = 32;
/// WebSocket frames and timer firings waiting for a module; a full queue holds the socket back.
const EVENT_QUEUE: usize = 64;

pub(crate) type Reply<T> = oneshot::Sender<Result<T, HostError>>;

/// A call into one of the module's exports, sent by a `ModuleHandle`.
pub(crate) enum Request {
    SignIn(Reply<AccountInfo>),
    Restore(String, Reply<()>),
    SignOut(String, Reply<()>),
    Call { account: String, tool: String, args: String, reply: Reply<String> },
}

/// What a module instance's sockets and timers deliver to its `events` export, through the actor
/// so each delivery is an ordinary call with the normal limits (spec 8.3).
pub(crate) enum GuestEvent {
    WsMessage { conn: u32, text: String },
    WsClosed { conn: u32, reason: String },
    Timer { id: u32, generation: u64 },
}

/// A task that belongs to a module instance (a socket, a timer): aborted when dropped, so an
/// instance that is dropped takes them with it.
pub(crate) struct Task(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
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

/// The deadline of the call in progress, shared by the actor, the store's epoch callback and the
/// sign-in imports, which pause it while they wait for the user (spec 8.3, R4).
#[derive(Clone, Default)]
pub(crate) struct Deadline(Arc<Mutex<DeadlineState>>);

#[derive(Default)]
struct DeadlineState {
    at: Option<Instant>,
    paused_since: Option<Instant>,
}

/// While paused, the actor looks at the deadline this often.
const PAUSED_POLL: Duration = Duration::from_secs(1);

impl Deadline {
    fn state(&self) -> MutexGuard<'_, DeadlineState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, at: Option<Instant>) {
        *self.state() = DeadlineState { at, paused_since: None };
    }

    /// When the actor should look at the deadline again; None outside a call.
    fn wake_at(&self) -> Option<Instant> {
        let state = self.state();
        if state.paused_since.is_some() { state.at.map(|_| Instant::now() + PAUSED_POLL) } else { state.at }
    }

    pub(crate) fn expired(&self) -> bool {
        let state = self.state();
        state.paused_since.is_none() && state.at.is_some_and(|at| Instant::now() >= at)
    }

    /// Stops the clock until the returned guard is dropped; the paused time is added back.
    pub(crate) fn pause(&self) -> Paused<'_> {
        let mut state = self.state();
        if state.paused_since.is_none() {
            state.paused_since = Some(Instant::now());
        }
        Paused(self)
    }
}

pub(crate) struct Paused<'a>(&'a Deadline);

impl Drop for Paused<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state();
        if let Some(since) = state.paused_since.take()
            && let Some(at) = &mut state.at
        {
            *at += since.elapsed();
        }
    }
}

/// One export call on an instance; boxed because it borrows the store it runs in.
type Export<'a, R> = Pin<Box<dyn Future<Output = wasmtime::Result<R>> + Send + 'a>>;

/// Runs `call` until it finishes or the deadline passes. Dropping the call cancels it wherever it
/// is: in WebAssembly (which yields on every epoch tick) or in a host import.
async fn within<R>(deadline: &Deadline, call: impl Future<Output = wasmtime::Result<R>>) -> Result<R, HostError> {
    tokio::pin!(call);
    loop {
        let Some(at) = deadline.wake_at() else { return Err(HostError::Timeout) };
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
    /// The current instance's events; replaced and dropped with it, so a dropped instance's
    /// leftovers never reach the next one.
    events: Option<mpsc::Receiver<GuestEvent>>,
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
        let actor = Actor { env, engine, pre, limits, deadline: Deadline::default(), instance: None, events: None, crashes: 0, state };
        (actor, watch)
    }

    fn store(&self) -> (Store<HostState>, mpsc::Receiver<GuestEvent>) {
        let (events, queue) = mpsc::channel(EVENT_QUEUE);
        let mut store = Store::new(&self.engine, HostState::new(self.deadline.clone(), self.env.clone(), events));
        store.limiter(|state| &mut state.limiter);
        // Yield to the executor on every epoch tick, so a busy module never holds a thread, and
        // stop it once its call is past the deadline.
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|state| Ok(if state.data().deadline.expired() { UpdateDeadline::Interrupt } else { UpdateDeadline::Yield(1) }));
        (store, queue)
    }

    /// Gives the module a new environment; the current instance is dropped so the next call
    /// runs with it.
    pub(crate) fn reset(&mut self, env: Arc<ModuleEnv>) {
        self.env = env;
        self.instance = None;
        self.events = None;
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
                let (mut store, events) = self.store();
                let module = within(&self.deadline, self.pre.instantiate_async(&mut store)).await?;
                self.events = Some(events);
                (store, module)
            }
        };
        let value = within(&self.deadline, export(&module, &mut store)).await?;
        self.instance = Some((store, module));
        Ok(value)
    }

    fn crashed(&mut self, error: HostError) -> HostError {
        self.events = None;
        self.crashes = self.crashes.saturating_add(1);
        tracing::warn!(module = %self.env.name, crashes = self.crashes, "module stopped: {error}");
        self.state.send_replace(if self.crashes >= limits::MAX_CRASHES {
            ModuleState::Disabled
        } else {
            ModuleState::Crashed { count: self.crashes, last: error.to_string() }
        });
        error
    }

    /// Serves requests and the instance's events until every handle is dropped or the module is
    /// disabled.
    pub(crate) async fn serve(mut self, mut requests: mpsc::Receiver<Request>) {
        loop {
            let work = tokio::select! {
                request = requests.recv() => match request {
                    Some(request) => Ok(request),
                    None => break,
                },
                Some(event) = next_event(&mut self.events) => Err(event),
            };
            match work {
                Ok(request) => self.answer(request).await,
                Err(event) => self.deliver(event).await,
            }
            if *self.state.borrow() == ModuleState::Disabled {
                break;
            }
        }
    }

    async fn deliver(&mut self, event: GuestEvent) {
        // A crash is recorded by `run`; there is nobody else to tell.
        let _delivered = match event {
            GuestEvent::WsMessage { conn, text } => {
                self.run(move |m, s| Box::pin(async move { m.open_mess_module_events().call_on_ws_message(s, conn, &text).await })).await
            }
            GuestEvent::WsClosed { conn, reason } => {
                self.run(move |m, s| {
                    Box::pin(async move {
                        s.data_mut().sockets.close(conn);
                        m.open_mess_module_events().call_on_ws_closed(s, conn, &reason).await
                    })
                })
                .await
            }
            GuestEvent::Timer { id, generation } => {
                self.run(move |m, s| {
                    Box::pin(async move {
                        if !s.data_mut().timers.fired(id, generation) {
                            return Ok(());
                        }
                        m.open_mess_module_events().call_on_timer(s, id).await
                    })
                })
                .await
            }
        };
    }

    async fn answer(&mut self, request: Request) {
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
    }
}

async fn next_event(events: &mut Option<mpsc::Receiver<GuestEvent>>) -> Option<GuestEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// A call that ran but returned the module's own error.
fn module_result<T, E: Into<ModuleError>>(result: Result<Result<T, E>, HostError>) -> Result<T, HostError> {
    result.and_then(|r| r.map_err(|e| HostError::Module(e.into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_time_does_not_count() {
        let deadline = Deadline::default();
        deadline.set(Some(Instant::now() + Duration::from_millis(50)));
        {
            let _paused = deadline.pause();
            std::thread::sleep(Duration::from_millis(100));
            assert!(!deadline.expired());
        }
        // The 100 ms paused were added back: 50 ms are still left.
        assert!(!deadline.expired());
        std::thread::sleep(Duration::from_millis(80));
        assert!(deadline.expired());
    }
}
