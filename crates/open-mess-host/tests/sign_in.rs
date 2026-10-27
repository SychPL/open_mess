//! The sign-in imports: prompts reach the daemon's side of the window, the user's answer reaches
//! the module, and the wait has its own budget instead of the call limit (spec 8.3, R4).

// clippy.toml allows unwrap in #[test] functions only; the helpers here are test code too.
#![allow(clippy::unwrap_used)]

mod common;

use std::time::Duration;

use common::{context, context_in, wasm};
use open_mess_host::{
    AccountInfo, Engine, HostError, ModuleContext, ModuleError, ModuleHandle, ModuleState, Prompt, SignInPrompts, SignInReply, SignInRequest, limits,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// test-echo's `sign-in` tool running, with the prompts it sends.
async fn signing_in(budget: Duration) -> (JoinHandle<String>, mpsc::Receiver<Prompt>) {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let (sender, prompts) = mpsc::channel(8);
    let ctx = ModuleContext { sign_in: SignInPrompts::with_budget(sender, budget), ..context() };
    let echo: ModuleHandle = engine.load(&wasm("open_mess_test_echo"), ctx).await.unwrap();
    (tokio::spawn(async move { echo.call("a", "sign-in", "{}").await.unwrap() }), prompts)
}

/// test-echo asks for a scope its manifest does not declare.
const WAM: &str = r#"refused: scope \"scope\" is not in the module's manifest"#;

#[tokio::test]
async fn device_code_reaches_the_window() {
    let (call, mut prompts) = signing_in(Duration::from_secs(10)).await;
    let (code, _) = prompts.recv().await.unwrap();
    let url = "https://login.example.com/device".into();
    assert_eq!(code, SignInRequest::DeviceCode { module: "test-echo".into(), code: "ABCD-1234".into(), url });
    let (page, reply) = prompts.recv().await.unwrap();
    let (url, redirect_prefix) = ("https://login.example.com/".into(), "https://login.example.com/done".into());
    assert_eq!(page, SignInRequest::OpenPage { module: "test-echo".into(), url, redirect_prefix });
    // The user takes longer than the call limit: waiting for them does not count.
    tokio::time::sleep(limits::for_tests().call_timeout + Duration::from_millis(500)).await;
    reply.send(SignInReply::Done(Some("https://login.example.com/done?code=1".into()))).unwrap();
    assert_eq!(call.await.unwrap(), format!(r#"{{"open-page":"ok","wam-token":"{WAM}"}}"#));
}

#[tokio::test]
async fn cancel_reaches_the_module() {
    let (call, mut prompts) = signing_in(Duration::from_secs(10)).await;
    prompts.recv().await.unwrap();
    let (_, reply) = prompts.recv().await.unwrap();
    reply.send(SignInReply::Cancelled).unwrap();
    assert_eq!(call.await.unwrap(), format!(r#"{{"open-page":"cancelled","wam-token":"{WAM}"}}"#));
}

#[tokio::test]
async fn budget_expires_as_timed_out() {
    let (call, mut prompts) = signing_in(Duration::from_millis(300)).await;
    prompts.recv().await.unwrap();
    // Nobody answers the page.
    let (_unanswered, _reply) = prompts.recv().await.unwrap();
    assert_eq!(call.await.unwrap(), format!(r#"{{"open-page":"timed-out","wam-token":"{WAM}"}}"#));
}

/// test-echo signing in with a device code (in its own data folder), and its prompts.
async fn device_code(
    data_dir: &std::path::Path,
    budget: Duration,
) -> (ModuleHandle, JoinHandle<Result<AccountInfo, HostError>>, mpsc::Receiver<Prompt>) {
    sign_in_with(data_dir, "device-code", budget).await
}

/// test-echo signing in the way `how` names (its `sign-in-with` tool), in its own data folder.
async fn sign_in_with(
    data_dir: &std::path::Path,
    how: &str,
    budget: Duration,
) -> (ModuleHandle, JoinHandle<Result<AccountInfo, HostError>>, mpsc::Receiver<Prompt>) {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let (sender, prompts) = mpsc::channel(8);
    let ctx = ModuleContext { sign_in: SignInPrompts::with_budget(sender, budget), ..context_in(data_dir.to_path_buf()) };
    let echo = engine.load(&wasm("open_mess_test_echo"), ctx).await.unwrap();
    echo.call("a", "sign-in-with", &format!(r#"{{"how": "{how}"}}"#)).await.unwrap();
    let signing_in = tokio::spawn({
        let echo = echo.clone();
        async move { echo.sign_in().await }
    });
    (echo, signing_in, prompts)
}

#[tokio::test]
async fn code_on_screen_does_not_count_and_cancel_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let (echo, signing_in, mut prompts) = device_code(dir.path(), Duration::from_secs(30)).await;
    let (code, reply) = prompts.recv().await.unwrap();
    assert!(matches!(&code, SignInRequest::DeviceCode { code, .. } if code == "WXYZ-5678"), "{code:?}");
    // The module polls while the user takes longer than the call limit.
    tokio::time::sleep(limits::for_tests().call_timeout + Duration::from_secs(1)).await;
    assert!(!signing_in.is_finished());
    reply.send(SignInReply::Cancelled).unwrap();
    let result = signing_in.await.unwrap();
    assert!(matches!(&result, Err(HostError::Module(ModuleError::Platform(why))) if why == "sign-in cancelled"), "{result:?}");
    // Not a crash.
    assert_eq!(echo.state(), ModuleState::Running);
}

#[tokio::test]
async fn code_budget_expires_as_timed_out() {
    let dir = tempfile::tempdir().unwrap();
    let (echo, signing_in, mut prompts) = device_code(dir.path(), Duration::from_millis(500)).await;
    let (_code, _unanswered) = prompts.recv().await.unwrap();
    let result = signing_in.await.unwrap();
    assert!(matches!(&result, Err(HostError::Module(ModuleError::Platform(why))) if why == "sign-in timed out"), "{result:?}");
    assert_eq!(echo.state(), ModuleState::Running);
}

#[tokio::test]
async fn cancel_stops_a_module_that_keeps_showing_codes() {
    let dir = tempfile::tempdir().unwrap();
    let (_echo, signing_in, mut prompts) = sign_in_with(dir.path(), "qr-loop", Duration::from_secs(30)).await;
    let started = std::time::Instant::now();
    let cancel_at = limits::for_tests().call_timeout + Duration::from_secs(1);
    // As the window's side does: a newer code replaces the shown one, and once the user cancelled,
    // every later one is cancelled at once.
    let window = tokio::spawn(async move {
        let mut shown: Option<oneshot::Sender<SignInReply>> = None;
        while let Some((_, reply)) = prompts.recv().await {
            if let Some(old) = shown.take() {
                let _ = old.send(SignInReply::Cancelled);
            }
            if started.elapsed() < cancel_at {
                shown = Some(reply);
            } else {
                let _ = reply.send(SignInReply::Cancelled);
            }
        }
    });
    // Until the cancel the codes stop the clock; after it the call limit stops the module, which
    // would show codes forever.
    let result = signing_in.await.unwrap();
    assert!(matches!(result, Err(HostError::Timeout)), "{result:?}");
    assert!(started.elapsed() > cancel_at, "{:?}", started.elapsed());
    window.abort();
}

#[tokio::test]
async fn without_a_window_sign_in_is_refused() {
    // The common context's prompts go nowhere.
    let echo = Engine::with_limits(limits::for_tests()).unwrap().load(&wasm("open_mess_test_echo"), context()).await.unwrap();
    let refused = "refused: sign-in UI not available";
    assert_eq!(echo.call("a", "sign-in", "{}").await.unwrap(), format!(r#"{{"open-page":"{refused}","wam-token":"{WAM}"}}"#));
}
