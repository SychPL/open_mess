//! `om daemon`: the background process that serves the local API, with the tray icon and the
//! configuration window on Windows.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{Engine, ModuleContext, Net, SignInPrompts};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::{Notify, mpsc, oneshot};

use crate::accounts::Accounts;
use crate::events::Bus;
use crate::instance::InstanceLock;
use crate::modules::Modules;
use crate::pending::Pending;
use crate::store::Store;
use crate::tools::{GateMode, Registry};
use crate::triggers::spawn::{self, Runs};
use crate::triggers::{self, Actions};
use crate::ui::sign_in::SignIns;
use crate::ui::{Opener, UiState};
use crate::update::probation::{self, Start};
use crate::update::quiesce::{Activity, Launcher};
use crate::update::state::Paths;
use crate::{Error, api, approval, paths, secrets, ui};

/// Why the last `om daemon` failed to start, one line, in the data folder. The CLI shows it when
/// it can't reach the daemon; a successful start deletes it.
pub const LAST_START_ERROR_FILE: &str = "last-start-error.txt";

/// Runs the daemon until Ctrl+C, an update hands over to its helper or, on Windows, `Zakończ` in the
/// tray menu. The tray and the configuration window take the main thread; the API runs on the
/// runtime's threads.
pub fn run() -> Result<(), Error> {
    let started = Instant::now();
    let dir = paths::data_dir()?;
    // First of all: an update's helper may be changing the files, or waiting for this daemon to be
    // gone (spec 2026-10-27-self-update, 7.6).
    let mut probation = None;
    if let Some((paths, launcher)) = update()? {
        match probation::on_start(&paths) {
            Start::Normal => {}
            Start::Probation => probation = Some((paths, launcher)),
            Start::Exit => return Ok(()),
            Start::LaunchHelperAndExit => return probation::launch_helper(&paths, &*launcher).map_err(|err| Error::Io(std::io::Error::other(err))),
        }
    }
    // A second daemon fails here and leaves the running one's error file alone.
    let _lock = acquire_lock(&dir)?;
    let error_file = dir.join(LAST_START_ERROR_FILE);
    let window = Opener::default();
    let (runtime, listener, registry, ui) = start(&dir, started, &window, probation.is_some()).inspect_err(|err| {
        let _ = std::fs::write(&error_file, format!("{}\n", err.to_string().replace(['\r', '\n'], " ")));
    })?;
    let _ = std::fs::remove_file(&error_file);
    let exit = Arc::clone(&ui.exit);
    let registry = Arc::new(registry);
    if let Some((paths, launcher)) = probation {
        let activity = Activity {
            gate: ui.gate.clone(),
            triggers: Arc::clone(&ui.activity),
            pending: Arc::clone(&ui.pending),
            runs: Arc::clone(&ui.runs),
            modules: ui.accounts.modules().handles(),
            host_events: ui.host_events.clone(),
        };
        let (registry, exit) = (Arc::clone(&registry), Arc::clone(&exit));
        runtime.spawn(async move {
            let status = || registry.status().unwrap_or_default();
            probation::watch(&paths, &activity, &*launcher, status, tokio::time::Instant::from_std(started), &exit).await;
        });
    }
    // Never println!: it panics when the launcher has closed stdout.
    let _ = writeln!(std::io::stdout(), "open_mess daemon listening on http://127.0.0.1:{}/mcp", listener.local_addr()?.port());
    let (stop, stopped) = oneshot::channel::<()>();
    let serve = api::serve(listener, ui.token.clone(), registry, window.clone(), async {
        let _ = stopped.await;
    });
    // The API ending on its own ends the daemon too, tray and all.
    let ended = Arc::new(Notify::new());
    let server = runtime.spawn({
        let ended = Arc::clone(&ended);
        async move {
            let served = serve.await;
            ended.notify_one();
            served
        }
    });
    if let Err(err) = ui::run(&runtime, ui, window.clone(), until_stopped(Arc::clone(&ended), Arc::clone(&exit))) {
        // Without the tray and the window the daemon still serves its agents; `om config` says so.
        let _ = writeln!(std::io::stderr(), "{err}");
        runtime.block_on(window.close());
        runtime.block_on(until_stopped(ended, exit));
    }
    let _ = stop.send(());
    runtime.block_on(server).map_err(|err| Error::Io(std::io::Error::other(err)))?
}

/// Ctrl+C, the API ended, or `exit` was notified.
async fn until_stopped(ended: Arc<Notify>, exit: Arc<Notify>) {
    tokio::select! {
        () = ctrl_c() => {}
        () = ended.notified() => {}
        () = exit.notified() => {}
    }
}

/// An update's folders and what starts its helper.
type Update = (Paths, Arc<dyn Launcher>);

/// None where updates do not install.
#[cfg(windows)]
fn update() -> Result<Option<Update>, Error> {
    let paths = Paths::current()?;
    let system = crate::update::system::WindowsSystem { data: paths.data.clone() };
    Ok(Some((paths, Arc::new(system))))
}

#[cfg(not(windows))]
fn update() -> Result<Option<Update>, Error> {
    Ok(None)
}

/// The instance lock, retried for half a second: a CLI checking whether a daemon runs holds it for
/// an instant (cli.rs), and must not be taken for a running daemon.
fn acquire_lock(dir: &Path) -> Result<InstanceLock, Error> {
    for _ in 0..25 {
        match InstanceLock::acquire(dir) {
            Err(Error::AlreadyRunning) => std::thread::sleep(Duration::from_millis(20)),
            result => return result,
        }
    }
    InstanceLock::acquire(dir)
}

/// Everything the daemon runs. `probation`: a new version is being checked, and the daemon starts
/// with writes, window saves and trigger deliveries held (spec 2026-10-27-self-update, 7.2).
fn start(dir: &Path, started: Instant, window: &Opener, probation: bool) -> Result<(Runtime, TcpListener, Registry, UiState), Error> {
    let store = Store::open(&dir.join("state.sqlite3"))?;
    let port = store.port()?;
    let token = secrets::local_api_token()?;
    let runtime = Runtime::new()?;
    let listener = runtime.block_on(api::bind(port))?;
    // One engine for the process; the modules keep it for loading files again.
    let engine = Engine::new()?;
    let (events, host_events) = mpsc::channel(EVENT_QUEUE);
    let event_queue = events.clone();
    let (prompts, sign_in_prompts) = mpsc::channel(SIGN_IN_QUEUE);
    let data_dir = dir.to_path_buf();
    let ctx = move |_: &str| ModuleContext {
        data_dir: data_dir.clone(),
        events: events.clone(),
        net: Net::new(AddressPolicy::Strict),
        sign_in: SignInPrompts::new(prompts.clone()),
    };
    let modules = runtime.block_on(Modules::scan(&store, &engine, &dir.join("modules"), ctx))?;
    // After the local API token: from here on keyring-core's default store is the platform one.
    let credentials = open_mess_host::credential_store().map_err(|why| Error::Secrets(keyring_core::Error::PlatformFailure(why.into())))?;
    let accounts = Arc::new(Accounts::new(store, modules, dir.to_path_buf(), credentials));
    let bus = Bus::default();
    let pending = Arc::new(Pending::new(Arc::clone(&accounts)));
    let registry = Registry::with_pending(started, Arc::clone(&accounts), Arc::clone(&pending));
    let activity = Arc::new(triggers::Activity::default());
    if probation {
        // Before the trigger engine, the approvals and the API run; the probation lets them go.
        registry.gate().set(GateMode::WritesBlocked);
        activity.hold.send_replace(true);
        pending.probation().set(true);
    }
    let runs = {
        // Inside the runtime: the engine spawns its tasks there.
        let _entered = runtime.enter();
        // Files of runs a daemon that stopped left behind are deleted here (spec 8.7, R6).
        let counters = registry.trigger_counters().clone();
        let runs = Runs::new(dir.join("runs"), spawn::RUN_TIMEOUT, counters.clone());
        let actions = Actions { accounts: Arc::clone(&accounts), sessions: registry.channel_sessions().clone(), runs: Arc::clone(&runs) };
        triggers::engine::spawn(&bus, Arc::clone(&accounts), Arc::new(actions), counters, Arc::clone(&activity), || chrono::Local::now().time());
        runs
    };
    runtime.spawn(bus.forward(host_events, Arc::clone(&accounts), Arc::clone(&activity)));
    let sign_ins = SignIns::default();
    runtime.spawn(sign_ins.clone().forward(sign_in_prompts));
    // Sessions come back in the background; the tools answer meanwhile.
    runtime.spawn({
        let (accounts, restored) = (Arc::clone(&accounts), Arc::clone(registry.accounts_restored()));
        async move {
            // ponytail: a failed restore leaves the stored states; reported once a log sink exists.
            let _ = accounts.restore().await;
            restored.store(true, Ordering::SeqCst);
        }
    });
    pending.recover()?;
    runtime.spawn(Arc::clone(&pending).sweep());
    approval::start(&runtime, Arc::clone(&pending), window.clone());
    let (counters, sessions) = (registry.trigger_counters().clone(), registry.channel_sessions().clone());
    let (gate, ui_ready) = (registry.gate().clone(), Arc::clone(registry.ui_ready()));
    let ui = UiState {
        accounts,
        probation: pending.probation().clone(),
        pending,
        started,
        port,
        token,
        sign_ins,
        counters,
        sessions,
        gate,
        activity,
        runs,
        host_events: event_queue,
        ui_ready,
        exit: Arc::default(),
    };
    Ok((runtime, listener, registry, ui))
}

/// Host events waiting for the daemon; a full queue holds the reporting module back.
const EVENT_QUEUE: usize = 256;
/// Sign-in prompts waiting for the window's side; only one module signs in at a time.
const SIGN_IN_QUEUE: usize = 8;

async fn ctrl_c() {
    // Without a Ctrl+C handler the daemon runs until it is killed, rather than stopping at once.
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}
