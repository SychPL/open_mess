//! Per-module key-value storage with a size quota (spec 8.3): `<data_dir>/modules/<name>.sqlite3`,
//! table `kv(key, value)`. The quota counts the bytes of all keys and values.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::limits;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StorageError {
    QuotaExceeded,
    Failed(String),
}

impl From<rusqlite::Error> for StorageError {
    fn from(e: rusqlite::Error) -> Self {
        StorageError::Failed(e.to_string())
    }
}

/// ponytail: SQLite calls block the executor thread for their (short) duration; a blocking pool
/// if module storage ever becomes busy.
pub(crate) struct Storage {
    db: Connection,
}

impl Storage {
    pub(crate) fn open(data_dir: &Path, module: &str) -> Result<Storage, StorageError> {
        let dir = data_dir.join("modules");
        std::fs::create_dir_all(&dir).map_err(|e| StorageError::Failed(e.to_string()))?;
        let db = Connection::open(dir.join(format!("{module}.sqlite3")))?;
        // `secrets` lists the module's credentials (not their values), so their number is capped
        // and they can be found again; it is not counted in the quota.
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS secrets (account TEXT NOT NULL, key TEXT NOT NULL, PRIMARY KEY (account, key));",
        )?;
        Ok(Storage { db })
    }

    /// Records a secret's name: `Some(true)` when it is new, `Some(false)` when it was recorded
    /// already, `None` when it is new and `MAX_SECRETS` are recorded.
    pub(crate) fn add_secret(&mut self, account: &str, key: &str) -> Result<Option<bool>, StorageError> {
        let tx = self.db.transaction()?;
        let known: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM secrets WHERE account = ?1 AND key = ?2)", [account, key], |row| row.get(0))?;
        if known {
            return Ok(Some(false));
        }
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM secrets", [], |row| row.get(0))?;
        if usize::try_from(count).unwrap_or(usize::MAX) >= limits::MAX_SECRETS {
            return Ok(None);
        }
        tx.execute("INSERT INTO secrets (account, key) VALUES (?1, ?2)", [account, key])?;
        tx.commit()?;
        Ok(Some(true))
    }

    pub(crate) fn remove_secret(&self, account: &str, key: &str) -> Result<(), StorageError> {
        self.db.execute("DELETE FROM secrets WHERE account = ?1 AND key = ?2", [account, key])?;
        Ok(())
    }

    pub(crate) fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.db.query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| row.get(0)).optional()?)
    }

    /// Stores `value` under `key`, unless the storage would then exceed the quota; the old value
    /// stays in that case.
    pub(crate) fn set(&mut self, key: &str, value: &[u8]) -> Result<(), StorageError> {
        let tx = self.db.transaction()?;
        let others: i64 =
            tx.query_row("SELECT COALESCE(SUM(LENGTH(CAST(key AS BLOB)) + LENGTH(value)), 0) FROM kv WHERE key <> ?1", [key], |row| row.get(0))?;
        let total = usize::try_from(others).unwrap_or(usize::MAX).saturating_add(key.len()).saturating_add(value.len());
        if total > limits::STORAGE_QUOTA {
            return Err(StorageError::QuotaExceeded);
        }
        tx.execute("INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", (key, value))?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.db.execute("DELETE FROM kv WHERE key = ?1", [key])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_counts_other_keys_and_replaced_values_once() {
        let dir = std::env::temp_dir().join(format!("open-mess-storage-{}", std::process::id()));
        let mut storage = Storage::open(&dir, "quota").unwrap();
        let half = limits::STORAGE_QUOTA / 2;
        storage.set("a", &vec![0; half - 1]).unwrap();
        // Replacing a value counts only the new one.
        storage.set("a", &vec![0; half - 1]).unwrap();
        storage.set("b", &vec![0; half - 1]).unwrap();
        assert_eq!(storage.set("c", &[0; 3]), Err(StorageError::QuotaExceeded));
        storage.delete("b").unwrap();
        storage.set("c", &[0; 3]).unwrap();
        assert_eq!(storage.get("c").unwrap(), Some(vec![0; 3]));
        assert_eq!(storage.get("b").unwrap(), None);
        drop(storage);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
