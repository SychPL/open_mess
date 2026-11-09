//! `om daemon`: the background process that serves the local API, with the tray icon and the
//! configuration window on Windows.

use std::io::Write as _;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{Engine, ModuleContext, Net, SignInPrompts};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::{Notify, mpsc, oneshot};
use tracing_subscriber::filter::{LevelFilter, Targets};

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
use crate::update::fetch::Source;
use crate::update::probation::{self, Start};
use crate::update::quiesce::{Activity, Launcher};
use crate::update::release::RELEASE_KEY;
use crate::update::service::{self, Setup, UpdateService};
use crate::update::state::Paths;
use crate::{Error, api, approval, i18n, paths, secrets, ui};

/// Why the last `om daemon` failed to start, one line, in the data folder. The CLI shows it when
/// it can't reach the daemon; a successful start deletes it.
pub const LAST_START_ERROR_FILE: &str = "last-start-error.txt";

/// Runs the daemon until Ctrl+C, an update hands over to its helper or, on Windows, `Zakończ` in the
/// tray menu. The tray and the configuration window take the main thread; the API runs on the
/// runtime's threads.
pub fn run() -> Result<(), Error> {
    // Diagnostics only: OPEN_MESS_LOG (error, warn, info, debug or trace) prints the daemon's and
    // the modules' logs to stderr. Logs carry states, codes and counts, never tokens or message
    // contents (spec 8.4).
    if let Some(level) = std::env::var("OPEN_MESS_LOG").ok().and_then(|level| level.parse::<LevelFilter>().ok()) {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_ansi(false))
            .with(log_targets(level))
            .try_init()
            .ok();
    }
    let started = Instant::now();
    let dir = paths::data_dir()?;
    let error_file = dir.join(LAST_START_ERROR_FILE);
    let record = |why: &str| drop(std::fs::write(&error_file, format!("{}\n", why.replace(['\r', '\n'], " "))));
    // First of all: an update's helper may be changing the files, or waiting for this daemon to be
    // gone (spec 2026-10-27-self-update, 7.6).
    let update = update()?;
    let mut probation = false;
    if let Some((paths, launcher)) = &update {
        match probation::on_start(paths) {
            Start::Normal => {}
            Start::Probation => probation = true,
            Start::Exit => {
                record("an update of open_mess is being installed or rolled back; its helper starts the daemon when it is done");
                return Ok(());
            }
            Start::LaunchHelperAndExit => {
                let launched = probation::launch_helper(paths, &**launcher);
                record(&match &launched {
                    Ok(()) => "an interrupted update of open_mess is being rolled back; its helper starts the daemon when it is done".to_owned(),
                    Err(err) => {
                        format!("an interrupted update of open_mess is not rolled back yet ({err}); the scheduled tasks try again within 5 minutes")
                    }
                });
                return launched.map_err(|err| Error::Io(std::io::Error::other(err)));
            }
        }
    }
    // A second daemon fails here and leaves the running one's error file alone.
    let _lock = acquire_lock(&dir)?;
    let window = Opener::default();
    let (runtime, listener, registry, ui, admin_token) =
        start(&dir, started, &window, probation, update.clone()).inspect_err(|err| record(&err.to_string()))?;
    let _ = std::fs::remove_file(&error_file);
    let exit = Arc::clone(&ui.exit);
    let registry = Arc::new(registry);
    runtime.spawn(Arc::clone(&registry).watch_tools());
    runtime.spawn(Arc::clone(&ui.accounts).restore_after_crashes());
    let accounts = Arc::clone(&ui.accounts);
    if let Some((paths, launcher)) = update.filter(|_| probation) {
        let activity = Activity {
            gate: ui.gate.clone(),
            triggers: Arc::clone(&ui.activity),
            pending: Arc::clone(&ui.pending),
            runs: Arc::clone(&ui.runs),
            modules: ui.accounts.modules().handles(),
            host_events: ui.host_events.clone(),
            recording: ui.recorder.as_ref().map(|recorder| recorder.link()).unwrap_or_default(),
        };
        let (registry, exit) = (Arc::clone(&registry), Arc::clone(&exit));
        runtime.spawn(async move {
            let status = || registry.status().unwrap_or_default();
            probation::watch(&paths, &activity, &*launcher, status, tokio::time::Instant::from_std(started), &exit).await;
        });
    }
    // Never println!: it panics when the launcher has closed stdout.
    let _ = writeln!(std::io::stdout(), "open_mess daemon listening on http://{}/mcp", listener.local_addr()?);
    let (stop, stopped) = oneshot::channel::<()>();
    let token = ui.token.clone();
    // A server has no window: the window's state goes to the administrative API instead (spec
    // 2026-11-03-paperclip-plugin, 5.4).
    let (window_state, admin) = match admin_token {
        Some(token) => (None, Some(api::Admin { token, ui: Arc::new(ui) })),
        None => (Some(ui), None),
    };
    let serve = api::serve_with_admin(listener, token, admin, registry, window.clone(), async {
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
    let quit = until_stopped(Arc::clone(&ended), Arc::clone(&exit));
    let shown = match window_state {
        Some(ui) => ui::run(&runtime, ui, window.clone(), quit),
        None => {
            runtime.block_on(quit);
            Ok(())
        }
    };
    if let Err(err) = shown {
        // Without the tray and the window the daemon still serves its agents; `om config` says so.
        let _ = writeln!(std::io::stderr(), "{err}");
        runtime.block_on(window.close());
        runtime.block_on(until_stopped(ended, exit));
    }
    // Before the modules go with the process: what others see of their sessions ends (spec
    // 2026-10-29-live-captions, 4.3), and no join, the captions archive's either, gets in after it
    // (spec 2026-11-04-captions-archive, 4.4).
    let _block = accounts.module_writes().block();
    runtime.block_on(accounts.module_writes().finished());
    runtime.block_on(accounts.end_sessions(&accounts.modules().handles()));
    let _ = stop.send(());
    runtime.block_on(server).map_err(|err| Error::Io(std::io::Error::other(err)))?
}

/// Ctrl+C, SIGTERM (`docker stop`, Unix), the API ended, or `exit` was notified.
async fn until_stopped(ended: Arc<Notify>, exit: Arc<Notify>) {
    tokio::select! {
        () = ctrl_c() => {}
        () = terminate() => {}
        () = ended.notified() => {}
        () = exit.notified() => {}
    }
}

/// The address the API listens on: 127.0.0.1, or with `OPEN_MESS_LISTEN=0.0.0.0` every interface
/// of a server container (spec 2026-11-03-paperclip-plugin, 5.1). Nothing else: the CLI always
/// calls 127.0.0.1, which 0.0.0.0 covers and an interface's own address would not. Windows listens
/// on 127.0.0.1 only, whatever the variable says (spec 8.1).
pub fn listen_ip(listen: Option<&str>, windows: bool) -> Result<Ipv4Addr, Error> {
    match listen {
        _ if windows => Ok(Ipv4Addr::LOCALHOST),
        None => Ok(Ipv4Addr::LOCALHOST),
        Some("0.0.0.0") => Ok(Ipv4Addr::UNSPECIFIED),
        Some(other) => Err(Error::BadConfig(format!("OPEN_MESS_LISTEN must be 0.0.0.0 or unset, not {other:?}"))),
    }
}

/// An update's folders and what starts its helper.
type Update = (Paths, Arc<dyn Launcher>);

/// What `OPEN_MESS_LOG` prints: open_mess's own targets only (`open_mess` and `open_mess_host`, by
/// prefix). A dependency's debug log, like rmcp's, prints whole tool results (Codex review of 0cff6c1).
fn log_targets(level: LevelFilter) -> Targets {
    Targets::new().with_target("open_mess", level)
}

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
/// `update`: where updates install, the service that checks for them.
fn start(
    dir: &Path,
    started: Instant,
    window: &Opener,
    probation: bool,
    update: Option<Update>,
) -> Result<(Runtime, TcpListener, Registry, UiState, Option<String>), Error> {
    // Before any secret: outside Windows they live in the data folder.
    secrets::install_platform_store(dir)?;
    let store = Store::open(&dir.join("state.sqlite3"))?;
    // Before anything shows a text: the tray, the toasts, the window.
    i18n::apply_language(&store);
    let port = store.port()?;
    let ip = listen_ip(std::env::var("OPEN_MESS_LISTEN").ok().as_deref(), cfg!(windows))?;
    let token = secrets::local_api_token()?;
    // The administrative API exists only where there is no window (spec 2026-11-03-paperclip-plugin, 5.3).
    let admin_token = if cfg!(windows) { None } else { Some(secrets::admin_api_token()?) };
    let runtime = Runtime::new()?;
    let listener = runtime.block_on(api::bind_on(ip, port))?;
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
    let mut registry = Registry::with_pending(started, Arc::clone(&accounts), Arc::clone(&pending));
    // In %LOCALAPPDATA%, which only Windows has (spec 2026-10-28-local-transcription, 4.4); elsewhere
    // the recording tools say they are not supported.
    if let Some((paths, _)) = &update {
        use crate::recording::queue;
        let local = paths.local.clone();
        let store = Arc::clone(&accounts);
        // The window's setting: `auto` (or none) lets the model find the language (4.3).
        let language = Arc::new(move || store.store().get_config("recording.language").ok().flatten().filter(|code| code != "auto"));
        // ponytail: a queue whose thread cannot start leaves recordings waiting; reported once a log sink exists.
        let (engine, ready) = queue::whisper(local.clone());
        let queue = queue::Queue::start(&local, engine, ready, language).ok().map(Arc::new);
        registry.set_recordings(crate::recording::Recordings::new(&local), queue);
    }
    let models = update.as_ref().and_then(|(paths, _)| {
        let base = url::Url::parse(crate::recording::model::BASE).ok()?;
        Some(Arc::new(crate::recording::model::Models::new(&paths.local, base, runtime.handle().clone())))
    });
    let recorder = recorder(&update, registry.queue(), Arc::clone(registry.ui_ready()), &accounts, models.as_ref());
    let captions = recorder.as_ref().zip(update.as_ref()).map(|((_, toasts), (paths, _))| {
        use crate::recording::captions::{Archiver, Setup};
        // Inside the runtime: a toast's "Zatrzymaj" runs there.
        let _entered = runtime.enter();
        let free = paths.local.clone();
        let archiver = Archiver::new(
            crate::recording::Recordings::new(&paths.local),
            Setup {
                accounts: Arc::clone(&accounts),
                gate: registry.gate().clone(),
                probation: pending.probation().clone(),
                ui_ready: Arc::clone(registry.ui_ready()),
                notices: toasts.clone(),
                free_disk: Box::new(move || crate::recording::recorder::free_disk(&free)),
            },
        );
        attach_archiver(toasts, &archiver);
        runtime.spawn(Arc::clone(&archiver).run());
        archiver
    });
    let recorder = recorder.map(|(recorder, _)| recorder);
    if let Some(recorder) = &recorder {
        registry.set_recorder(Arc::clone(recorder));
    }
    if let Some(archiver) = &captions {
        registry.set_archiver(Arc::clone(archiver));
    }
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
    // A sign-in page off the module's approved hosts is refused before any front end shows it.
    let page_accounts = Arc::clone(&accounts);
    runtime.spawn(sign_ins.clone().forward(sign_in_prompts, move |request| ui::sign_in_page::page_allowed(&page_accounts, request)));
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
    let exit = Arc::new(Notify::new());
    // The built-in URL always parses; were it not to, there would be no updates, never no daemon.
    let update = update.zip(Source::github().ok()).map(|((paths, launcher), source)| {
        // Inside the runtime: the service spawns its checks there.
        let _entered = runtime.enter();
        UpdateService::spawn(Setup {
            installable: service::installable_here(&paths),
            paths,
            source,
            public_key: RELEASE_KEY.to_owned(),
            accounts: Arc::clone(&accounts),
            activity: Activity {
                gate: registry.gate().clone(),
                triggers: Arc::clone(&activity),
                pending: Arc::clone(&pending),
                runs: Arc::clone(&runs),
                modules: Vec::new(),
                host_events: event_queue.clone(),
                recording: recorder.as_ref().map(|recorder| recorder.link()).unwrap_or_default(),
            },
            launcher,
            exit: Arc::clone(&exit),
            announce: Arc::new(approval::update_ready),
        })
    });
    if let Some(update) = &update {
        registry.set_update(update.clone());
    }
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
        exit,
        update,
        models,
        recorder,
        captions,
    };
    Ok((runtime, listener, registry, ui, admin_token))
}

/// What the recording's toasts show and stop: on Windows the toasts, elsewhere nothing.
#[cfg(windows)]
type Toasts = Arc<crate::recording::notice::Toasts>;
#[cfg(not(windows))]
type Toasts = Arc<dyn crate::recording::captions::Notices>;

#[cfg(windows)]
fn attach_archiver(toasts: &Toasts, archiver: &Arc<crate::recording::captions::Archiver>) {
    toasts.attach_archiver(archiver);
}

#[cfg(not(windows))]
fn attach_archiver(_toasts: &Toasts, _archiver: &Arc<crate::recording::captions::Archiver>) {}

/// The recorder of the calls and its toasts, where they are recorded (Windows, spec
/// 2026-10-28-local-transcription); the captions archive shows its toasts with them.
#[cfg(windows)]
fn recorder(
    update: &Option<Update>,
    queue: Option<Arc<crate::recording::queue::Queue>>,
    ui_ready: Arc<std::sync::atomic::AtomicBool>,
    accounts: &Arc<Accounts>,
    models: Option<&Arc<crate::recording::model::Models>>,
) -> Option<(Arc<crate::recording::recorder::Recorder>, Toasts)> {
    use crate::recording::model::Download;
    use crate::recording::recorder::{self, Settings};
    let ((paths, _), queue, models) = (update.as_ref()?, queue?, Arc::clone(models?));
    let store = Arc::clone(accounts);
    let settings = Box::new(move || {
        let store = store.store();
        let setting = |key: &str| store.get_config(key).ok().flatten();
        // The automatic mode is off unless the window turned it on (4.6); the microphone is on.
        Settings { auto: setting("recording.auto").as_deref() == Some("true"), microphone: setting("recording.mic").as_deref() != Some("false") }
    });
    let model = Box::new(move || {
        let view = models.view();
        match view.download {
            Download::Running { .. } => "downloading",
            _ if view.installed.is_none() => "missing",
            _ => "ready",
        }
    });
    // ponytail: a recorder whose threads cannot start records nothing; reported once a log sink exists.
    let toasts = Arc::new(crate::recording::notice::Toasts::new().ok()?);
    let recorder = recorder::start(&paths.local, queue, ui_ready, settings, model, Arc::clone(&toasts)).ok()?;
    Some((recorder, toasts))
}

#[cfg(not(windows))]
fn recorder(
    _update: &Option<Update>,
    _queue: Option<Arc<crate::recording::queue::Queue>>,
    _ui_ready: Arc<std::sync::atomic::AtomicBool>,
    _accounts: &Arc<Accounts>,
    _models: Option<&Arc<crate::recording::model::Models>>,
) -> Option<(Arc<crate::recording::recorder::Recorder>, Toasts)> {
    None
}

/// Host events waiting for the daemon; a full queue holds the reporting module back.
const EVENT_QUEUE: usize = 256;
/// Sign-in prompts waiting for the window's side; only one module signs in at a time.
const SIGN_IN_QUEUE: usize = 8;

#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut sigterm) => {
            let _ = sigterm.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await;
}

async fn ctrl_c() {
    // Without a Ctrl+C handler the daemon runs until it is killed, rather than stopping at once.
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_or_every_interface_is_listened_on() {
        assert_eq!(listen_ip(None, false).unwrap(), Ipv4Addr::LOCALHOST);
        assert_eq!(listen_ip(Some("0.0.0.0"), false).unwrap(), Ipv4Addr::UNSPECIFIED);
        // An interface's own address would cut off the CLI, which calls 127.0.0.1.
        for wrong in ["::", "10.0.0.5", "localhost", "abc", "", "127.0.0.1"] {
            assert!(listen_ip(Some(wrong), false).is_err(), "{wrong}");
        }
        // Windows listens on loopback only, whatever the variable says.
        assert_eq!(listen_ip(Some("0.0.0.0"), true).unwrap(), Ipv4Addr::LOCALHOST);
        assert_eq!(listen_ip(Some("abc"), true).unwrap(), Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn diagnostics_print_only_open_mess_logs() {
        let targets = log_targets(LevelFilter::DEBUG);
        let debug = LevelFilter::DEBUG.into_level().unwrap();
        let error = LevelFilter::ERROR.into_level().unwrap();
        let trace = LevelFilter::TRACE.into_level().unwrap();
        assert!(targets.would_enable("open_mess::daemon", &debug));
        assert!(targets.would_enable("open_mess_host::bindings", &debug));
        assert!(!targets.would_enable("open_mess_host::bindings", &trace));
        // rmcp logs whole tool results at debug; not even its errors are printed.
        assert!(!targets.would_enable("rmcp::service", &error));
        assert!(!targets.would_enable("hyper", &error));
    }
}
