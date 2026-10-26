//! Secrets in Windows Credential Manager.

use std::fmt::Write as _;

use keyring_core::Entry;

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
    // Tests install the mock store first; everyone else gets Credential Manager.
    #[cfg(windows)]
    if keyring_core::get_default_store().is_none() {
        keyring_core::set_default_store(windows_native_keyring_store::Store::new()?);
    }
    Ok(Entry::new(SERVICE, LOCAL_API_USER)?)
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
}
