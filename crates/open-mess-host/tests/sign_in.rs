//! The sign-in imports: prompts reach the daemon's side of the window, the user's answer reaches
//! the module, and the wait has its own budget instead of the call limit (spec 8.3, R4).

// clippy.toml allows unwrap in #[test] functions only; the helpers here are test code too.
#![allow(clippy::unwrap_used)]

mod common;

use std::time::Duration;

use common::{context, wasm};
use open_mess_host::{Engine, ModuleContext, ModuleHandle, Prompt, SignInPrompts, SignInReply, SignInRequest, limits};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// test-echo's `sign-in` tool running, with the prompts it sends.
async fn signing_in(budget: Duration) -> (JoinHandle<String>, mpsc::Receiver<Prompt>) {
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let (sender, prompts) = mpsc::channel(8);
    let ctx = ModuleContext { sign_in: SignInPrompts::with_budget(sender, budget), ..context() };
    let echo: ModuleHandle = engine.load(&wasm("open_mess_test_echo"), ctx).await.unwrap();
    (tokio::spawn(async move { echo.call("a", "sign-in", "{}").await.unwrap() }), prompts)
}

const WAM: &str = "refused: Windows account sign-in not available yet";

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

#[tokio::test]
async fn without_a_window_sign_in_is_refused() {
    // The common context's prompts go nowhere.
    let echo = Engine::with_limits(limits::for_tests()).unwrap().load(&wasm("open_mess_test_echo"), context()).await.unwrap();
    let refused = "refused: sign-in UI not available";
    assert_eq!(echo.call("a", "sign-in", "{}").await.unwrap(), format!(r#"{{"open-page":"{refused}","wam-token":"{WAM}"}}"#));
}
