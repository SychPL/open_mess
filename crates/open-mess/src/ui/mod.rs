//! The tray icon and the configuration window (spec 8.8). On Windows `om daemon` runs the window's
//! event loop on the main thread while the daemon's runtime serves the API on its own threads; the
//! window exists only while it is open. Elsewhere there is no tray and no window: [`run`] only waits
//! for the daemon to be stopped.

pub mod commands;
#[cfg(windows)]
mod window;

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::Error;
use crate::accounts::Accounts;
use crate::pending::Pending;

/// What the window's commands work on.
pub struct UiState {
    pub accounts: Arc<Accounts>,
    pub pending: Arc<Pending>,
    /// When the daemon started.
    pub started: Instant,
    /// The local API's port and token, for the command that connects an agent.
    pub port: u16,
    pub token: String,
}

/// One request to open the window, answered once the window is open or failed to open.
pub type OpenRequest = oneshot::Sender<Result<(), String>>;

/// Requests to open the window, from `POST /api/ui/open` (`om config`), for the tray's event loop.
#[derive(Clone)]
pub struct Opener {
    requests: mpsc::Sender<OpenRequest>,
    queue: Arc<Mutex<mpsc::Receiver<OpenRequest>>>,
}

/// Open requests waiting for the event loop; more are refused.
const OPEN_QUEUE: usize = 8;
/// The longest a request waits for the window: creating the first webview can take a while.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

impl Default for Opener {
    fn default() -> Opener {
        let (requests, queue) = mpsc::channel(OPEN_QUEUE);
        Opener { requests, queue: Arc::new(Mutex::new(queue)) }
    }
}

impl Opener {
    /// Asks for the window and waits until it is open, so a window that cannot open is an error
    /// for whoever asked.
    pub async fn open(&self) -> Result<(), Error> {
        if !cfg!(windows) {
            return Err(Error::NoWindow);
        }
        let (reply, answer) = oneshot::channel();
        self.requests.try_send(reply).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => Error::Ui("the window is already being opened".into()),
            mpsc::error::TrySendError::Closed(_) => Error::Ui(UNAVAILABLE.into()),
        })?;
        match tokio::time::timeout(OPEN_TIMEOUT, answer).await {
            Ok(Ok(opened)) => opened.map_err(Error::Ui),
            Ok(Err(_)) => Err(Error::Ui(UNAVAILABLE.into())),
            Err(_) => Err(Error::Ui(format!("the window did not open within {} s", OPEN_TIMEOUT.as_secs()))),
        }
    }

    /// The next request, for the event loop; None once the opener is closed.
    pub async fn next(&self) -> Option<OpenRequest> {
        self.queue.lock().await.recv().await
    }

    /// No window will open (the tray could not start): every request fails at once.
    pub async fn close(&self) {
        let mut queue = self.queue.lock().await;
        queue.close();
        // Dropped unanswered, they fail now rather than at their timeout.
        while queue.try_recv().is_ok() {}
    }
}

const UNAVAILABLE: &str = "the configuration window is not available - see the daemon's output";

/// Shows the tray icon and opens the window on request, until the user chooses `Zakończ` or `quit`
/// completes. Blocks; call it on the main thread.
#[cfg(windows)]
pub fn run(runtime: &Runtime, state: UiState, opener: Opener, quit: impl Future<Output = ()> + Send + 'static) -> Result<(), Error> {
    window::run(runtime, state, opener, quit)
}

#[cfg(not(windows))]
pub fn run(runtime: &Runtime, _state: UiState, _opener: Opener, quit: impl Future<Output = ()> + Send + 'static) -> Result<(), Error> {
    runtime.block_on(quit);
    Ok(())
}
