//! Time and memory limits, and what a crash does to the module and to its neighbours.

mod common;

use std::time::{Duration, Instant};

use common::load;
use open_mess_host::{Engine, HostError, ModuleError, ModuleState, limits};

#[tokio::test]
async fn lifecycle_exports_reach_the_module() {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let echo = load(&engine, "open_mess_test_echo").await;
    echo.restore("known").await.unwrap();
    let result = echo.restore("other").await;
    assert!(matches!(&result, Err(HostError::Module(ModuleError::AuthRequired(account))) if account == "other"), "{result:?}");
    echo.sign_out("other").await.unwrap();
    // No window takes its sign-in page.
    let result = echo.sign_in().await;
    assert!(matches!(&result, Err(HostError::Module(ModuleError::Platform(why))) if why == "refused: sign-in UI not available"), "{result:?}");
}

#[tokio::test]
async fn infinite_loop_is_interrupted() {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let spin = load(&engine, "open_mess_test_spin").await;
    let started = Instant::now();
    let result = spin.call("a", "spin", "{}").await;
    assert!(matches!(result, Err(HostError::Timeout)), "{result:?}");
    assert!(started.elapsed() < limits::for_tests().call_timeout + Duration::from_secs(1), "{:?}", started.elapsed());
    assert!(matches!(spin.state(), ModuleState::Crashed { count: 1, .. }), "{:?}", spin.state());
    // The module was unloaded; the next call runs on a fresh instance (and times out again).
    assert!(matches!(spin.call("a", "spin", "{}").await, Err(HostError::Timeout)));
    assert!(matches!(spin.state(), ModuleState::Crashed { count: 2, .. }), "{:?}", spin.state());
}

#[tokio::test]
async fn memory_hog_is_stopped() {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let hog = load(&engine, "open_mess_test_hog").await;
    let result = hog.call("a", "hog", "{}").await;
    assert!(matches!(&result, Err(HostError::Crashed(why)) if why.contains("memory limit")), "{result:?}");
}

#[tokio::test]
async fn three_crashes_disable_the_module() {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let hog = load(&engine, "open_mess_test_hog").await;
    for _ in 0..limits::MAX_CRASHES {
        assert!(matches!(hog.call("a", "hog", "{}").await, Err(HostError::Crashed(_))));
    }
    assert_eq!(hog.state(), ModuleState::Disabled);
    assert!(matches!(hog.call("a", "hog", "{}").await, Err(HostError::Closed)));
}

#[tokio::test]
async fn echo_round_trips() {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let echo = load(&engine, "open_mess_test_echo").await;
    let args = r#"{"text":"hello","n":[1,2,3]}"#;
    assert_eq!(echo.call("a", "echo", args).await.unwrap(), args);
    assert_eq!(echo.state(), ModuleState::Running);
}

#[tokio::test]
async fn one_module_crash_does_not_affect_another() {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let hog = load(&engine, "open_mess_test_hog").await;
    let echo = load(&engine, "open_mess_test_echo").await;
    assert!(matches!(hog.call("a", "hog", "{}").await, Err(HostError::Crashed(_))));
    assert_eq!(echo.call("a", "echo", "{}").await.unwrap(), "{}");
}

// Two threads, as in the daemon: the module's busy call holds one, and the caller gives up on the
// other meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_nobody_waits_for_is_not_run() {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    // Its own storage, where test-echo counts the calls.
    let dir = tempfile::tempdir().unwrap();
    let echo = engine.load(&common::wasm("open_mess_test_echo"), common::context_in(dir.path().to_path_buf())).await.unwrap();
    // The first call keeps the module busy for 1 s; the caller of the second gives up behind it.
    let busy = echo.call("a", "peek", r#"{"answer":"slow"}"#);
    let given_up = tokio::time::timeout(Duration::from_millis(200), echo.call("a", "peek", "{}"));
    let (busy, given_up) = tokio::join!(busy, given_up);
    assert!(busy.is_ok() && given_up.is_err(), "{busy:?} {given_up:?}");
    // Only the first ran.
    assert_eq!(echo.call("a", "peeks", "{}").await.unwrap(), r#"{"count":1}"#);
}

#[tokio::test]
async fn module_errors_are_not_crashes() {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let echo = load(&engine, "open_mess_test_echo").await;
    let result = echo.call("a", "no-such-tool", "{}").await;
    assert!(matches!(result, Err(HostError::Module(ModuleError::NotSupported(_)))), "{result:?}");
    assert_eq!(echo.state(), ModuleState::Running);
}
