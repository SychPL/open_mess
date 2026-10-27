//! Self-update: signed release manifests, downloads and installation (spec 2026-10-27-self-update).

pub mod fetch;
pub mod release;

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
}
