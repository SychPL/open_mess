//! open_mess: the daemon, its local API and the `om` CLI.

pub mod api;
pub mod cli;
pub mod instance;
pub mod paths;
pub mod secrets;
pub mod store;
pub mod tools;

/// Everything that can go wrong in the daemon and the CLI.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("open_mess daemon is already running")]
    AlreadyRunning,
    #[error("cannot find the data folder: APPDATA is not set (set OPEN_MESS_DATA_DIR to choose one)")]
    NoDataDir,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
