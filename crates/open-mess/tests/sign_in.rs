//! Signing in from the configuration window: the module's prompts reach the window, its answers
//! reach the module, and the account the module signs in is added (spec 8.3, 8.8).

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Daemon, ECHO, daemon};
use open_mess::modules;
use open_mess::ui::commands::{self, AccountDto, AnswerDto, SignInPromptDto};
use open_mess::ui::sign_in::{BUSY, NO_WINDOW, NOT_FROM_WINDOW};
use open_mess::ui::{UiState, sign_in_page};
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
async fn cancelling_ends_the_whole_sign_in() {
    let d = daemon(&[]).await;
    let state = state(&d);
    let offer = |request| {
        let (reply, answer) = oneshot::channel();
        state.sign_ins.offer(request, reply);
        answer
    };
    let qr = || SignInRequest::Qr { module: "m".into(), data: "x".into() };
    let active = state.sign_ins.begin("m").unwrap();
    let first = offer(qr());
    let id = commands::sign_in_request(&state).unwrap().id;
    commands::sign_in_answer(&state, id, AnswerDto::Cancelled).unwrap();
    assert_eq!(first.await, Ok(SignInReply::Cancelled));
    // A module that asks again gets cancelled at once; nothing waits on screen.
    assert_eq!(offer(qr()).await, Ok(SignInReply::Cancelled));
    assert!(commands::sign_in_request(&state).is_none());
    drop(active);

    // Closing the window before the module's first prompt cancels it too.
    let active = state.sign_ins.begin("m").unwrap();
    state.sign_ins.window_closed();
    assert_eq!(offer(qr()).await, Ok(SignInReply::Cancelled));
    drop(active);
    // The next sign-in starts afresh.
    let _active = state.sign_ins.begin("m").unwrap();
    let _waiting = offer(qr());
    assert!(commands::sign_in_request(&state).is_some());
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
    // Closing the window ends the sign-in.
    state.sign_ins.window_closed();
    assert_eq!(offer(SignInRequest::WamWindow { module: "m".into() }).await, Ok(SignInReply::Cancelled));
    // Not for a module that is not signing in.
    state.sign_ins.window_opened(7);
    assert_eq!(offer(SignInRequest::WamWindow { module: "other".into() }).await, Ok(SignInReply::Refused(NOT_FROM_WINDOW.into())));
}

/// The manual check of WAM (plan E2, Task 2): test-echo signs in with the Teams client id, the
/// first Windows account is picked as the user would in the window, and the module gets a token,
/// of which only the length is shown. Talks to Windows, never to Teams; leaves no secret behind.
/// Only with the user present and asked for (spec 9.5):
/// `OPEN_MESS_LIVE_WAM=1 cargo test -p open-mess --test sign_in -- --ignored --nocapture`
#[tokio::test]
#[ignore = "talks to Windows' account manager; run by hand"]
#[allow(clippy::print_stdout)]
async fn windows_account_live() {
    if std::env::var_os("OPEN_MESS_LIVE_WAM").is_none() {
        println!("skipped: set OPEN_MESS_LIVE_WAM=1 to sign in with a Windows account");
        return;
    }
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
async fn unknown_sign_in_host_asks_and_remembers() {
    let d = daemon(&[ECHO]).await;
    let state = state(&d);
    let echo = d.accounts.modules().handle("test-echo").unwrap().manifest().unwrap();
    let approved = |module: &str| {
        let manifest = open_mess_host::Manifest { name: module.into(), sign_in_hosts: Vec::new(), ..echo.clone() };
        modules::approved_sign_in_hosts(&common::store(d.dir.path()), &manifest).unwrap()
    };
    let _active = state.sign_ins.begin("test-echo").unwrap();
    let page = |url: &str| {
        let (reply, answer) = oneshot::channel();
        let (url, redirect_prefix) = (url.into(), "https://login.example.com/done".into());
        state.sign_ins.offer(SignInRequest::OpenPage { module: "test-echo".into(), url, redirect_prefix }, reply);
        (state.sign_ins.shown().unwrap().0, answer)
    };

    // The page goes to the company's own sign-in server: the window asks.
    let (id, mut answer) = page("https://login.example.com/start");
    let asking = tokio::spawn({
        let (sign_ins, accounts) = (state.sign_ins.clone(), Arc::clone(&state.accounts));
        async move { sign_in_page::unknown_host(&sign_ins, &accounts, id, "test-echo", "login.contoso-adfs.example").await }
    });
    let question = prompt(&state, "ask-host").await;
    assert_eq!((question.url.as_deref(), question.module.as_str()), (Some("login.contoso-adfs.example"), "test-echo"));
    commands::sign_in_answer(&state, question.id, AnswerDto::Done(Some("allow".into()))).unwrap();
    assert!(asking.await.unwrap());
    // Yes: the page goes on and the host is approved for this module only, from now on.
    assert!(answer.try_recv().is_err());
    assert_eq!(commands::sign_in_request(&state).unwrap().kind, "page");
    assert_eq!(approved("test-echo"), ["login.contoso-adfs.example"]);
    assert!(approved("teams").is_empty());

    // No: the sign-in ends as refused.
    let asking = tokio::spawn({
        let (sign_ins, accounts) = (state.sign_ins.clone(), Arc::clone(&state.accounts));
        async move { sign_in_page::unknown_host(&sign_ins, &accounts, id, "test-echo", "evil.example").await }
    });
    let question = prompt(&state, "ask-host").await;
    commands::sign_in_answer(&state, question.id, AnswerDto::Cancelled).unwrap();
    assert!(!asking.await.unwrap());
    assert!(matches!(answer.await, Ok(SignInReply::Refused(_))));
    assert_eq!(approved("test-echo"), ["login.contoso-adfs.example"]);
}

#[tokio::test]
async fn sign_in_page_closed_by_the_user_cancels() {
    let d = daemon(&[ECHO]).await;
    let state = state(&d);
    let signing_in = add_account(&state);
    let page = prompt(&state, "page").await;
    state.sign_ins.page_closed(page.id);
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
