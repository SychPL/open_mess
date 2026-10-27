//! Triggers (spec 8.7): rules that wake an agent when someone writes to the user. The engine
//! (engine.rs) matches the event bus against the rules (rules.rs), batches and rate-limits, and
//! hands each batch to its rule's action.

pub mod engine;
pub mod rules;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use serde_json::{Value, json};

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
