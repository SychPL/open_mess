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

/// The limits an engine applies; `Default` is the constants above.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub call_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { call_timeout: CALL_TIMEOUT }
    }
}

/// Short limits that keep tests fast.
#[cfg(any(test, feature = "test-support"))]
pub fn for_tests() -> Limits {
    Limits { call_timeout: Duration::from_secs(2) }
}

/// Caps a module instance's memories and tables in total, so a component with many memories gets
/// no more than one module's share. Growing past a cap traps, which unloads the module.
#[derive(Default)]
pub(crate) struct ModuleLimiter {
    memory: usize,
    table_elements: usize,
}

fn grow(total: usize, current: usize, desired: usize, max: usize, what: &str) -> wasmtime::Result<usize> {
    let total = total.saturating_sub(current).saturating_add(desired);
    if total > max { Err(wasmtime::format_err!("module {what} limit reached")) } else { Ok(total) }
}

impl wasmtime::ResourceLimiter for ModuleLimiter {
    fn memory_growing(&mut self, current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        self.memory = grow(self.memory, current, desired, MAX_MEMORY, "memory")?;
        Ok(true)
    }

    fn table_growing(&mut self, current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        self.table_elements = grow(self.table_elements, current, desired, MAX_TABLE_ELEMENTS, "table")?;
        Ok(true)
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
}
