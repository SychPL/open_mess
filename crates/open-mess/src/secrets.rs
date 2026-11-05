//! Secrets in Windows Credential Manager, and outside Windows in the data folder's secrets file
//! (file_store.rs; spec 2026-11-03-paperclip-plugin, 5.2).

#[cfg(windows)]
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use keyring_core::Entry;
#[cfg(windows)]
use keyring_core::api::CredentialStoreApi as _;

use crate::Error;

const SERVICE: &str = "open_mess";
const LOCAL_API_USER: &str = "local-api";
const ADMIN_API_USER: &str = "admin-api";

/// The bearer token of the local API. Generated and stored at the first call.
pub fn local_api_token() -> Result<String, Error> {
    token(LOCAL_API_USER)
}

/// The stored bearer token, if the daemon has created one. Never creates it, so a CLI call can't
/// race the daemon's first start with a token of its own.
pub fn stored_local_api_token() -> Result<Option<String>, Error> {
    stored(LOCAL_API_USER)
}

/// The bearer token of the administrative API outside Windows (spec 2026-11-03-paperclip-plugin,
/// 5.3). Generated and stored at the first call, like the local API's.
pub fn admin_api_token() -> Result<String, Error> {
    token(ADMIN_API_USER)
}

/// The stored administrative token, if the daemon has created one; never creates it.
pub fn stored_admin_api_token() -> Result<Option<String>, Error> {
    stored(ADMIN_API_USER)
}

/// The stored token of `user`, else 32 random bytes in hex, stored.
fn token(user: &str) -> Result<String, Error> {
    if let Some(token) = stored(user)? {
        return Ok(token);
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| Error::Random(err.to_string()))?;
    let token = bytes.iter().fold(String::with_capacity(64), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    entry(user)?.set_password(&token)?;
    Ok(token)
}

fn stored(user: &str) -> Result<Option<String>, Error> {
    match entry(user)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Outside Windows, makes the secrets file of data folder `dir` keyring-core's default store, for
/// the daemon and the CLI alike, before either touches a secret. Windows keeps Credential Manager.
#[cfg(not(windows))]
pub fn install_platform_store(dir: &Path) -> Result<(), Error> {
    keyring_core::set_default_store(std::sync::Arc::new(crate::file_store::FileStore::open(dir)?));
    Ok(())
}

#[cfg(windows)]
pub fn install_platform_store(_dir: &Path) -> Result<(), Error> {
    Ok(())
}

fn entry(user: &str) -> Result<Entry, Error> {
    match keyring_core::get_default_store() {
        // The secrets file outside Windows, or the tests' mock store.
        Some(store) => Ok(store.build(SERVICE, user, None)?),
        None => credential_manager_entry(SERVICE, user),
    }
}

/// A Credential Manager entry with Local persistence: the default, Enterprise, roams with a
/// roaming Windows profile to the user's other machines, and secrets never leave this one.
#[cfg(windows)]
fn credential_manager_entry(service: &str, user: &str) -> Result<Entry, Error> {
    let local = HashMap::from([("persistence", "local")]);
    Ok(windows_native_keyring_store::Store::new()?.build(service, user, Some(&local))?)
}

#[cfg(not(windows))]
fn credential_manager_entry(_service: &str, _user: &str) -> Result<Entry, Error> {
    Err(keyring_core::Error::NotSupportedByStore("Credential Manager exists only on Windows".into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_generated_once() {
        keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        assert_eq!(stored_local_api_token().unwrap(), None);
        let first = local_api_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(local_api_token().unwrap(), first);
        assert_eq!(stored_local_api_token().unwrap(), Some(first.clone()));
        // The administrative token is a token of its own.
        assert_eq!(stored_admin_api_token().unwrap(), None);
        let admin = admin_api_token().unwrap();
        assert_ne!(admin, first);
        assert_eq!(admin_api_token().unwrap(), admin);
        assert_eq!(stored_admin_api_token().unwrap(), Some(admin));
    }

    #[cfg(windows)]
    #[test]
    fn credential_manager_entries_stay_on_this_machine() {
        let entry = credential_manager_entry(&format!("open_mess-test-{}", std::process::id()), "persistence").unwrap();
        entry.set_password("not a secret").unwrap();
        let persistence = entry.get_attributes().unwrap()["persistence"].clone();
        entry.delete_credential().unwrap();
        assert_eq!(persistence, "Local");
    }
}
