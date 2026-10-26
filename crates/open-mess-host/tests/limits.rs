//! Time and memory limits, and what a crash does to the module and to its neighbours.

mod common;

use std::time::{Duration, Instant};

use common::load;
use open_mess_host::{Engine, HostError, ModuleState, limits};

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

#[tokio::test]
async fn module_errors_are_not_crashes() {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let echo = load(&engine, "open_mess_test_echo").await;
    let result = echo.call("a", "no-such-tool", "{}").await;
    assert!(matches!(result, Err(HostError::Module(open_mess_host::ModuleError::NotSupported(_)))), "{result:?}");
    assert_eq!(echo.state(), ModuleState::Running);
}
