//! The tray icon and the configuration window (spec 8.8). On Windows `om daemon` runs the window's
//! event loop on the main thread while the daemon's runtime serves the API on its own threads; the
//! window exists only while it is open. Elsewhere there is no tray and no window: [`run`] only waits
//! for the daemon to be stopped.

pub mod commands;
#[cfg(windows)]
mod window;

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use tokio::runtime::Runtime;
use tokio::sync::Notify;

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

/// Requests to open the window, from `POST /api/ui/open` (`om config`). Requests that arrive while
/// one waits are the same request.
#[derive(Clone, Default)]
pub struct Opener(Arc<Notify>);

impl Opener {
    pub fn open(&self) -> Result<(), Error> {
        if cfg!(windows) {
            self.0.notify_one();
            Ok(())
        } else {
            Err(Error::NoWindow)
        }
    }

    /// Waits for the next request.
    pub async fn requested(&self) {
        self.0.notified().await;
    }
}

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
