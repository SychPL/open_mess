//! Secrets in Windows Credential Manager.

#[cfg(windows)]
use std::collections::HashMap;
use std::fmt::Write as _;

use keyring_core::Entry;
#[cfg(windows)]
use keyring_core::api::CredentialStoreApi as _;

use crate::Error;

const SERVICE: &str = "open_mess";
const LOCAL_API_USER: &str = "local-api";

/// The bearer token of the local API. Generated and stored at the first call.
pub fn local_api_token() -> Result<String, Error> {
    if let Some(token) = stored_local_api_token()? {
        return Ok(token);
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| Error::Random(err.to_string()))?;
    let token = bytes.iter().fold(String::with_capacity(64), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    local_api_entry()?.set_password(&token)?;
    Ok(token)
}

/// The stored bearer token, if the daemon has created one. Never creates it, so a CLI call can't
/// race the daemon's first start with a token of its own.
pub fn stored_local_api_token() -> Result<Option<String>, Error> {
    match local_api_entry()?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn local_api_entry() -> Result<Entry, Error> {
    match keyring_core::get_default_store() {
        // Only tests install a default store (the mock one).
        Some(store) => Ok(store.build(SERVICE, LOCAL_API_USER, None)?),
        None => credential_manager_entry(SERVICE, LOCAL_API_USER),
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
        assert_eq!(stored_local_api_token().unwrap(), Some(first));
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
