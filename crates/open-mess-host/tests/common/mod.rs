//! Helpers shared by the host's integration tests.

// clippy.toml allows unwrap in #[test] functions only; these helpers are test code too. Each test
// binary compiles this module and uses only some of it.
#![allow(clippy::unwrap_used, dead_code)]

use std::path::PathBuf;

use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{Engine, ModuleContext, ModuleHandle, Net, limits};

/// A module built by `cargo xtask ci` (the wasm build runs before the tests), found next to this
/// test binary so a custom `CARGO_TARGET_DIR` works too: `<target>/<profile>/deps/<test>.exe`
/// -> `<target>/wasm32-wasip2/<profile>/<name>.wasm`.
pub fn wasm(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().and_then(|deps| deps.parent()).unwrap();
    let target = profile_dir.parent().unwrap();
    let path = target.join("wasm32-wasip2").join(profile_dir.file_name().unwrap()).join(format!("{name}.wasm"));
    assert!(path.exists(), "{} is missing; build the modules first (cargo xtask ci)", path.display());
    path
}

/// A context whose network allows loopback, so modules can reach the tests' local servers.
pub fn context() -> ModuleContext {
    let (events, _) = tokio::sync::mpsc::channel(8);
    ModuleContext { data_dir: std::env::temp_dir(), events, net: Net::with_limits(AddressPolicy::AllowLoopbackForTests, limits::for_tests()) }
}

pub async fn load(engine: &Engine, name: &str) -> ModuleHandle {
    engine.load(&wasm(name), context()).await.unwrap()
}
