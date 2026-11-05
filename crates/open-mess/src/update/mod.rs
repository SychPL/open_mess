//! Self-update: signed release manifests, downloads and installation (spec 2026-10-27-self-update).

pub mod apply;
pub mod consent;
pub mod fetch;
pub mod probation;
pub mod quiesce;
pub mod release;
pub mod service;
pub mod state;
#[cfg(windows)]
pub mod system;

/// Everything that can go wrong while checking for, downloading or installing an update.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// The release is not acceptable (bad signature, bad manifest, wrong file); never retried as is.
    #[error("release rejected: {0}")]
    Rejected(String),
    /// The network failed; worth retrying later.
    #[error("network: {0}")]
    Network(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("state database: {0}")]
    Store(#[from] rusqlite::Error),
    /// The helper did not take `update.lock` in time; the daemon went back to work.
    #[error("the update helper did not start")]
    HelperNotStarted,
    /// The helper may or may not run, and `update-pending.json` could not be deleted: the daemon
    /// exits, and the recovery at its next start takes over (spec 7.6).
    #[error("update-pending.json could not be withdrawn ({0}); the daemon exits and the recovery takes over")]
    StageFileLeft(std::io::Error),
}
