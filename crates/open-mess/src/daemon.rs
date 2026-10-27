//! `om daemon`: the background process that serves the local API.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{Engine, ModuleContext, Net};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use crate::accounts::Accounts;
use crate::events::Bus;
use crate::instance::InstanceLock;
use crate::modules::Modules;
use crate::store::Store;
use crate::tools::Registry;
use crate::triggers::spawn::{self, Runs};
use crate::triggers::{self, Actions};
use crate::{Error, api, approval, paths, secrets};

/// Why the last `om daemon` failed to start, one line, in the data folder. The CLI shows it when
/// it can't reach the daemon; a successful start deletes it.
pub const LAST_START_ERROR_FILE: &str = "last-start-error.txt";

/// Runs the daemon until Ctrl+C.
pub fn run() -> Result<(), Error> {
    let started = Instant::now();
    let dir = paths::data_dir()?;
    // A second daemon fails here and leaves the running one's error file alone.
    let _lock = acquire_lock(&dir)?;
    let error_file = dir.join(LAST_START_ERROR_FILE);
    let (runtime, listener, token, registry) = start(&dir, started).inspect_err(|err| {
        let _ = std::fs::write(&error_file, format!("{}\n", err.to_string().replace(['\r', '\n'], " ")));
    })?;
    let _ = std::fs::remove_file(&error_file);
    // Never println!: it panics when the launcher has closed stdout.
    let _ = writeln!(std::io::stdout(), "open_mess daemon listening on http://127.0.0.1:{}/mcp", listener.local_addr()?.port());
    runtime.block_on(api::serve(listener, token, Arc::new(registry), ctrl_c()))
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

fn start(dir: &Path, started: Instant) -> Result<(Runtime, TcpListener, String, Registry), Error> {
    let store = Store::open(&dir.join("state.sqlite3"))?;
    let port = store.port()?;
    let token = secrets::local_api_token()?;
    let runtime = Runtime::new()?;
    let listener = runtime.block_on(api::bind(port))?;
    // One engine for the process; the modules it runs keep it alive.
    let engine = Engine::new()?;
    let (events, host_events) = mpsc::channel(EVENT_QUEUE);
    let ctx = |_: &str| ModuleContext { data_dir: dir.to_path_buf(), events: events.clone(), net: Net::new(AddressPolicy::Strict) };
    let modules = runtime.block_on(Modules::scan(&store, &engine, &dir.join("modules"), ctx))?;
    // After the local API token: from here on keyring-core's default store is the platform one.
    let credentials = open_mess_host::credential_store().map_err(|why| Error::Secrets(keyring_core::Error::PlatformFailure(why.into())))?;
    let accounts = Arc::new(Accounts::new(store, modules, dir.to_path_buf(), credentials));
    let bus = Bus::default();
    let registry = Registry::new(started, Arc::clone(&accounts));
    {
        // Inside the runtime: the engine spawns its tasks there.
        let _entered = runtime.enter();
        // Files of runs a daemon that stopped left behind are deleted here (spec 8.7, R6).
        let actions = Actions { accounts: Arc::clone(&accounts), runs: Runs::new(dir.join("runs"), spawn::RUN_TIMEOUT) };
        let counters = registry.trigger_counters().clone();
        triggers::engine::spawn(&bus, Arc::clone(&accounts), Arc::new(actions), counters, || chrono::Local::now().time());
    }
    runtime.spawn(bus.forward(host_events, Arc::clone(&accounts)));
    // Sessions come back in the background; the tools answer meanwhile.
    runtime.spawn({
        let accounts = Arc::clone(&accounts);
        async move {
            // ponytail: a failed restore leaves the stored states; reported once a log sink exists.
            let _ = accounts.restore().await;
        }
    });
    if let Some(pending) = registry.pending() {
        pending.recover()?;
        runtime.spawn(Arc::clone(pending).sweep());
        approval::start(&runtime, Arc::clone(pending));
    }
    Ok((runtime, listener, token, registry))
}

/// Host events waiting for the daemon; a full queue holds the reporting module back.
const EVENT_QUEUE: usize = 256;

async fn ctrl_c() {
    // Without a Ctrl+C handler the daemon runs until it is killed, rather than stopping at once.
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}
