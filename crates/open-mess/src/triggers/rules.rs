//! Trigger rules (spec 8.7): per account, a condition, an action, quiet hours and limits. Only the
//! configuration window (milestone E) and tests create or change them, never a tool: a rule can
//! start a program, and an agent must not be able to make one.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::NaiveTime;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::permissions::{self, Write};
use crate::store::Store;

/// What [`put`] returns when agents woken by the rule may write without the user's approval
/// (spec 8.6); the window shows it.
pub const AUTONOMOUS_WARNING: &str = "Agents woken by this rule can send, edit and react without your approval, because the account or \
     a conversation it covers is set to write on its own. Set it to 'with confirmation' if they should ask first.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub account: String,
    pub condition: Condition,
    pub action: Action,
    /// A local time window without deliveries; it wraps midnight when it starts later than it ends.
    pub quiet_hours: Option<(NaiveTime, NaiveTime)>,
    /// Deliveries per hour; batches past it are dropped.
    pub max_per_hour: u32,
    /// A conversation quiet this long ends the batch.
    pub quiet_ms: u64,
    /// The latest a batch is delivered, from its first message, however busy the conversation.
    pub max_delay_ms: u64,
    #[serde(skip)]
    pub enabled: bool,
}

impl Rule {
    /// An enabled rule with the default limits: 6 deliveries an hour, a minute of quiet, at most
    /// five minutes late.
    pub fn new(id: &str, account: &str, condition: Condition, action: Action) -> Rule {
        Rule {
            id: id.into(),
            account: account.into(),
            condition,
            action,
            quiet_hours: None,
            max_per_hour: 6,
            quiet_ms: 60_000,
            max_delay_ms: 300_000,
            enabled: true,
        }
    }
}

/// Which messages of the account fire the rule; the user's own never do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Condition {
    /// A direct message to the user.
    Direct,
    /// A message that mentions the user.
    Mention,
    /// Any message in one conversation.
    Conversation { id: String },
    /// A message from one of these people (the platform's sender ids).
    From { senders: Vec<String> },
}

impl Condition {
    pub fn name(&self) -> &'static str {
        match self {
            Condition::Direct => "direct",
            Condition::Mention => "mention",
            Condition::Conversation { .. } => "conversation",
            Condition::From { .. } => "from",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// A notification into the most recently active Claude Code session with channels; with no
    /// such session, the fallback runs, if there is one.
    Channel { fallback_spawn: Option<Spawn> },
    /// A program started for the batch.
    Spawn(Spawn),
}

/// A program and its fixed arguments, started directly, never through a shell. Nothing from a
/// message ever becomes part of them (spec 8.7, R1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub working_dir: Option<PathBuf>,
}

/// Every rule, oldest first.
pub fn list(store: &Store) -> Result<Vec<Rule>, Error> {
    let mut statement = store.conn().prepare("SELECT id, account, rule, enabled FROM trigger_rules ORDER BY created_ms, id")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
    rows.map(|row| parse(row?)).collect()
}

pub fn get(store: &Store, id: &str) -> Result<Option<Rule>, Error> {
    let row = store
        .conn()
        .query_row("SELECT id, account, rule, enabled FROM trigger_rules WHERE id = ?1", [id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .optional()?;
    row.map(parse).transpose()
}

/// Saves a rule, new or changed, after checking it. Returns [`AUTONOMOUS_WARNING`] when agents it
/// wakes could write without approval; the rule is saved anyway.
pub fn put(store: &Store, rule: &Rule) -> Result<Option<&'static str>, Error> {
    validate(rule)?;
    let json = serde_json::to_string(rule).map_err(|err| Error::BadConfig(err.to_string()))?;
    let created_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX));
    store.conn().execute(
        "INSERT INTO trigger_rules (id, account, rule, enabled, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (id) DO UPDATE SET account = excluded.account, rule = excluded.rule, enabled = excluded.enabled",
        (&rule.id, &rule.account, &json, rule.enabled, created_ms),
    )?;
    let autonomous = match &rule.condition {
        Condition::Conversation { id } => permissions::effective(store, &rule.account, Some(id))?.write == Write::Autonomous,
        _ => permissions::autonomous_anywhere(store, &rule.account)?,
    };
    Ok(autonomous.then_some(AUTONOMOUS_WARNING))
}

/// Deletes a rule; false when there was none.
pub fn delete(store: &Store, id: &str) -> Result<bool, Error> {
    Ok(store.conn().execute("DELETE FROM trigger_rules WHERE id = ?1", [id])? > 0)
}

fn parse((id, account, json, enabled): (String, String, String, bool)) -> Result<Rule, Error> {
    let rule: Rule = serde_json::from_str(&json).map_err(|err| Error::BadConfig(format!("trigger rule {id:?}: {err}")))?;
    Ok(Rule { id, account, enabled, ..rule })
}

fn validate(rule: &Rule) -> Result<(), Error> {
    let bad = |why: &str| Err(Error::BadConfig(format!("trigger rule: {why}")));
    if rule.id.is_empty() || rule.id.chars().count() > 64 || rule.id.chars().any(char::is_control) {
        return bad("the id must be 1 to 64 characters without control characters");
    }
    if rule.account.is_empty() {
        return bad("the account is missing");
    }
    match &rule.condition {
        Condition::Conversation { id } if id.is_empty() => return bad("the conversation is missing"),
        Condition::From { senders } if senders.is_empty() || senders.iter().any(String::is_empty) => return bad("the senders are missing"),
        _ => {}
    }
    let spawn = match &rule.action {
        Action::Channel { fallback_spawn } => fallback_spawn.as_ref(),
        Action::Spawn(spawn) => Some(spawn),
    };
    if let Some(spawn) = spawn {
        if !spawn.program.is_absolute() || !spawn.program.is_file() {
            return bad("the program must be the full path of an existing file");
        }
        if spawn.working_dir.as_ref().is_some_and(|dir| !dir.is_absolute() || !dir.is_dir()) {
            return bad("the working folder must be the full path of an existing folder");
        }
    }
    if !(1..=60).contains(&rule.max_per_hour) {
        return bad("deliveries per hour must be 1 to 60");
    }
    if !(5_000..=600_000).contains(&rule.quiet_ms) {
        return bad("the quiet period must be 5 seconds to 10 minutes");
    }
    if !(rule.quiet_ms..=3_600_000).contains(&rule.max_delay_ms) {
        return bad("the longest delay must be from the quiet period to one hour");
    }
    if rule.quiet_hours.is_some_and(|(start, end)| start == end) {
        return bad("quiet hours must start and end at different times");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::tools::Registry;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        (dir, store)
    }

    /// An absolute path to a file that exists: this test binary.
    fn program() -> PathBuf {
        std::env::current_exe().unwrap()
    }

    fn spawn() -> Spawn {
        Spawn { program: program(), args: vec!["-p".into(), "check open_mess".into()], working_dir: None }
    }

    #[test]
    fn round_trip() {
        let (_dir, store) = store();
        let mut rule = Rule::new("r1", "teams:a", Condition::From { senders: vec!["p1".into(), "p2".into()] }, Action::Spawn(spawn()));
        rule.quiet_hours = Some((at(22, 0), at(7, 30)));
        put(&store, &rule).unwrap();
        let other = Rule::new("r2", "teams:b", Condition::Direct, Action::Channel { fallback_spawn: Some(spawn()) });
        put(&store, &other).unwrap();
        assert_eq!(get(&store, "r1").unwrap(), Some(rule.clone()));
        assert_eq!(list(&store).unwrap(), [rule.clone(), other.clone()]);

        // Saving again changes the rule in place.
        rule.condition = Condition::Conversation { id: "c1".into() };
        rule.enabled = false;
        rule.max_per_hour = 2;
        put(&store, &rule).unwrap();
        assert_eq!(list(&store).unwrap(), [rule, other.clone()]);

        assert!(delete(&store, "r1").unwrap());
        assert!(!delete(&store, "r1").unwrap());
        assert_eq!(list(&store).unwrap(), [other]);
        assert_eq!(get(&store, "r1").unwrap(), None);
    }

    #[test]
    fn relative_program_is_rejected() {
        let (_dir, store) = store();
        let with_program = |program: PathBuf| {
            let spawn = Spawn { program, args: Vec::new(), working_dir: None };
            Rule::new("r", "teams:a", Condition::Mention, Action::Spawn(spawn))
        };
        assert!(put(&store, &with_program("claude".into())).is_err());
        assert!(put(&store, &with_program(PathBuf::from("bin").join("claude.exe"))).is_err());
        // Absolute, but no such file; and a folder is not a program.
        assert!(put(&store, &with_program(program().with_file_name("no-such-program.exe"))).is_err());
        assert!(put(&store, &with_program(std::env::temp_dir())).is_err());
        // The fallback of a channel rule is checked the same way.
        let fallback = Spawn { program: "claude".into(), args: Vec::new(), working_dir: None };
        assert!(put(&store, &Rule::new("r", "teams:a", Condition::Mention, Action::Channel { fallback_spawn: Some(fallback) })).is_err());
        let relative_dir = Spawn { working_dir: Some("work".into()), ..spawn() };
        assert!(put(&store, &Rule::new("r", "teams:a", Condition::Mention, Action::Spawn(relative_dir))).is_err());
        assert_eq!(list(&store).unwrap(), []);
        put(&store, &with_program(program())).unwrap();
    }

    #[test]
    fn limits_are_validated() {
        let (_dir, store) = store();
        let rule = Rule::new("r", "teams:a", Condition::Direct, Action::Channel { fallback_spawn: None });
        assert_eq!((rule.max_per_hour, rule.quiet_ms, rule.max_delay_ms, rule.enabled), (6, 60_000, 300_000, true));
        let changed = |change: fn(&mut Rule)| {
            let mut rule = rule.clone();
            change(&mut rule);
            put(&store, &rule)
        };
        assert!(changed(|rule| rule.max_per_hour = 0).is_err());
        assert!(changed(|rule| rule.max_per_hour = 61).is_err());
        assert!(changed(|rule| rule.quiet_ms = 4_999).is_err());
        assert!(changed(|rule| rule.quiet_ms = 600_001).is_err());
        assert!(changed(|rule| rule.max_delay_ms = rule.quiet_ms - 1).is_err());
        assert!(changed(|rule| rule.max_delay_ms = 3_600_001).is_err());
        // An empty quiet window is a mistake, not "never quiet".
        assert!(changed(|rule| rule.quiet_hours = Some((at(12, 0), at(12, 0)))).is_err());
        assert!(changed(|rule| rule.id = String::new()).is_err());
        assert!(changed(|rule| rule.account = String::new()).is_err());
        assert!(changed(|rule| rule.condition = Condition::Conversation { id: String::new() }).is_err());
        assert!(changed(|rule| rule.condition = Condition::From { senders: Vec::new() }).is_err());
        assert_eq!(list(&store).unwrap(), []);

        // The bounds themselves are fine.
        changed(|rule| (rule.max_per_hour, rule.quiet_ms, rule.max_delay_ms) = (60, 5_000, 5_000)).unwrap();
        changed(|rule| (rule.max_per_hour, rule.quiet_ms, rule.max_delay_ms) = (1, 600_000, 3_600_000)).unwrap();
        changed(|rule| rule.quiet_hours = Some((at(12, 0), at(12, 1)))).unwrap();
    }

    fn at(hour: u32, minute: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, minute, 0).unwrap()
    }

    #[test]
    fn autonomous_writes_warn() {
        let (_dir, store) = store();
        let direct = Rule::new("r1", "teams:a", Condition::Direct, Action::Channel { fallback_spawn: None });
        let in_c1 = Rule::new("r2", "teams:a", Condition::Conversation { id: "c1".into() }, Action::Channel { fallback_spawn: None });
        let in_c2 = Rule::new("r3", "teams:a", Condition::Conversation { id: "c2".into() }, Action::Channel { fallback_spawn: None });
        // New accounts write with confirmation.
        assert_eq!(put(&store, &direct).unwrap(), None);
        // One autonomous conversation: rules that may cover it warn, rules for other conversations don't.
        permissions::set(&store, "teams:a", Some("c1"), true, Write::Autonomous).unwrap();
        assert_eq!(put(&store, &direct).unwrap(), Some(AUTONOMOUS_WARNING));
        assert_eq!(put(&store, &in_c1).unwrap(), Some(AUTONOMOUS_WARNING));
        assert_eq!(put(&store, &in_c2).unwrap(), None);
        // The whole account autonomous, c2 included.
        permissions::set(&store, "teams:a", None, true, Write::Autonomous).unwrap();
        assert_eq!(put(&store, &in_c2).unwrap(), Some(AUTONOMOUS_WARNING));
        // Saved either way: the warning is for the window to show.
        assert_eq!(list(&store).unwrap().len(), 3);
    }

    #[test]
    fn no_tool_creates_rules() {
        // A rule can start a program, so an agent must never be able to make one (spec 8.7): no
        // tool, over MCP or the CLI, touches rules.
        for tool in Registry::with_builtin(Instant::now()).list() {
            let text = format!("{} {}", tool.name, tool.description).to_lowercase();
            assert!(!text.contains("rule") && !text.contains("trigger"), "{}", tool.name);
        }
    }
}
