//! Agent permissions (spec 8.6): per account, optionally overridden per conversation, the same for
//! every caller (MCP, CLI, agents started by triggers). The core enforces them; no tool changes
//! them: only the configuration window and tests call [`set`] and [`clear_override`].

use rusqlite::OptionalExtension;

use crate::Error;
use crate::store::Store;

/// What an agent may write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    Never,
    /// Every write waits for the user's approval, outside the agent's reach.
    Confirm,
    Autonomous,
}

impl Write {
    pub fn name(self) -> &'static str {
        match self {
            Write::Never => "never",
            Write::Confirm => "confirm",
            Write::Autonomous => "autonomous",
        }
    }

    pub fn parse(name: &str) -> Result<Write, Error> {
        [Write::Never, Write::Confirm, Write::Autonomous]
            .into_iter()
            .find(|write| write.name() == name)
            .ok_or_else(|| Error::BadConfig(format!("unknown write permission {name:?}")))
    }
}

/// The permissions that apply to one account or conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effective {
    pub read: bool,
    pub write: Write,
}

/// What an account has until the user changes it: reading, and writing with confirmation.
pub const DEFAULT: Effective = Effective { read: true, write: Write::Confirm };

/// The permissions for a conversation of `account` (or the account itself): the conversation's row,
/// else the account's, else [`DEFAULT`].
pub fn effective(store: &Store, account: &str, conversation: Option<&str>) -> Result<Effective, Error> {
    // The account's row has the empty conversation, which sorts last here.
    let row = store
        .conn()
        .query_row(
            "SELECT read, write FROM permissions WHERE account = ?1 AND conversation IN ('', ?2) ORDER BY conversation DESC LIMIT 1",
            (account, conversation.unwrap_or_default()),
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    match row {
        Some((read, write)) => Ok(Effective { read, write: Write::parse(&write)? }),
        None => Ok(DEFAULT),
    }
}

/// Whether an agent may read anything of `account`: the account, or one of its conversations.
pub fn readable_anywhere(store: &Store, account: &str) -> Result<bool, Error> {
    if effective(store, account, None)?.read {
        return Ok(true);
    }
    let any = "SELECT EXISTS(SELECT 1 FROM permissions WHERE account = ?1 AND conversation <> '' AND read = 1)";
    Ok(store.conn().query_row(any, [account], |row| row.get(0))?)
}

/// Sets the permissions of an account (`conversation` None) or of one of its conversations.
pub fn set(store: &Store, account: &str, conversation: Option<&str>, read: bool, write: Write) -> Result<(), Error> {
    store.conn().execute(
        "INSERT INTO permissions (account, conversation, read, write) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (account, conversation) DO UPDATE SET read = excluded.read, write = excluded.write",
        (account, conversation.unwrap_or_default(), read, write.name()),
    )?;
    Ok(())
}

/// Removes a conversation's own permissions; the account's apply to it again.
pub fn clear_override(store: &Store, account: &str, conversation: &str) -> Result<(), Error> {
    store.conn().execute("DELETE FROM permissions WHERE account = ?1 AND conversation = ?2 AND conversation <> ''", (account, conversation))?;
    Ok(())
}

/// The conversations of `account` with their own permissions, by conversation id.
pub fn overrides(store: &Store, account: &str) -> Result<Vec<(String, Effective)>, Error> {
    let mut statement =
        store.conn().prepare("SELECT conversation, read, write FROM permissions WHERE account = ?1 AND conversation <> '' ORDER BY conversation")?;
    let rows = statement.query_map([account], |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?, row.get::<_, String>(2)?)))?;
    let mut overrides = Vec::new();
    for row in rows {
        let (conversation, read, write) = row?;
        overrides.push((conversation, Effective { read, write: Write::parse(&write)? }));
    }
    Ok(overrides)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        (dir, store)
    }

    #[test]
    fn defaults_are_read_and_confirm() {
        let (_dir, store) = store();
        assert_eq!(DEFAULT, Effective { read: true, write: Write::Confirm });
        assert_eq!(effective(&store, "teams:a", None).unwrap(), DEFAULT);
        assert_eq!(effective(&store, "teams:a", Some("c1")).unwrap(), DEFAULT);
        assert!(readable_anywhere(&store, "teams:a").unwrap());
    }

    #[test]
    fn conversation_overrides_account() {
        let (_dir, store) = store();
        set(&store, "teams:a", None, false, Write::Never).unwrap();
        assert!(!readable_anywhere(&store, "teams:a").unwrap());
        set(&store, "teams:a", Some("c1"), true, Write::Autonomous).unwrap();
        assert_eq!(effective(&store, "teams:a", None).unwrap(), Effective { read: false, write: Write::Never });
        assert_eq!(effective(&store, "teams:a", Some("c2")).unwrap(), Effective { read: false, write: Write::Never });
        assert_eq!(effective(&store, "teams:a", Some("c1")).unwrap(), Effective { read: true, write: Write::Autonomous });
        assert!(readable_anywhere(&store, "teams:a").unwrap());
        // Another account keeps the defaults, even for a conversation with the same id.
        assert_eq!(effective(&store, "teams:b", Some("c1")).unwrap(), DEFAULT);
        // Setting again replaces the row.
        set(&store, "teams:a", None, true, Write::Confirm).unwrap();
        assert_eq!(effective(&store, "teams:a", Some("c2")).unwrap(), DEFAULT);
        assert_eq!(effective(&store, "teams:a", Some("c1")).unwrap().write, Write::Autonomous);
    }
}
