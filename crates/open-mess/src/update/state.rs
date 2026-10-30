//! What an installation in progress leaves on disk for the next step, whoever takes it: the stage
//! file `update-pending.json` and the helper's lock `update.lock` (spec 2026-10-27-self-update, 7.2).
//! Both the daemon and the helper process use them; nothing here needs the state database.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::apply::Outcome;
use crate::{Error, paths};

/// The folders an installation works in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// The data folder: the state database, the modules and the files below.
    pub data: PathBuf,
    /// `%LOCALAPPDATA%\open_mess`: the programs and the downloaded releases.
    pub local: PathBuf,
}

impl Paths {
    /// The running daemon's folders.
    pub fn current() -> Result<Paths, Error> {
        let local =
            std::env::var_os("LOCALAPPDATA").filter(|dir| !dir.is_empty()).ok_or_else(|| Error::BadConfig("LOCALAPPDATA is not set".into()))?;
        Ok(Paths { data: paths::data_dir()?, local: PathBuf::from(local).join("open_mess") })
    }

    /// `om.exe`, and the helper `om.update.exe`.
    pub fn bin(&self) -> PathBuf {
        self.local.join("bin")
    }

    /// The downloaded releases, one folder per version.
    pub fn updates(&self) -> PathBuf {
        self.local.join("updates")
    }

    pub fn pending_file(&self) -> PathBuf {
        self.data.join("update-pending.json")
    }

    /// Held by the helper for as long as it works: "the helper is alive".
    pub fn lock_file(&self) -> PathBuf {
        self.data.join("update.lock")
    }

    /// The last installation's result, for the window.
    pub fn last_result(&self) -> PathBuf {
        self.data.join("last-update.txt")
    }
}

/// How far an installation got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// The daemon launched the helper; nothing is changed yet.
    Started,
    BackedUp,
    Installed,
    RollingBack,
}

/// `update-pending.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingFile {
    pub version: String,
    pub manifest_sha256: String,
    /// The release's verified files, and the backup once there is one.
    pub update_dir: PathBuf,
    pub stage: Stage,
    /// The modules active when the daemon stopped; the new daemon must bring them all back.
    pub active_modules: Vec<String>,
    /// Files the installation added, written here before each is added, so a rollback removes them.
    pub added: Vec<PathBuf>,
    pub replaced: Vec<PathBuf>,
}

/// What reading `update-pending.json` found.
#[derive(Debug)]
pub enum Read {
    Missing,
    /// Read, but not a stage file: a write never leaves half a file, so something else broke it.
    Unreadable,
    /// Not read, even after a few tries (an antivirus holding it, say): leave everything alone and
    /// look again later.
    Error(io::Error),
    Found(PendingFile),
}

/// A stage file is a few lists of paths; anything bigger is not one.
const MAX_FILE: u64 = 1024 * 1024;

/// Tries 5 times, 100 ms apart, before an error counts.
pub fn read(path: &Path) -> Read {
    let mut tries = 5;
    let bytes = loop {
        match read_capped(path) {
            Ok(bytes) => break bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Read::Missing,
            Err(_) if tries > 1 => {
                tries -= 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Read::Error(err),
        }
    };
    if bytes.len() as u64 > MAX_FILE {
        return Read::Unreadable;
    }
    serde_json::from_slice(&bytes).map_or(Read::Unreadable, Read::Found)
}

fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Writes the whole file next to `path` and moves it over `path`: a reader, or the next step after
/// a power loss, finds the old file or the new one, never part of one.
pub fn write(path: &Path, file: &PendingFile) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut out = File::create(&temp)?;
    out.write_all(&bytes)?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&temp, path)
}

/// The lock on `path`, tried every 100 ms for `wait`; None when another process holds it. The
/// operating system releases it when the file is closed, so a helper that died leaves no lock.
pub fn try_lock(path: &Path, wait: Duration) -> io::Result<Option<File>> {
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    let mut tries = wait.as_millis() / 100;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) if tries > 0 => {
                tries -= 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(err)) => return Err(err),
        }
    }
}

/// Whether a helper holds `update.lock` now. Taking it for a moment to look is harmless: the helper
/// tries again for 2 s.
pub fn helper_alive(paths: &Paths) -> bool {
    matches!(try_lock(&paths.lock_file(), Duration::ZERO), Ok(None))
}

/// The installation's result, one line, for the window: always an [`Outcome`], in its English form.
/// Best effort: one that cannot be written is only not shown.
pub fn record_result(paths: &Paths, outcome: &Outcome) {
    let _ = std::fs::write(paths.last_result(), format!("{}\n", outcome.to_string().replace(['\r', '\n'], " ")));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_does_not_parse_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-pending.json");
        assert!(matches!(read(&path), Read::Missing));
        std::fs::write(&path, "{\"version\": ").unwrap();
        assert!(matches!(read(&path), Read::Unreadable));
        std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(matches!(read(&path), Read::Unreadable));
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_error_not_unreadable() {
        // A folder in its place fails to open on every try, like a file an antivirus keeps locked.
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(read(dir.path()), Read::Error(_)));
    }
}
