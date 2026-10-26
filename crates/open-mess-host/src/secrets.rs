//! Module secrets in the platform credential store (Windows Credential Manager): service
//! `open_mess/module/<name>`, user `<account>/<key>`. The host fills in the module name, so a
//! module only ever reaches its own service (spec 8.3). The module's storage lists the names of its
//! secrets, which caps their number at `MAX_SECRETS`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use keyring_core::Entry;
use keyring_core::api::CredentialStore;

use crate::limits;
use crate::storage::{Storage, StorageError};

/// The platform credential store, set up on first use. It becomes keyring-core's default store.
pub fn platform_store() -> Result<Arc<CredentialStore>, String> {
    keyring::Entry::store_status().as_ref().map_err(|e| format!("credential store unavailable: {e}"))?;
    keyring_core::get_default_store().ok_or_else(|| "credential store unavailable".to_string())
}

/// The credential for a module's account and key. Control characters are refused: Windows cuts a
/// credential name at a NUL, which would let `<account>/<key>` name another module's credential.
/// In Credential Manager the credential is Local: the default, Enterprise, roams with a roaming
/// Windows profile to the user's other machines, and secrets never leave this one (spec 8.4).
fn entry(store: &CredentialStore, module: &str, account: &str, key: &str) -> Result<Entry, String> {
    if account.chars().chain(key.chars()).any(char::is_control) {
        return Err("account and key may not contain control characters".to_string());
    }
    let local = HashMap::from([("persistence", "Local")]);
    let modifiers = store.vendor().starts_with("Windows Credential Manager").then_some(&local);
    store.build(&format!("open_mess/module/{module}"), &format!("{account}/{key}"), modifiers).map_err(|e| e.to_string())
}

fn failed(e: StorageError) -> String {
    match e {
        StorageError::QuotaExceeded => "storage quota exceeded".to_string(),
        StorageError::Failed(why) => why,
    }
}

pub(crate) fn get(store: &CredentialStore, module: &str, account: &str, key: &str) -> Option<String> {
    entry(store, module, account, key).ok()?.get_password().ok()
}

pub(crate) fn set(store: &CredentialStore, storage: &mut Storage, module: &str, account: &str, key: &str, value: &str) -> Result<(), String> {
    let entry = entry(store, module, account, key)?;
    let Some(new) = storage.add_secret(account, key).map_err(failed)? else {
        return Err(format!("at most {} secrets per module", limits::MAX_SECRETS));
    };
    entry.set_password(value).map_err(|e| {
        // A name for a secret that was never written does not take a place.
        if new {
            storage.remove_secret(account, key).ok();
        }
        e.to_string()
    })
}

/// Deletes the credential even when the module's storage is unavailable; the name is forgotten
/// when it is.
pub(crate) fn delete(store: &CredentialStore, storage: Option<&mut Storage>, module: &str, account: &str, key: &str) {
    let Ok(entry) = entry(store, module, account, key) else { return };
    // A credential that could not be deleted keeps its name, so it can still be found.
    let gone = matches!(entry.delete_credential(), Ok(()) | Err(keyring_core::Error::NoEntry));
    if gone && let Some(storage) = storage {
        storage.remove_secret(account, key).ok();
    }
}

/// Deletes every secret a module stored for one of its accounts, when the account is removed
/// (spec 8.8). The module's storage lists them. Fails when any is left.
pub fn delete_account_secrets(store: &CredentialStore, data_dir: &Path, module: &str, account: &str) -> Result<(), String> {
    let mut storage = Storage::open(data_dir, module).map_err(failed)?;
    for key in storage.secret_keys(account).map_err(failed)? {
        delete(store, Some(&mut storage), module, account, &key);
    }
    match storage.secret_keys(account).map_err(failed)?.len() {
        0 => Ok(()),
        left => Err(format!("{left} secrets could not be deleted")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_storage(name: &str) -> (std::path::PathBuf, Storage) {
        let dir = std::env::temp_dir().join(format!("open-mess-secrets-{name}-{}", std::process::id()));
        let storage = Storage::open(&dir, name).unwrap();
        (dir, storage)
    }

    #[test]
    fn secrets_are_namespaced_by_module() {
        let store: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        let (dir, mut storage) = temp_storage("namespaced");
        set(&*store, &mut storage, "b", "acct", "refresh", "b's token").unwrap();
        assert_eq!(get(&*store, "a", "acct", "refresh"), None);
        assert_eq!(get(&*store, "b", "acct", "refresh").as_deref(), Some("b's token"));
        set(&*store, &mut storage, "a", "acct", "refresh", "a's token").unwrap();
        delete(&*store, None, "a", "acct", "refresh");
        assert_eq!(get(&*store, "a", "acct", "refresh"), None);
        assert_eq!(get(&*store, "b", "acct", "refresh").as_deref(), Some("b's token"));
        // A NUL would end the Windows credential name early: refused.
        assert!(set(&*store, &mut storage, "b", "acct", "refresh.open_mess/module/a\0", "x").is_err());
        assert_eq!(get(&*store, "b", "acct\0", "refresh"), None);
        drop(storage);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn secrets_per_module_are_capped() {
        let store: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        let (dir, mut storage) = temp_storage("capped");
        for n in 0..limits::MAX_SECRETS {
            set(&*store, &mut storage, "m", "acct", &format!("k{n}"), "v").unwrap();
        }
        assert!(set(&*store, &mut storage, "m", "acct", "one-more", "v").is_err());
        assert_eq!(get(&*store, "m", "acct", "one-more"), None);
        // Overwriting a known secret is fine; deleting one makes room.
        set(&*store, &mut storage, "m", "acct", "k0", "new").unwrap();
        delete(&*store, Some(&mut storage), "m", "acct", "k1");
        set(&*store, &mut storage, "m", "acct", "one-more", "v").unwrap();
        drop(storage);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn module_secrets_stay_on_this_machine() {
        let store = platform_store().unwrap();
        let module = format!("open-mess-test-{}", std::process::id());
        let entry = entry(&*store, &module, "acct", "k").unwrap();
        entry.set_password("not a secret").unwrap();
        let persistence = entry.get_attributes().unwrap().get("persistence").cloned();
        entry.delete_credential().unwrap();
        assert_eq!(persistence.as_deref(), Some("Local"));
    }

    #[test]
    fn account_secrets_go_with_the_account() {
        let store: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        let (dir, mut storage) = temp_storage("account");
        for (account, key) in [("a", "refresh"), ("a", "device"), ("b", "refresh")] {
            set(&*store, &mut storage, "account", account, key, "v").unwrap();
        }
        delete_account_secrets(&*store, &dir, "account", "a").unwrap();
        assert_eq!(get(&*store, "account", "a", "refresh"), None);
        assert_eq!(get(&*store, "account", "a", "device"), None);
        assert_eq!(get(&*store, "account", "b", "refresh").as_deref(), Some("v"));
        assert_eq!(storage.secret_keys("a").unwrap(), Vec::<String>::new());
        // A credential the store refuses to delete is reported, and its name kept.
        let entry = store.build("open_mess/module/account", "b/refresh", None).unwrap();
        let cred: &keyring_core::mock::Cred = entry.as_any().downcast_ref().unwrap();
        cred.set_error(keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("locked"))));
        assert!(delete_account_secrets(&*store, &dir, "account", "b").is_err());
        assert_eq!(storage.secret_keys("b").unwrap(), ["refresh"]);
        drop(storage);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_write_takes_no_place() {
        let store: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        let (dir, mut storage) = temp_storage("failed");
        // The mock hands out the same credential for the same names; make its next call fail, as
        // Windows does for a value over the credential size limit.
        let entry = store.build("open_mess/module/m", "acct/k", None).unwrap();
        let cred: &keyring_core::mock::Cred = entry.as_any().downcast_ref().unwrap();
        cred.set_error(keyring_core::Error::TooLong("password".to_string(), 2560));
        assert!(set(&*store, &mut storage, "m", "acct", "k", "too long").is_err());
        // Not recorded: recording it now finds it new.
        assert_eq!(storage.add_secret("acct", "k").unwrap(), Some(true));
        // A delete the store refuses keeps the name; one that works forgets it.
        set(&*store, &mut storage, "m", "acct", "k", "v").unwrap();
        cred.set_error(keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("locked"))));
        delete(&*store, Some(&mut storage), "m", "acct", "k");
        assert_eq!(storage.add_secret("acct", "k").unwrap(), Some(false));
        delete(&*store, Some(&mut storage), "m", "acct", "k");
        assert_eq!(storage.add_secret("acct", "k").unwrap(), Some(true));
        drop(storage);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
