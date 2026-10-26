//! Module secrets in the platform credential store (Windows Credential Manager): service
//! `open_mess/module/<name>`, user `<account>/<key>`. The host fills in the module name, so a
//! module only ever reaches its own service (spec 8.3).

use std::sync::Arc;

use keyring_core::Entry;
use keyring_core::api::CredentialStore;

/// The platform credential store, set up on first use.
pub(crate) fn platform_store() -> Result<Arc<CredentialStore>, String> {
    keyring::Entry::store_status().as_ref().map_err(|e| format!("credential store unavailable: {e}"))?;
    keyring_core::get_default_store().ok_or_else(|| "credential store unavailable".to_string())
}

fn entry(store: &CredentialStore, module: &str, account: &str, key: &str) -> Result<Entry, String> {
    store.build(&format!("open_mess/module/{module}"), &format!("{account}/{key}"), None).map_err(|e| e.to_string())
}

pub(crate) fn get(store: &CredentialStore, module: &str, account: &str, key: &str) -> Option<String> {
    entry(store, module, account, key).ok()?.get_password().ok()
}

pub(crate) fn set(store: &CredentialStore, module: &str, account: &str, key: &str, value: &str) -> Result<(), String> {
    entry(store, module, account, key)?.set_password(value).map_err(|e| e.to_string())
}

pub(crate) fn delete(store: &CredentialStore, module: &str, account: &str, key: &str) {
    if let Ok(entry) = entry(store, module, account, key) {
        entry.delete_credential().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_namespaced_by_module() {
        let store: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        set(&*store, "b", "acct", "refresh", "b's token").unwrap();
        assert_eq!(get(&*store, "a", "acct", "refresh"), None);
        assert_eq!(get(&*store, "b", "acct", "refresh").as_deref(), Some("b's token"));
        set(&*store, "a", "acct", "refresh", "a's token").unwrap();
        delete(&*store, "a", "acct", "refresh");
        assert_eq!(get(&*store, "a", "acct", "refresh"), None);
        assert_eq!(get(&*store, "b", "acct", "refresh").as_deref(), Some("b's token"));
    }
}
