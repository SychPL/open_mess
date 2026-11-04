//! Secrets in a file, for open_mess outside Windows (spec 2026-11-03-paperclip-plugin, 5.2): a server
//! container has no credential manager. `<data>/secrets.json` maps service to user to secret, has
//! mode 0600, and is written whole and atomically on every change. Only the daemon writes; the CLI
//! reads. No encryption: its key would lie next to it.

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use keyring_core::api::{CredentialApi, CredentialStoreApi};
use keyring_core::{Entry, Error as KeyringError, Result as KeyringResult};

use crate::Error;

/// The file's name in the data folder.
pub const FILE: &str = "secrets.json";
/// The largest file read or written.
pub const MAX_FILE: u64 = 1024 * 1024;

type Secrets = BTreeMap<String, BTreeMap<String, String>>;

/// The store. Clones share the file and its lock.
#[derive(Debug, Clone)]
pub struct FileStore(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    path: PathBuf,
    /// Held over each read, change and write, so changes made at once don't lose each other.
    lock: Mutex<()>,
}

impl FileStore {
    /// The store of data folder `dir`. Narrows the folder and the file to their owner, and fails
    /// on a file it can't read, which it leaves alone so no secret is lost.
    pub fn open(dir: &Path) -> Result<FileStore, Error> {
        restrict(dir, 0o700)?;
        let store = FileStore(Arc::new(Inner { path: dir.join(FILE), lock: Mutex::new(()) }));
        if store.0.path.exists() {
            restrict(&store.0.path, 0o600)?;
        }
        store.read().map_err(|err| Error::SecretsFile(err.to_string()))?;
        Ok(store)
    }

    fn read(&self) -> KeyringResult<Secrets> {
        let file = match File::open(&self.0.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Secrets::new()),
            Err(err) => return Err(failure(err)),
        };
        let mut text = String::new();
        file.take(MAX_FILE + 1).read_to_string(&mut text).map_err(failure)?;
        if text.len() as u64 > MAX_FILE {
            return Err(failure(format!("{} is over {MAX_FILE} bytes", self.0.path.display())));
        }
        serde_json::from_str(&text).map_err(|err| failure(format!("{} is damaged: {err}", self.0.path.display())))
    }

    /// Writes `secrets` to a file beside, then puts it in place.
    fn write(&self, secrets: &Secrets) -> KeyringResult<()> {
        let json = serde_json::to_vec(secrets).map_err(failure)?;
        if json.len() as u64 > MAX_FILE {
            return Err(KeyringError::TooLong(FILE.into(), u32::try_from(MAX_FILE).unwrap_or(u32::MAX)));
        }
        let temp = self.0.path.with_extension("json.tmp");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temp).map_err(failure)?;
        file.write_all(&json).and_then(|()| file.sync_all()).map_err(failure)?;
        drop(file);
        std::fs::rename(&temp, &self.0.path).map_err(failure)?;
        // The rename survives a power cut only once the folder is on disk too.
        #[cfg(unix)]
        if let Some(dir) = self.0.path.parent() {
            File::open(dir).and_then(|dir| dir.sync_all()).map_err(failure)?;
        }
        Ok(())
    }

    /// Reads, lets `change` edit, and writes; all under the lock. Nothing is written when `change`
    /// fails.
    fn change(&self, change: impl FnOnce(&mut Secrets) -> KeyringResult<()>) -> KeyringResult<()> {
        let _locked = self.0.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut secrets = self.read()?;
        change(&mut secrets)?;
        self.write(&secrets)
    }
}

impl CredentialStoreApi for FileStore {
    fn vendor(&self) -> String {
        "open_mess file store".into()
    }

    fn id(&self) -> String {
        self.0.path.display().to_string()
    }

    /// Modifiers (Credential Manager's persistence) mean nothing here and are ignored.
    fn build(&self, service: &str, user: &str, _modifiers: Option<&HashMap<&str, &str>>) -> KeyringResult<Entry> {
        let credential = FileCredential { store: self.clone(), service: service.into(), user: user.into() };
        Ok(Entry::new_with_credential(Arc::new(credential)))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// One secret of the file.
#[derive(Debug)]
struct FileCredential {
    store: FileStore,
    service: String,
    user: String,
}

impl CredentialApi for FileCredential {
    fn set_secret(&self, secret: &[u8]) -> KeyringResult<()> {
        let secret = String::from_utf8(secret.to_vec()).map_err(|err| KeyringError::BadEncoding(err.into_bytes()))?;
        self.store.change(|secrets| {
            secrets.entry(self.service.clone()).or_default().insert(self.user.clone(), secret);
            Ok(())
        })
    }

    fn get_secret(&self) -> KeyringResult<Vec<u8>> {
        let secrets = self.store.read()?;
        secrets.get(&self.service).and_then(|users| users.get(&self.user)).map(|secret| secret.clone().into_bytes()).ok_or(KeyringError::NoEntry)
    }

    fn delete_credential(&self) -> KeyringResult<()> {
        self.store.change(|secrets| {
            let users = secrets.get_mut(&self.service).ok_or(KeyringError::NoEntry)?;
            users.remove(&self.user).ok_or(KeyringError::NoEntry)?;
            if users.is_empty() {
                secrets.remove(&self.service);
            }
            Ok(())
        })
    }

    fn get_credential(&self) -> KeyringResult<Option<Arc<keyring_core::api::Credential>>> {
        self.get_secret().map(|_| None)
    }

    fn get_specifiers(&self) -> Option<(String, String)> {
        Some((self.service.clone(), self.user.clone()))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn failure(err: impl std::fmt::Display) -> KeyringError {
    KeyringError::PlatformFailure(err.to_string().into())
}

/// Takes away the group's and others' access to `path` (Unix); nothing elsewhere.
#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let current = std::fs::metadata(path)?.permissions().mode();
    if current & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<(), Error> {
    Ok(())
}
