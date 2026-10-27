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
use open_mess::ui::sign_in::{BUSY, NO_WINDOW, NOT_FROM_WINDOW};
use open_mess_host::{SignInReply, SignInRequest, WamAccount};
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
    for _ in 0..1500 {
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
async fn windows_accounts_are_picked_in_the_window() {
    let d = daemon(&[]).await;
    let state = state(&d);
    let _active = state.sign_ins.begin("m").unwrap();
    let offer = |request| {
        let (reply, answer) = oneshot::channel();
        state.sign_ins.offer(request, reply);
        answer
    };
    let account = |id: &str| WamAccount { id: id.into(), user_name: "ann@contoso.example".into(), tenant: format!("tenant-{id}") };
    let picked = offer(SignInRequest::WamPick { module: "m".into(), accounts: vec![account("w1"), account("w2")] });
    let shown = commands::sign_in_request(&state).unwrap();
    assert_eq!(shown.kind, "wam-pick");
    let listed: Vec<_> = shown.accounts.iter().map(|account| (account.id.as_str(), account.user_name.as_str(), account.tenant.as_str())).collect();
    assert_eq!(listed, [("w1", "ann@contoso.example", "tenant-w1"), ("w2", "ann@contoso.example", "tenant-w2")]);
    commands::sign_in_answer(&state, shown.id, AnswerDto::Done(Some("w2".into()))).unwrap();
    assert_eq!(picked.await, Ok(SignInReply::Done(Some("w2".into()))));

    // The WAM dialog's owner is the open window, answered without the user.
    assert_eq!(offer(SignInRequest::WamWindow { module: "m".into() }).await, Ok(SignInReply::Refused(NO_WINDOW.into())));
    state.sign_ins.window_opened(7);
    assert_eq!(offer(SignInRequest::WamWindow { module: "m".into() }).await, Ok(SignInReply::Window(7)));
    assert!(commands::sign_in_request(&state).is_none());
    state.sign_ins.window_closed();
    assert_eq!(offer(SignInRequest::WamWindow { module: "m".into() }).await, Ok(SignInReply::Refused(NO_WINDOW.into())));
    // Not for a module that is not signing in.
    state.sign_ins.window_opened(7);
    assert_eq!(offer(SignInRequest::WamWindow { module: "other".into() }).await, Ok(SignInReply::Refused(NOT_FROM_WINDOW.into())));
}

/// The manual check of WAM (plan E2, Task 2): test-echo signs in with the Teams client id, the
/// first Windows account is picked as the user would in the window, and the module gets a token,
/// of which only the length is shown. Talks to Windows, never to Teams; leaves no secret behind.
/// `cargo test -p open-mess --test sign_in -- --ignored --nocapture`
#[tokio::test]
#[ignore = "talks to Windows' account manager; run by hand"]
#[allow(clippy::print_stdout)]
async fn windows_account_live() {
    let d = daemon(&[ECHO]).await;
    let state = state(&d);
    let echo = d.accounts.modules().handle("test-echo").unwrap();
    echo.call("x", "sign-in-with", r#"{"how": "windows"}"#).await.unwrap();
    let signing_in = add_account(&state);
    let pick = prompt(&state, "wam-pick").await;
    println!("the window lists {} Windows account(s)", pick.accounts.len());
    let first = pick.accounts.first().expect("Windows knows no work account for the Teams client id");
    commands::sign_in_answer(&state, pick.id, AnswerDto::Done(Some(first.id.clone()))).unwrap();
    let signed_in = signing_in.await.unwrap();
    // The secret the pick left (in Credential Manager) goes, whatever happened.
    let credentials = open_mess_host::credential_store().unwrap();
    open_mess_host::delete_account_secrets(&*credentials, d.dir.path(), "test-echo", "windows").unwrap();
    let added = signed_in.unwrap();
    println!("signed in: {}", added.display_name);
    assert!(added.display_name.starts_with("Windows token of "));
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
