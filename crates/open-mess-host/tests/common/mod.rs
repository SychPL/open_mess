//! Helpers shared by the host's integration tests.

// clippy.toml allows unwrap in #[test] functions only; these helpers are test code too. Each test
// binary compiles this module and uses only some of it.
#![allow(clippy::unwrap_used, clippy::panic, dead_code)]

use std::path::PathBuf;

use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{Engine, ModuleContext, ModuleHandle, Net, limits};

/// A module built by `cargo xtask ci` (the wasm build runs before the tests), found from this test
/// binary so a custom `CARGO_TARGET_DIR` or `CARGO_BUILD_TARGET` works too: the binary is
/// `<target>[/<triple>]/<profile>/deps/<test>.exe`, the module `<target>/wasm32-wasip2/<profile>/<name>.wasm`.
pub fn wasm(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().and_then(|deps| deps.parent()).unwrap();
    let relative = PathBuf::from("wasm32-wasip2").join(profile_dir.file_name().unwrap()).join(format!("{name}.wasm"));
    let found = profile_dir.ancestors().map(|dir| dir.join(&relative)).find(|path| path.exists());
    found.unwrap_or_else(|| panic!("{} is missing; build the modules first (cargo xtask ci)", relative.display()))
}

/// A context whose network allows loopback, so modules can reach the tests' local servers.
pub fn context() -> ModuleContext {
    let (events, _) = tokio::sync::mpsc::channel(8);
    ModuleContext { data_dir: std::env::temp_dir(), events, net: Net::with_limits(AddressPolicy::AllowLoopbackForTests, limits::for_tests()) }
}

pub async fn load(engine: &Engine, name: &str) -> ModuleHandle {
    engine.load(&wasm(name), context()).await.unwrap()
}
