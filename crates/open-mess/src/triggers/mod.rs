//! Triggers (spec 8.7): rules that wake an agent when someone writes to the user. The engine
//! (engine.rs) matches the event bus against the rules (rules.rs), batches and rate-limits, and
//! hands each batch to its rule's action ([`Actions`]): a notification into an open Claude Code
//! session (channel.rs), or a program started for it (spawn.rs).
//!
//! What an agent gets of a batch ([`payload`]) keeps metadata (account, conversation, kind, ids,
//! times, counts) apart from other people's content (sender names, text), which sits only under
//! `untrusted`, with the shared tools' notice (spec 8.5).

pub mod channel;
pub mod engine;
pub mod rules;
pub mod spawn;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use serde_json::{Value, json};

use crate::accounts::Accounts;
use crate::pending;
use crate::tools::{self, shared};
use channel::Sessions;
use engine::{Batch, Delivery, DeliveryResult};
use rules::{Action, Rule};
use spawn::Runs;

/// The most a batch's payload takes, in bytes of JSON; past it the oldest messages are left out.
pub const MAX_PAYLOAD: usize = 1024 * 1024;

/// The daemon's actions, behind the engine.
pub struct Actions {
    pub accounts: Arc<Accounts>,
    pub sessions: Sessions,
    pub runs: Arc<Runs>,
}

impl Delivery for Actions {
    async fn deliver(&self, rule: Rule, mut batch: Batch) -> DeliveryResult {
        fill_in_content(&self.accounts, &mut batch).await;
        match &rule.action {
            Action::Spawn(program) => spawn::deliver(&self.runs, &rule.id, program, &batch, self.still_allowed(&rule.id, &batch)),
            Action::Channel { .. } if channel::deliver(&self.sessions, &rule.id, &batch).await => DeliveryResult::Delivered,
            // No session: the rule's program instead, if it has one (spec 8.7).
            Action::Channel { fallback_spawn: Some(program) } => {
                spawn::deliver(&self.runs, &rule.id, program, &batch, self.still_allowed(&rule.id, &batch))
            }
            Action::Channel { fallback_spawn: None } => DeliveryResult::NoSession,
        }
    }
}

impl Actions {
    /// Asked before a batch that waited for its run starts: the rule, the account or the read
    /// permission may have gone meanwhile (spec 8.7, R7).
    fn still_allowed(&self, rule: &str, batch: &Batch) -> spawn::StillAllowed {
        let (accounts, rule, account, conversation) =
            (Arc::clone(&self.accounts), rule.to_owned(), batch.account.clone(), batch.conversation.clone());
        Box::new(move || engine::current_rule(&accounts, &rule, &account, &conversation).is_some())
    }
}

/// Fills in the content of the batch's messages (sender names and text) from the module's
/// `read_messages`, as the read tools return it: only what the agent may read, under `untrusted`.
/// Best effort: when the module can't answer, the batch keeps the ids, and the agent can read the
/// conversation itself.
async fn fill_in_content(accounts: &Accounts, batch: &mut Batch) {
    let Ok(Some(account)) = accounts.get(&batch.account) else {
        return;
    };
    // Newest first: the batch's messages, unless the conversation moved on since.
    let args = json!({"conversation": batch.conversation, "limit": batch.messages.len().clamp(1, 100)});
    let Ok(result) = tools::call_account(accounts, &account, "read_messages", &args).await else {
        return;
    };
    let items = result.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    for message in &mut batch.messages {
        let found = items.iter().find(|item| item.get("id").and_then(Value::as_str) == Some(message.id.as_str()));
        message.untrusted = found.and_then(|item| item.get("untrusted")).cloned();
    }
}

/// What an agent gets of a batch of `rule`, at most [`MAX_PAYLOAD`] bytes of JSON.
pub fn payload(rule: &str, batch: &Batch) -> Value {
    let time = |ms: u64| pending::time(i64::try_from(ms).unwrap_or(i64::MAX));
    let mut messages: Vec<Value> = batch
        .messages
        .iter()
        .map(|message| {
            let mut item = json!({"id": message.id, "sender": message.sender, "at": time(message.at_ms)});
            if let (Some(untrusted), Value::Object(item)) = (&message.untrusted, &mut item) {
                item.insert("untrusted".into(), untrusted.clone());
            }
            item
        })
        .collect();
    let without_messages = json!({
        "rule": rule,
        "account": batch.account,
        "conversation": batch.conversation,
        "kind": batch.kind,
        "count": batch.count,
        "first_at": time(batch.first_ms),
        "last_at": time(batch.last_ms),
        "messages": [],
        "untrusted_notice": shared::UNTRUSTED_NOTICE,
    });
    // The oldest messages go first until the rest fits.
    let mut size = without_messages.to_string().len() + messages.iter().map(|message| message.to_string().len() + 1).sum::<usize>();
    let mut left_out = 0;
    for message in &messages {
        if size <= MAX_PAYLOAD {
            break;
        }
        size = size.saturating_sub(message.to_string().len() + 1);
        left_out += 1;
    }
    messages.drain(..left_out);
    let mut payload = without_messages;
    if let Value::Object(fields) = &mut payload {
        fields.insert("messages".into(), messages.into());
        fields.remove("untrusted_notice");
    }
    shared::with_notice(payload)
}

/// What happened to the batches of one rule since the daemon started, for `status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RuleCounters {
    pub delivered: u64,
    /// Past the rule's deliveries per hour.
    pub dropped_rate: u64,
    pub dropped_quiet_hours: u64,
    /// Pushed out of a full queue.
    pub dropped_queue: u64,
    /// No session to notify and no program to start.
    pub dropped_no_session: u64,
    /// The program could not be started (moved or deleted, say).
    pub failed_to_start: u64,
}

/// The engine's counters, shared with `status`.
#[derive(Clone, Default)]
pub struct Counters(Arc<Mutex<CounterState>>);

#[derive(Default)]
struct CounterState {
    /// Events the engine missed because it fell behind the bus.
    lagged: u64,
    rules: BTreeMap<String, RuleCounters>,
}

impl Counters {
    pub fn rule(&self, id: &str) -> RuleCounters {
        self.lock().rules.get(id).copied().unwrap_or_default()
    }

    pub fn lagged(&self) -> u64 {
        self.lock().lagged
    }

    pub fn to_json(&self) -> Value {
        let state = self.lock();
        json!({"lagged": state.lagged, "rules": state.rules})
    }

    fn add_lagged(&self, count: u64) {
        let mut state = self.lock();
        state.lagged = state.lagged.saturating_add(count);
    }

    fn count(&self, rule: &str, counter: impl FnOnce(&mut RuleCounters) -> &mut u64) {
        let mut state = self.lock();
        let value = counter(state.rules.entry(rule.to_owned()).or_default());
        *value = value.saturating_add(1);
    }

    fn lock(&self) -> MutexGuard<'_, CounterState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::BatchedMessage;

    #[test]
    fn payload_is_capped_and_keeps_the_newest() {
        let text = "x".repeat(28_000);
        let messages: Vec<BatchedMessage> = (0..100)
            .map(|n| BatchedMessage {
                id: format!("m{n}"),
                sender: "p1".into(),
                at_ms: 0,
                untrusted: Some(json!({"sender_name": "Pat", "text": text})),
            })
            .collect();
        let batch = Batch { account: "a".into(), conversation: "c".into(), kind: "direct", messages, count: 100, first_ms: 0, last_ms: 0 };
        let payload = payload("r", &batch);
        assert!(payload.to_string().len() <= MAX_PAYLOAD);
        let kept = payload["messages"].as_array().unwrap();
        assert!(kept.len() > 30 && kept.len() < 100, "{}", kept.len());
        assert_eq!(kept.last().unwrap()["id"], "m99");
        assert_eq!((payload["count"].as_u64(), payload["untrusted_notice"].as_str()), (Some(100), Some(shared::UNTRUSTED_NOTICE)));
        // Without content there is nothing to warn about.
        let bare = Batch { messages: Vec::new(), ..batch };
        assert!(super::payload("r", &bare).get("untrusted_notice").is_none());
    }
}
