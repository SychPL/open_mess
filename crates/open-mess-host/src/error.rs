//! Errors a module call can end with.

/// Why the host could not load or run a module call.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The file is not a component of the `platform-module` world, or its manifest is refused.
    #[error("module not loaded: {0}")]
    Load(String),
    /// The call ran past its time limit; the module is unloaded and reloaded on the next call.
    #[error("module call timed out")]
    Timeout,
    /// The module trapped (for example ran out of memory); it is unloaded like after a timeout.
    #[error("module crashed: {0}")]
    Crashed(String),
    /// The module ran and returned an error of its own.
    #[error(transparent)]
    Module(ModuleError),
    /// The module is no longer running (disabled after repeated crashes, or shut down).
    #[error("module is not running")]
    Closed,
}

/// Mirrors `module-error` in `wit/module.wit`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModuleError {
    #[error("sign-in required: {0}")]
    AuthRequired(String),
    #[error("incompatible server response: {0}")]
    Incompatible(String),
    #[error("platform error: {0}")]
    Platform(String),
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("not supported: {0}")]
    NotSupported(String),
    /// A write may have reached the platform; it is never run again by itself (spec 8.5).
    #[error("uncertain: {0}")]
    Uncertain(String),
}
