//! Resource limits for modules (spec 8.3 and the resource rule in section 15). The values are the
//! milestone B1 plan's decisions.

use std::time::Duration;

/// How often the epoch ticker lets running WebAssembly check its deadline.
pub const EPOCH_TICK: Duration = Duration::from_millis(10);
