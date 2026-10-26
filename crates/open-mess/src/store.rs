//! The SQLite state store.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::Error;

/// The local API port when none is configured.
pub const DEFAULT_PORT: u16 = 7341;

/// Schema migrations in order: entry `i` takes `PRAGMA user_version` from `i` to `i + 1`.
/// Append only; never edit a migration that has shipped.
const MIGRATIONS: &[&str] = &["CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL);"];

/// Configuration and state in `state.sqlite3`.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (or creates) the database and brings its schema up to date.
    ///
    /// A database written by a newer open_mess is refused with [`Error::SchemaTooNew`], never
    /// rewritten.
    pub fn open(path: &Path) -> Result<Store, Error> {
        let mut conn = Connection::open(path)?;
        migrate(&mut conn)?;
        Ok(Store { conn })
    }

    pub fn get_config(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(self.conn.query_row("SELECT value FROM config WHERE key = ?1", [key], |row| row.get(0)).optional()?)
    }

    pub fn set_config(&self, key: &str, value: &str) -> Result<(), Error> {
        self.conn.execute("INSERT INTO config (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value", [key, value])?;
        Ok(())
    }

    /// The local API port: the stored `port`, or [`DEFAULT_PORT`].
    pub fn port(&self) -> Result<u16, Error> {
        let Some(value) = self.get_config("port")? else {
            return Ok(DEFAULT_PORT);
        };
        value
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| Error::BadConfig(format!("port must be a number from 1 to 65535, found {value:?}")))
    }
}

fn migrate(conn: &mut Connection) -> Result<(), Error> {
    // Immediate: two processes opening a fresh database don't both run the migrations.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let known = i64::try_from(MIGRATIONS.len()).map_err(|_| Error::BadConfig("too many migrations".into()))?;
    if found > known {
        return Err(Error::SchemaTooNew { found, known });
    }
    let done = usize::try_from(found).map_err(|_| Error::BadConfig(format!("state database has schema version {found}")))?;
    if found == known {
        return Ok(());
    }
    for sql in MIGRATIONS.iter().skip(done) {
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", known)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn port_defaults_to_7341_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.port().unwrap(), 7341);
        store.set_config("port", "7400").unwrap();
        drop(store);
        assert_eq!(Store::open(&path).unwrap().port().unwrap(), 7400);
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        rusqlite::Connection::open(&path).unwrap().pragma_update(None, "user_version", 999).unwrap();
        assert!(matches!(Store::open(&path), Err(Error::SchemaTooNew { found: 999, .. })));
    }

    #[test]
    fn corrupt_file_is_refused_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        let garbage = vec![0x5a_u8; 8192];
        std::fs::write(&path, &garbage).unwrap();
        assert!(matches!(Store::open(&path), Err(Error::Sqlite(_))));
        assert_eq!(std::fs::read(&path).unwrap(), garbage);
    }

    #[test]
    fn bad_port_value_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        store.set_config("port", "not-a-port").unwrap();
        assert!(store.port().is_err());
    }
}
