//! Module timers: `host.set-timer` calls `events.on-timer` once, through the actor queue.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::actor::{GuestEvent, Task};
use crate::limits;

/// One module instance's pending timers; dropping it cancels them.
#[derive(Default)]
pub(crate) struct Timers {
    /// Timer id -> the generation it was set in, and its task.
    pending: HashMap<u32, (u64, Task)>,
    generation: u64,
}

impl Timers {
    /// Sets timer `id`, replacing it if it is pending. Returns false (and sets nothing) when
    /// `MAX_TIMERS` other timers are pending.
    pub(crate) fn set(&mut self, id: u32, after_ms: u32, events: &mpsc::Sender<GuestEvent>) -> bool {
        if !self.pending.contains_key(&id) && self.pending.len() >= limits::MAX_TIMERS {
            return false;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let delay = Duration::from_millis(u64::from(after_ms)).max(limits::MIN_TIMER_DELAY);
        let events = events.clone();
        let task = Task(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            events.send(GuestEvent::Timer { id, generation }).await.ok();
        }));
        // A replaced timer's task is aborted as it drops.
        self.pending.insert(id, (generation, task));
        true
    }

    /// Whether a timer that fired is still the one set (not replaced since); forgets it.
    pub(crate) fn fired(&mut self, id: u32, generation: u64) -> bool {
        let current = self.pending.get(&id).is_some_and(|(set_in, _)| *set_in == generation);
        if current {
            self.pending.remove(&id);
        }
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_timers_are_capped_and_resets_replace() {
        let (events, _queue) = mpsc::channel(1);
        let mut timers = Timers::default();
        for id in 0..limits::MAX_TIMERS as u32 {
            assert!(timers.set(id, 60_000, &events));
        }
        assert!(!timers.set(1000, 60_000, &events));
        // Setting a pending id again replaces it; the first setting's firing is stale.
        assert!(timers.set(0, 60_000, &events));
        assert!(!timers.fired(0, 1));
        assert!(timers.fired(0, timers.generation));
        assert!(timers.set(1000, 60_000, &events));
    }
}
