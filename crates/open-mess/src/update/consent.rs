//! The user's one-time install consent and the update settings (spec 2026-10-27-self-update, 6).
//!
//! Everything takes a plain `&Connection`: the helper process that installs an update checks and
//! uses the consent without `Store::open`, which would migrate a database a newer daemon may have
//! migrated further.

use rusqlite::{Connection, OptionalExtension, params};

use super::UpdateError;

/// How long a click stays valid after the helper was launched.
const VALID_MS: i64 = 600_000;

const AUTO_INSTALL: &str = "update_auto_install";

/// The user clicked "install" for this release; replaces any earlier consent.
pub fn record_click(conn: &Connection, version: &str, manifest_sha256: &str, now_ms: i64) -> Result<(), UpdateError> {
    conn.execute("DELETE FROM update_consent", [])?;
    conn.execute("INSERT INTO update_consent (version, manifest_sha256, clicked_ms) VALUES (?1, ?2, ?3)", params![version, manifest_sha256, now_ms])?;
    Ok(())
}

/// The helper was started for this click.
pub fn mark_launched(conn: &Connection, version: &str, manifest_sha256: &str, now_ms: i64) -> Result<(), UpdateError> {
    conn.execute(
        "UPDATE update_consent SET launched_ms = ?3 WHERE version = ?1 AND manifest_sha256 = ?2 AND used_ms IS NULL",
        params![version, manifest_sha256, now_ms],
    )?;
    Ok(())
}

/// Launched, not yet used, and launched at most ten minutes ago.
pub fn valid(conn: &Connection, version: &str, manifest_sha256: &str, now_ms: i64) -> Result<bool, UpdateError> {
    let launched: Option<Option<i64>> = conn
        .query_row(
            "SELECT launched_ms FROM update_consent WHERE version = ?1 AND manifest_sha256 = ?2 AND used_ms IS NULL",
            params![version, manifest_sha256],
            |row| row.get(0),
        )
        .optional()?;
    Ok(matches!(launched, Some(Some(at)) if now_ms.saturating_sub(at) <= VALID_MS))
}

/// The consent is spent: it never installs a second time.
pub fn mark_used(conn: &Connection, version: &str, manifest_sha256: &str, now_ms: i64) -> Result<(), UpdateError> {
    conn.execute("UPDATE update_consent SET used_ms = ?3 WHERE version = ?1 AND manifest_sha256 = ?2", params![version, manifest_sha256, now_ms])?;
    Ok(())
}

fn get(conn: &Connection, key: &str) -> Result<Option<String>, UpdateError> {
    Ok(conn.query_row("SELECT value FROM config WHERE key = ?1", [key], |row| row.get(0)).optional()?)
}

fn set(conn: &Connection, key: &str, value: &str) -> Result<(), UpdateError> {
    conn.execute("INSERT INTO config (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value", [key, value])?;
    Ok(())
}

/// Install without a click (off unless the user turned it on).
pub fn auto_install(conn: &Connection) -> Result<bool, UpdateError> {
    Ok(get(conn, AUTO_INSTALL)?.as_deref() == Some("1"))
}

pub fn set_auto_install(conn: &Connection, on: bool) -> Result<(), UpdateError> {
    set(conn, AUTO_INSTALL, if on { "1" } else { "0" })
}

fn rolled_back_key(version: &str) -> String {
    format!("update_rolled_back:{version}")
}

/// This release was installed and failed its health check; it is not offered again unchanged.
pub fn record_rolled_back(conn: &Connection, version: &str, manifest_sha256: &str) -> Result<(), UpdateError> {
    set(conn, &rolled_back_key(version), manifest_sha256)
}

pub fn rolled_back(conn: &Connection, version: &str, manifest_sha256: &str) -> Result<bool, UpdateError> {
    Ok(get(conn, &rolled_back_key(version))?.as_deref() == Some(manifest_sha256))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    const SHA: &str = "aa";

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        (dir, store)
    }

    #[test]
    fn consent_expires_ten_minutes_after_launch() {
        let (_dir, store) = store();
        let conn = store.conn();
        record_click(conn, "1.2.0", SHA, 1_000).unwrap();
        mark_launched(conn, "1.2.0", SHA, 5_000).unwrap();
        assert!(valid(conn, "1.2.0", SHA, 5_000 + 600_000).unwrap());
        assert!(!valid(conn, "1.2.0", SHA, 5_000 + 600_001).unwrap());
    }

    #[test]
    fn consent_without_launch_is_invalid() {
        let (_dir, store) = store();
        record_click(store.conn(), "1.2.0", SHA, 1_000).unwrap();
        assert!(!valid(store.conn(), "1.2.0", SHA, 1_001).unwrap());
    }

    #[test]
    fn consent_is_single_use() {
        let (_dir, store) = store();
        let conn = store.conn();
        record_click(conn, "1.2.0", SHA, 1_000).unwrap();
        mark_launched(conn, "1.2.0", SHA, 2_000).unwrap();
        assert!(valid(conn, "1.2.0", SHA, 3_000).unwrap());
        mark_used(conn, "1.2.0", SHA, 3_000).unwrap();
        assert!(!valid(conn, "1.2.0", SHA, 3_001).unwrap());
        // A used consent cannot be launched again either.
        mark_launched(conn, "1.2.0", SHA, 4_000).unwrap();
        assert!(!valid(conn, "1.2.0", SHA, 4_001).unwrap());
    }

    #[test]
    fn consent_for_other_manifest_is_invalid() {
        let (_dir, store) = store();
        let conn = store.conn();
        record_click(conn, "1.2.0", SHA, 1_000).unwrap();
        mark_launched(conn, "1.2.0", SHA, 2_000).unwrap();
        assert!(!valid(conn, "1.2.0", "bb", 3_000).unwrap());
        assert!(!valid(conn, "1.3.0", SHA, 3_000).unwrap());
        // A newer click replaces the old consent.
        record_click(conn, "1.3.0", "cc", 4_000).unwrap();
        assert!(!valid(conn, "1.2.0", SHA, 4_001).unwrap());
    }

    #[test]
    fn rolled_back_release_is_remembered() {
        let (_dir, store) = store();
        let conn = store.conn();
        assert!(!rolled_back(conn, "1.2.0", SHA).unwrap());
        record_rolled_back(conn, "1.2.0", SHA).unwrap();
        assert!(rolled_back(conn, "1.2.0", SHA).unwrap());
        assert!(!rolled_back(conn, "1.2.0", "bb").unwrap(), "a re-signed release is a new chance");
        assert!(!rolled_back(conn, "1.3.0", SHA).unwrap());
    }

    #[test]
    fn auto_install_is_off_by_default() {
        let (_dir, store) = store();
        assert!(!auto_install(store.conn()).unwrap());
        set_auto_install(store.conn(), true).unwrap();
        assert!(auto_install(store.conn()).unwrap());
        set_auto_install(store.conn(), false).unwrap();
        assert!(!auto_install(store.conn()).unwrap());
    }
}
