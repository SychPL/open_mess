//! Signing in from the configuration window: the module's prompts reach the window, its answers
//! reach the module, and the account the module signs in is added (spec 8.3, 8.8).

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Daemon, ECHO, daemon};
use open_mess::ui::UiState;
use open_mess::ui::commands::{self, AccountDto, AnswerDto, SignInPromptDto};
use open_mess::ui::sign_in::{BUSY, NOT_FROM_WINDOW};
use open_mess_host::{SignInReply, SignInRequest};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

fn state(d: &Daemon) -> Arc<UiState> {
    Arc::new(UiState {
        accounts: Arc::clone(&d.accounts),
        pending: Arc::clone(d.registry.pending().unwrap()),
        started: Instant::now(),
        port: 7341,
        token: "t0ken".into(),
        sign_ins: d.sign_ins.clone(),
    })
}

/// `Dodaj konto` for test-echo, running.
fn add_account(state: &Arc<UiState>) -> JoinHandle<Result<AccountDto, String>> {
    let state = Arc::clone(state);
    tokio::spawn(async move { commands::sign_in_start(&state, "test-echo").await })
}

/// Waits until the window shows a prompt of `kind`.
async fn prompt(state: &UiState, kind: &str) -> SignInPromptDto {
    for _ in 0..500 {
        if let Some(prompt) = commands::sign_in_request(state).filter(|prompt| prompt.kind == kind) {
            return prompt;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the window shows no {kind} prompt");
}

#[tokio::test]
async fn prompts_are_shown_in_the_window() {
    let d = daemon(&[]).await;
    let state = state(&d);
    let _active = state.sign_ins.begin("m").unwrap();
    let offer = |request| {
        let (reply, answer) = oneshot::channel();
        state.sign_ins.offer(request, reply);
        answer
    };
    let code = offer(SignInRequest::DeviceCode { module: "m".into(), code: "ABCD-1234".into(), url: "https://login.example.com/device".into() });
    let shown = commands::sign_in_request(&state).unwrap();
    assert_eq!((shown.kind, shown.module.as_str(), shown.code.as_deref()), ("device-code", "m", Some("ABCD-1234")));
    assert_eq!(shown.url.as_deref(), Some("https://login.example.com/device"));

    // A newer prompt replaces it.
    offer(SignInRequest::Qr { module: "m".into(), data: "otpauth://example".into() });
    assert_eq!(code.await, Ok(SignInReply::Cancelled));
    let shown = commands::sign_in_request(&state).unwrap();
    assert_eq!(shown.kind, "qr");
    assert!(shown.qr_svg.unwrap().contains("<svg"));

    let url = "https://login.example.com/start?client=1".into();
    offer(SignInRequest::OpenPage { module: "m".into(), url, redirect_prefix: "https://login.example.com/done".into() });
    let shown = commands::sign_in_request(&state).unwrap();
    // The window names the page's host; the page itself opens in its own window.
    assert_eq!((shown.kind, shown.url.as_deref()), ("page", Some("login.example.com")));
    // The window can't make up the page's answer, only cancel it.
    assert!(commands::sign_in_answer(&state, shown.id, AnswerDto::Done(Some("https://login.example.com/done".into()))).is_err());
    assert!(commands::sign_in_open_link(&state, shown.id).is_err());

    // Another module's prompt is refused.
    let other = offer(SignInRequest::Qr { module: "other".into(), data: "x".into() });
    assert_eq!(other.await, Ok(SignInReply::Refused(NOT_FROM_WINDOW.into())));
    assert_eq!(commands::sign_in_request(&state).unwrap().kind, "page");
}

#[tokio::test]
async fn sign_in_from_window_adds_the_account() {
    let d = daemon(&[ECHO]).await;
    let state = state(&d);
    let signing_in = add_account(&state);
    let page = prompt(&state, "page").await;
    // One sign-in at a time.
    assert_eq!(commands::sign_in_start(&state, "test-echo").await.unwrap_err(), BUSY);
    // The sign-in window saw the redirect.
    state.sign_ins.answer(page.id, SignInReply::Done(Some("https://login.example.com/done?account=a&code=1".into()))).unwrap();
    let added = signing_in.await.unwrap().unwrap();
    assert_eq!((added.id.as_str(), added.display_name.as_str(), added.state.as_str()), ("test-echo:a", "Echo a", "online"));
    assert_eq!(commands::accounts(&state).unwrap().len(), 1);
    // Nothing is left on screen.
    assert!(commands::sign_in_request(&state).is_none());
}

#[tokio::test]
async fn cancel_reaches_the_module() {
    let d = daemon(&[ECHO]).await;
    let state = state(&d);
    let signing_in = add_account(&state);
    let page = prompt(&state, "page").await;
    commands::sign_in_answer(&state, page.id, AnswerDto::Cancelled).unwrap();
    let failed = signing_in.await.unwrap().unwrap_err();
    assert!(failed.contains("cancelled"), "{failed}");
    assert!(commands::accounts(&state).unwrap().is_empty());
    // An answer to a prompt that no longer waits is refused.
    assert!(commands::sign_in_answer(&state, page.id, AnswerDto::Cancelled).is_err());

    // Closing the window cancels too.
    let signing_in = add_account(&state);
    prompt(&state, "page").await;
    state.sign_ins.window_closed();
    assert!(signing_in.await.unwrap().unwrap_err().contains("cancelled"));
}

#[tokio::test]
async fn prompts_outside_the_window_are_refused() {
    let d = daemon(&[ECHO]).await;
    // test-echo's `sign-in` tool prompts in an ordinary call: no sign-in from the window runs.
    let echo = d.accounts.modules().handle("test-echo").unwrap();
    let answers: serde_json::Value = serde_json::from_str(&echo.call("a", "sign-in", "{}").await.unwrap()).unwrap();
    assert_eq!(answers["open-page"], format!("refused: {NOT_FROM_WINDOW}"));
    assert!(d.sign_ins.shown().is_none());
}
