//! Resource limits for modules (spec 8.3 and the resource rule in section 15). The values are the
//! milestone B1 plan's decisions.

use std::time::Duration;

/// Wall-clock limit of one call into a module, time spent in host imports included.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Linear memory of one module instance, all its memories together.
pub const MAX_MEMORY: usize = 256 * 1024 * 1024;
/// Table elements of one module instance, all its tables together.
pub const MAX_TABLE_ELEMENTS: usize = 1_000_000;
/// Consecutive crashes (traps or timeouts) after which the module is not reloaded again.
pub const MAX_CRASHES: u32 = 3;
/// How often the epoch ticker lets running WebAssembly check its deadline.
pub const EPOCH_TICK: Duration = Duration::from_millis(10);
/// A fetch ends as `timeout` this long before its call's limit, so the module can still answer
/// instead of being unloaded (spec 8.3: the call limit counts time in imports).
pub const FETCH_MARGIN: Duration = Duration::from_millis(500);
/// HTTP response body; a longer one is cut off with `too-large`.
pub const MAX_BODY: usize = 16 * 1024 * 1024;
/// An attachment: what `http.fetch` takes in a `get-attachment` call instead of `MAX_BODY`, and the
/// most the core writes to disk (spec 8.5, R4 and R5).
pub const MAX_ATTACHMENT: usize = 64 * 1024 * 1024;
/// A 429 whose `Retry-After` (seconds) is above this ends the fetch with `throttled` instead of a
/// response.
pub const MAX_RETRY_AFTER: u32 = 300;
/// Establishing a TCP (and TLS) connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Waiting for the next bytes of a response.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// One WebSocket message, either way; a larger incoming one closes the socket.
pub const MAX_WS_MESSAGE: usize = 4 * 1024 * 1024;
/// Open WebSockets per module instance.
pub const MAX_SOCKETS: usize = 4;
/// Pending timers per module instance.
pub const MAX_TIMERS: usize = 32;
/// The shortest timer delay; shorter ones are lengthened to it.
pub const MIN_TIMER_DELAY: Duration = Duration::from_millis(100);
/// A module's storage: the bytes of all keys and values together.
pub const STORAGE_QUOTA: usize = 16 * 1024 * 1024;
/// Secrets (credentials) one module keeps, all accounts together.
pub const MAX_SECRETS: usize = 64;
/// One string of a host event (an account, conversation or message id); a larger event is dropped,
/// because it would sit in the daemon's queue outside the module's memory limit.
pub const MAX_EVENT_TEXT: usize = 64 * 1024;
/// One `host.log` line; the rest is cut off.
pub const MAX_LOG_MESSAGE: usize = 2048;

/// The time limits an engine or a `Net` applies; `Default` is the constants above.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub call_timeout: Duration,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { call_timeout: CALL_TIMEOUT, connect_timeout: CONNECT_TIMEOUT, read_timeout: READ_TIMEOUT }
    }
}

/// Short limits that keep tests fast.
#[cfg(any(test, feature = "test-support"))]
pub fn for_tests() -> Limits {
    Limits { call_timeout: Duration::from_secs(2), connect_timeout: Duration::from_secs(1), read_timeout: Duration::from_secs(1) }
}

/// Caps a module instance's memories and tables in total, so a component with many memories gets
/// no more than one module's share. Growing past a cap traps, which unloads the module.
#[derive(Default)]
pub(crate) struct ModuleLimiter {
    memory: usize,
    table_elements: usize,
}

/// Counts a growth into `total`. Growth past the memory's or table's own maximum is refused
/// without counting: wasmtime asks the limiter before it checks that maximum, the guest just sees
/// the grow fail, and counting it would make a later, allowed growth trap.
fn grow(total: &mut usize, current: usize, desired: usize, own_max: Option<usize>, cap: usize, what: &str) -> wasmtime::Result<bool> {
    if own_max.is_some_and(|max| desired > max) {
        return Ok(false);
    }
    let grown = total.saturating_sub(current).saturating_add(desired);
    if grown > cap {
        return Err(wasmtime::format_err!("module {what} limit reached"));
    }
    *total = grown;
    Ok(true)
}

impl wasmtime::ResourceLimiter for ModuleLimiter {
    fn memory_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> wasmtime::Result<bool> {
        grow(&mut self.memory, current, desired, maximum, MAX_MEMORY, "memory")
    }

    fn table_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> wasmtime::Result<bool> {
        grow(&mut self.table_elements, current, desired, maximum, MAX_TABLE_ELEMENTS, "table")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::ResourceLimiter;

    #[test]
    fn memory_cap_counts_all_memories_together() {
        let mut limiter = ModuleLimiter::default();
        assert!(limiter.memory_growing(0, MAX_MEMORY / 2, None).is_ok());
        assert!(limiter.memory_growing(0, MAX_MEMORY / 2, None).is_ok());
        assert!(limiter.memory_growing(0, 1, None).is_err());
        // Growing one memory counts only the difference.
        assert!(limiter.memory_growing(MAX_MEMORY / 2, MAX_MEMORY / 2, None).is_ok());
    }

    #[test]
    fn growth_past_the_memorys_own_maximum_is_not_counted() {
        let mut limiter = ModuleLimiter::default();
        // Refused (the guest sees -1), not a trap, and not counted.
        assert!(!limiter.memory_growing(0, MAX_MEMORY, Some(1024)).unwrap());
        assert!(limiter.memory_growing(0, MAX_MEMORY, None).unwrap());
    }
}
