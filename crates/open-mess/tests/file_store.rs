//! The file store of secrets outside Windows (spec 2026-11-03-paperclip-plugin, 5.2). It runs on
//! every platform; only the file modes are Unix.

use keyring_core::api::CredentialStoreApi;
use open_mess::file_store::{FILE, FileStore, MAX_FILE};

#[test]
fn secrets_are_set_read_and_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    let entry = store.build("open_mess", "local-api", None).unwrap();
    assert!(matches!(entry.get_password(), Err(keyring_core::Error::NoEntry)));
    entry.set_password("first").unwrap();
    entry.set_password("second").unwrap();
    assert_eq!(entry.get_password().unwrap(), "second");
    let other = store.build("open_mess/module/teams", "acct/refresh", None).unwrap();
    other.set_password("r").unwrap();
    entry.delete_credential().unwrap();
    assert!(matches!(entry.get_password(), Err(keyring_core::Error::NoEntry)));
    assert!(matches!(entry.delete_credential(), Err(keyring_core::Error::NoEntry)));
    assert_eq!(other.get_password().unwrap(), "r");
}

#[test]
fn another_store_on_the_file_sees_the_secrets() {
    let dir = tempfile::tempdir().unwrap();
    FileStore::open(dir.path()).unwrap().build("s", "u", None).unwrap().set_password("kept").unwrap();
    let again = FileStore::open(dir.path()).unwrap();
    assert_eq!(again.build("s", "u", None).unwrap().get_password().unwrap(), "kept");
}

#[test]
fn credential_manager_modifiers_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    let local = std::collections::HashMap::from([("persistence", "Local")]);
    let entry = store.build("s", "u", Some(&local)).unwrap();
    entry.set_password("v").unwrap();
    assert_eq!(store.build("s", "u", None).unwrap().get_password().unwrap(), "v");
}

#[test]
fn a_damaged_file_fails_and_stays() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(FILE), "{not json").unwrap();
    let err = FileStore::open(dir.path()).unwrap_err().to_string();
    assert!(err.contains("damaged"), "{err}");
    assert_eq!(std::fs::read_to_string(dir.path().join(FILE)).unwrap(), "{not json");
}

#[test]
fn a_write_over_the_limit_fails_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    let entry = store.build("s", "u", None).unwrap();
    entry.set_password("small").unwrap();
    let before = std::fs::read(dir.path().join(FILE)).unwrap();
    let huge = "x".repeat(usize::try_from(MAX_FILE).unwrap());
    assert!(entry.set_password(&huge).is_err());
    assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
    assert_eq!(entry.get_password().unwrap(), "small");
}

#[test]
fn writes_at_once_lose_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    let writers: Vec<_> = (0..8)
        .map(|i| {
            let store = store.clone();
            std::thread::spawn(move || store.build("s", &format!("u{i}"), None).unwrap().set_password("v").unwrap())
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    for i in 0..8 {
        assert_eq!(store.build("s", &format!("u{i}"), None).unwrap().get_password().unwrap(), "v");
    }
}

#[cfg(unix)]
#[test]
fn only_the_owner_reads_the_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    store.build("s", "u", None).unwrap().set_password("v").unwrap();
    let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir.path().join(FILE)), 0o600);
    assert_eq!(mode(dir.path()), 0o700);
}

/// The module host keeps a default store set before it: its own secrets (a Teams refresh token)
/// go to the file too, not to a platform store a server container lacks (spec 5.2).
#[test]
fn the_module_host_uses_the_installed_file_store() {
    let dir = tempfile::tempdir().unwrap();
    keyring_core::set_default_store(std::sync::Arc::new(FileStore::open(dir.path()).unwrap()));
    let store = open_mess_host::credential_store().unwrap();
    store.build("open_mess/module/teams", "acct/refresh", None).unwrap().set_password("r").unwrap();
    let again = FileStore::open(dir.path()).unwrap();
    assert_eq!(again.build("open_mess/module/teams", "acct/refresh", None).unwrap().get_password().unwrap(), "r");
}
