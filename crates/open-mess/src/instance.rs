//! At most one daemon per Windows user.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::Error;

/// An exclusive lock on `<data dir>/daemon.lock`, held for the daemon's lifetime.
///
/// The operating system releases the lock when the file is closed, so a crashed daemon never
/// leaves a stale lock behind.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Takes the lock, or fails with [`Error::AlreadyRunning`] when another daemon holds it.
    pub fn acquire(dir: &Path) -> Result<InstanceLock, Error> {
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("daemon.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(InstanceLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(Error::AlreadyRunning),
            Err(TryLockError::Error(err)) => Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn second_lock_fails() {
        let dir = tempfile::tempdir().unwrap();
        let _first = InstanceLock::acquire(dir.path()).unwrap();
        assert!(matches!(InstanceLock::acquire(dir.path()), Err(Error::AlreadyRunning)));
    }

    #[test]
    fn lock_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        drop(InstanceLock::acquire(dir.path()).unwrap());
        assert!(InstanceLock::acquire(dir.path()).is_ok());
    }
}
