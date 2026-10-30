//! The window in English (spec 2026-10-30-ui-languages, 10.7): the states whose Polish texts ui.rs,
//! sign_in.rs and update_service.rs check, with the window's language set to `en`. The language is
//! the process's, so every daemon of this file speaks English.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Daemon, ECHO, echo, info, install, scan, store};
use open_mess::i18n::Lang;
use open_mess::pending::Pending;
use open_mess::triggers::Counters;
use open_mess::triggers::channel::Sessions;
use open_mess::triggers::rules::{Action, Condition, Rule};
use open_mess::triggers::spawn::{RUN_TIMEOUT, Runs};
use open_mess::ui::commands::{self, AccountDto, AnswerDto, LanguageDto, RuleDto, SignInPromptDto};
use open_mess::ui::sign_in::SignIns;
use open_mess::ui::{UiState, sign_in_page};
use open_mess::update::UpdateError;
use open_mess::update::fetch::Source;
use open_mess::update::quiesce::{Activity, Launcher};
use open_mess::update::service::{NotInstallable, Setup, UpdateService};
use open_mess::update::state::Paths;
use open_mess_host::{Engine, SignInReply};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};

async fn daemon(files: &[(&str, &str)]) -> Daemon {
    common::daemon_in(files, "en").await
}

fn state(d: &Daemon) -> Arc<UiState> {
    Arc::new(UiState {
        accounts: Arc::clone(&d.accounts),
        pending: Arc::clone(d.registry.pending().unwrap()),
        started: Instant::now(),
        port: 7341,
        token: "t0ken".into(),
        sign_ins: d.sign_ins.clone(),
        counters: d.registry.trigger_counters().clone(),
        sessions: d.registry.channel_sessions().clone(),
        gate: d.registry.gate().clone(),
        activity: Arc::default(),
        runs: Runs::new(d.dir.path().join("runs"), RUN_TIMEOUT, Counters::default()),
        host_events: mpsc::channel(1).0,
        ui_ready: Arc::default(),
        exit: Arc::default(),
        probation: d.registry.pending().unwrap().probation().clone(),
        update: None,
    })
}

/// A daemon whose modules folder has `files`, none approved; its window speaks English.
async fn unapproved(files: &[(&str, Vec<u8>)]) -> (TempDir, UiState) {
    let dir = tempfile::tempdir().unwrap();
    for (file, bytes) in files {
        install(dir.path(), file, bytes);
    }
    common::set_language(&store(dir.path()), "en");
    let modules = scan(&store(dir.path()), &Engine::new().unwrap(), dir.path()).await;
    let accounts = common::accounts(dir.path(), modules, keyring_core::mock::Store::new().unwrap());
    let pending = Arc::new(Pending::new(Arc::clone(&accounts)));
    let state = UiState {
        accounts,
        probation: pending.probation().clone(),
        pending,
        started: Instant::now(),
        port: 7341,
        token: "t0ken".into(),
        sign_ins: SignIns::default(),
        counters: Counters::default(),
        sessions: Sessions::default(),
        gate: Default::default(),
        activity: Arc::default(),
        runs: Runs::new(dir.path().join("runs"), RUN_TIMEOUT, Counters::default()),
        host_events: mpsc::channel(1).0,
        ui_ready: Arc::default(),
        exit: Arc::default(),
        update: None,
    };
    (dir, state)
}

#[tokio::test]
async fn modules_in_english() {
    let (dir, state) = unapproved(&[("echo.wasm", echo())]).await;
    assert_eq!(commands::language(&state), LanguageDto { setting: "en", lang: "en" });
    let listed = commands::modules(&state).unwrap();
    assert_eq!(listed[0].detail, "awaiting approval: new module");
    assert_eq!(commands::approve_module(&state, "test-echo", "00", "00").await, Err("the module file changed - refresh".to_string()));
    assert_eq!(commands::set_module_enabled(&state, "nope", true).await, Err("no module nope".to_string()));
    install(dir.path(), "broken.wasm", b"not a component");
    commands::rescan_modules(&state).await.unwrap();
    let problems = commands::status(&state).unwrap().problems;
    assert!(problems[0].starts_with("broken.wasm (broken.wasm): not a valid module: "), "{problems:?}");
    assert_eq!(commands::set_language(&state, "de"), Err("unknown language".to_string()));
}

#[tokio::test]
async fn saves_during_an_update_are_refused_in_english() {
    let d = daemon(&[]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    state.probation.set(true);
    let refused = Err("update in progress".to_string());
    assert_eq!(commands::set_permissions(&state, "test-echo:a", None, true, "never"), refused);
    assert_eq!(commands::set_language(&state, "pl").map(drop), refused);
    assert_eq!(commands::language(&state), LanguageDto { setting: "en", lang: "en" });
    state.probation.set(false);
    assert_eq!(commands::set_permissions(&state, "test-echo:nobody", None, true, "never"), Err("no account test-echo:nobody".to_string()));
    assert_eq!(commands::set_permissions(&state, "test-echo:a", Some(" ".into()), true, "never"), Err("enter a conversation id".to_string()));
    assert_eq!(commands::delete_rule(&state, "r1"), Err("no rule r1".to_string()));
}

#[tokio::test]
async fn window_texts_of_actions_and_rules_in_english() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    // The message replied to is longer than an approval shows.
    let echo = d.accounts.modules().handle("test-echo").unwrap();
    echo.call("a", "set-target", &json!({"conversation": "c1", "text": "x".repeat(28_001)}).to_string()).await.unwrap();
    let sent = d.registry.call("send", json!({"account": "test-echo:a", "conversation": "c1", "text": "ok", "reply_to": "m1"})).await.unwrap();
    let refused = commands::decide(&state, sent["action"].as_str().unwrap(), true).await;
    assert_eq!(refused, Err("the message this action is about can't be shown in full - it can only be rejected".to_string()));

    commands::set_permissions(&state, "test-echo:a", None, true, "autonomous").unwrap();
    let rule = RuleDto {
        id: "r1".into(),
        account: "test-echo:a".into(),
        enabled: true,
        rule: Rule::new("r1", "test-echo:a", Condition::Direct, Action::Channel { fallback_spawn: None }),
    };
    let warning = commands::put_rule(&state, rule).unwrap().unwrap();
    assert_eq!(warning, commands::autonomous_rule(Lang::En));
    assert!(warning.contains("\"autonomously\"") && warning.contains("\"with confirmation\"") && warning.contains("Agent permissions"), "{warning}");
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

/// Signs in through test-echo's page as the account the redirect names; `again` re-signs one.
async fn sign_in_as(state: &Arc<UiState>, again: Option<&str>, account: &str) -> Result<AccountDto, String> {
    let signing_in = {
        let (state, again) = (Arc::clone(state), again.map(str::to_owned));
        tokio::spawn(async move { commands::sign_in_start(&state, "test-echo", again.as_deref()).await })
    };
    let page = prompt(state, "page").await;
    // Only cancelled from the window: the page's own redirect answers it.
    let made_up = commands::sign_in_answer(state, page.id, AnswerDto::Done(Some("https://login.example.com/done".into())));
    assert_eq!(made_up, Err("a sign-in page can only be cancelled".to_string()));
    let redirect = format!("https://login.example.com/done?account={account}&code=1");
    state.sign_ins.answer(page.id, SignInReply::Done(Some(redirect))).unwrap();
    signing_in.await.unwrap()
}

#[tokio::test]
async fn sign_in_errors_in_english() {
    let d = daemon(&[ECHO]).await;
    let state = state(&d);
    let module_gone = commands::sign_in_start(&state, "nope", None).await;
    assert_eq!(module_gone.map(drop), Err("module nope is not running".to_string()));
    let nobody = commands::sign_in_start(&state, "test-echo", Some("test-echo:nobody")).await;
    assert_eq!(nobody.map(drop), Err("no account test-echo:nobody in module test-echo".to_string()));

    // One sign-in at a time; an answer to a prompt that no longer waits is refused.
    let signing_in = {
        let state = Arc::clone(&state);
        tokio::spawn(async move { commands::sign_in_start(&state, "test-echo", None).await })
    };
    let page = prompt(&state, "page").await;
    let busy = commands::sign_in_start(&state, "test-echo", None).await;
    assert_eq!(busy.map(drop), Err("another sign-in is in progress - finish or cancel it".to_string()));
    commands::sign_in_answer(&state, page.id, AnswerDto::Cancelled).unwrap();
    assert!(signing_in.await.unwrap().is_err());
    assert_eq!(commands::sign_in_answer(&state, page.id, AnswerDto::Cancelled), Err("this sign-in request is no longer waiting".to_string()));

    // Signed in again as another account: refused.
    sign_in_as(&state, None, "a").await.unwrap();
    let other = sign_in_as(&state, Some("test-echo:a"), "b").await.unwrap_err();
    assert_eq!(
        other,
        "this is another account - sign in with the account you are signing in again, or remove the old account first and add the new one"
    );
    // A sign-in page outside the approved sign-in hosts is refused, in the window's language.
    assert_eq!(
        sign_in_page::check_page("https://evil.example/", "https://login.example.com/done", &["login.example.com".into()]),
        Err("the module asked for a sign-in page outside its approved sign-in hosts".to_string())
    );
}

struct NoHelper;

impl Launcher for NoHelper {
    fn launch_helper(&self, _helper: &Path, _data: &Path) -> Result<(), UpdateError> {
        Err(UpdateError::HelperNotStarted)
    }
}

/// The update service of `d`, which checks nothing within a test (its first check is 5 minutes
/// away), with `installable` and the data folder's `last-update.txt`.
fn updates(d: &Daemon, installable: Result<(), NotInstallable>, local: &Path) -> UpdateService {
    let paths = Paths { data: d.dir.path().to_path_buf(), local: local.to_path_buf() };
    let source = Source { latest: "http://127.0.0.1:9/latest".parse().unwrap(), allowed_hosts: vec!["127.0.0.1".into()], https_only: false };
    UpdateService::spawn(Setup {
        paths,
        source,
        public_key: String::new(),
        accounts: Arc::clone(&d.accounts),
        activity: Activity {
            gate: d.registry.gate().clone(),
            triggers: Arc::default(),
            pending: Arc::clone(d.registry.pending().unwrap()),
            runs: Runs::new(d.dir.path().join("runs"), RUN_TIMEOUT, Counters::default()),
            modules: Vec::new(),
            host_events: mpsc::channel(1).0,
        },
        launcher: Arc::new(NoHelper),
        exit: Arc::new(Notify::new()),
        installable,
        announce: Arc::new(|_: &UpdateService, _: &open_mess::update::release::Version, _: &str| {}),
    })
}

#[tokio::test]
async fn updates_in_english() {
    let d = daemon(&[]).await;
    let local = tempfile::tempdir().unwrap();
    let (running, installed) = (local.path().join("target").join("om.exe"), local.path().join("bin").join("om.exe"));
    let service = updates(&d, Err(NotInstallable::OtherExe { running: running.clone(), installed: installed.clone() }), local.path());
    let reason = format!("this daemon runs from {}, not from {}: scripts/install.ps1 installs new versions", running.display(), installed.display());
    assert_eq!(service.view().installable, Err(reason.clone()));
    assert_eq!(service.install(), Err(reason));
    let other_data = NotInstallable::OtherDataDir { data: "C:\\a".into(), task: "C:\\b".into() };
    assert_eq!(
        other_data.text(Lang::En),
        "the data folder C:\\a is not the one the open_mess task gives the daemon (C:\\b): scripts/install.ps1 installs new versions"
    );
    assert_eq!(NotInstallable::UnknownTaskDataDir.text(Lang::En), "it is not known which data folder the open_mess task gives");

    // The last result, as the helper or the daemon wrote it.
    std::fs::write(d.dir.path().join("last-update.txt"), "aborted: the update helper did not start\n").unwrap();
    assert_eq!(service.view().last_result.as_deref(), Some("aborted: the update helper did not start"));

    let installable = updates(&d, Ok(()), local.path());
    assert_eq!(installable.install(), Err("no downloaded version to install".to_string()));
    d.registry.pending().unwrap().probation().set(true);
    assert_eq!(installable.set_auto_install(true), Err("update in progress".to_string()));
    assert_eq!(installable.install(), Err("update in progress".to_string()));
    d.registry.pending().unwrap().probation().set(false);
}
