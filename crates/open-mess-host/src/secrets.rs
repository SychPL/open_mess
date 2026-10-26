//! Module secrets in the platform credential store (Windows Credential Manager): service
//! `open_mess/module/<name>`, user `<account>/<key>`. The host fills in the module name, so a
//! module only ever reaches its own service (spec 8.3). The module's storage lists the names of its
//! secrets, which caps their number at `MAX_SECRETS`.

use std::sync::Arc;

use keyring_core::Entry;
use keyring_core::api::CredentialStore;

use crate::limits;
use crate::storage::{Storage, StorageError};

/// The platform credential store, set up on first use.
pub(crate) fn platform_store() -> Result<Arc<CredentialStore>, String> {
    keyring::Entry::store_status().as_ref().map_err(|e| format!("credential store unavailable: {e}"))?;
    keyring_core::get_default_store().ok_or_else(|| "credential store unavailable".to_string())
}

/// The credential for a module's account and key. Control characters are refused: Windows cuts a
/// credential name at a NUL, which would let `<account>/<key>` name another module's credential.
fn entry(store: &CredentialStore, module: &str, account: &str, key: &str) -> Result<Entry, String> {
    if account.chars().chain(key.chars()).any(char::is_control) {
        return Err("account and key may not contain control characters".to_string());
    }
    store.build(&format!("open_mess/module/{module}"), &format!("{account}/{key}"), None).map_err(|e| e.to_string())
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
    if !storage.add_secret(account, key).map_err(failed)? {
        return Err(format!("at most {} secrets per module", limits::MAX_SECRETS));
    }
    entry.set_password(value).map_err(|e| e.to_string())
}

pub(crate) fn delete(store: &CredentialStore, storage: &mut Storage, module: &str, account: &str, key: &str) {
    if let Ok(entry) = entry(store, module, account, key) {
        entry.delete_credential().ok();
        storage.remove_secret(account, key).ok();
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
        delete(&*store, &mut storage, "a", "acct", "refresh");
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
        delete(&*store, &mut storage, "m", "acct", "k1");
        set(&*store, &mut storage, "m", "acct", "one-more", "v").unwrap();
        drop(storage);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
