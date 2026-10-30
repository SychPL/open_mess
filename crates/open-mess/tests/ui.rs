//! The configuration window's commands, without a webview: accounts, modules and their approval,
//! and the trigger rules.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::NaiveTime;
use common::{Daemon, ECHO, daemon, echo, echo_rebuilt, echo_with_another_host, info, install, scan, store};
use open_mess::approval;
use open_mess::events::{Bus, Event};
use open_mess::i18n::Lang;
use open_mess::pending::{Kind, Pending, Preview, State};
use open_mess::tools::ToolError;
use open_mess::triggers::Counters;
use open_mess::triggers::channel::Sessions;
use open_mess::triggers::engine::{self, Batch, Delivery, DeliveryResult};
use open_mess::triggers::rules::{Action, Condition, Rule, Spawn};
use open_mess::triggers::spawn::{RUN_TIMEOUT, Runs};
use open_mess::ui::UiState;
use open_mess::ui::commands::{self, LanguageDto, MODULE_CHANGED, ModuleDto, RuleDto};
use open_mess::ui::sign_in::SignIns;
use open_mess_host::Engine;
use serde_json::{Value, json};
use tempfile::TempDir;

fn state(d: &Daemon) -> UiState {
    UiState {
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
        host_events: tokio::sync::mpsc::channel(1).0,
        ui_ready: Arc::default(),
        exit: Arc::default(),
        probation: d.registry.pending().unwrap().probation().clone(),
        update: None,
    }
}

fn modules(state: &UiState) -> Vec<ModuleDto> {
    commands::modules(state).unwrap()
}

fn module<'a>(list: &'a [ModuleDto], status: &str) -> &'a ModuleDto {
    list.iter().find(|module| module.status == status).unwrap_or_else(|| panic!("no {status} module in {list:?}"))
}

/// A daemon whose modules folder has `files`, none approved; its window speaks Polish.
async fn unapproved(files: &[(&str, Vec<u8>)]) -> (TempDir, UiState) {
    let dir = tempfile::tempdir().unwrap();
    for (file, bytes) in files {
        install(dir.path(), file, bytes);
    }
    let engine = Engine::new().unwrap();
    common::set_language(&store(dir.path()), "pl");
    let modules = scan(&store(dir.path()), &engine, dir.path()).await;
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
        host_events: tokio::sync::mpsc::channel(1).0,
        ui_ready: Arc::default(),
        exit: Arc::default(),
        update: None,
    };
    (dir, state)
}

#[tokio::test]
async fn module_dto_lists_hosts_and_identity() {
    let (_dir, state) = unapproved(&[("teams.wasm", std::fs::read(common::wasm("open_mess_teams")).unwrap())]).await;
    let list = modules(&state);
    assert_eq!(list.len(), 1);
    let teams = &list[0];
    assert_eq!((teams.name.as_str(), teams.status, teams.detail.as_str()), ("teams", "awaiting-approval", "czeka na zatwierdzenie: nowy moduł"));
    assert!(teams.hosts.contains(&"*.sharepoint.com".to_string()), "{:?}", teams.hosts);
    assert_eq!(teams.sign_in_hosts, ["login.microsoftonline.com"]);
    let wam = teams.wam.as_ref().unwrap();
    assert_eq!(wam.client_id, "1fec8e78-bce4-4aaf-ab1b-5451cc387264");
    assert_eq!(wam.scopes.len(), 3);
    assert_eq!(teams.file_sha256.len(), 64);
    assert_eq!(teams.manifest_sha256.len(), 64);
    // A new module takes over nothing.
    assert!(!teams.takes_over);
    assert!(teams.changes.is_empty());
}

#[tokio::test]
async fn stale_module_approval_is_refused() {
    let (dir, state) = unapproved(&[("echo.wasm", echo())]).await;
    let shown = modules(&state)[0].clone();
    // The file changes after the window showed it.
    install(dir.path(), "echo.wasm", &echo_rebuilt());
    let refused = commands::approve_module(&state, "test-echo", &shown.file_sha256, &shown.manifest_sha256).await;
    assert_eq!(refused, Err(MODULE_CHANGED.to_string()));
    assert!(state.accounts.modules().handle("test-echo").is_none());
    // Made-up hashes are refused the same way.
    assert_eq!(commands::approve_module(&state, "test-echo", "00", "00").await, Err(MODULE_CHANGED.to_string()));

    // Refreshed, the window shows the new file, and approving that loads it.
    commands::rescan_modules(&state).await.unwrap();
    let fresh = modules(&state)[0].clone();
    assert_ne!(fresh.file_sha256, shown.file_sha256);
    commands::approve_module(&state, "test-echo", &fresh.file_sha256, &fresh.manifest_sha256).await.unwrap();
    assert_eq!(modules(&state)[0].status, "active");
    assert!(state.accounts.modules().handle("test-echo").is_some());

    // Turned off and on again without a restart.
    commands::set_module_enabled(&state, "test-echo", false).await.unwrap();
    assert_eq!(modules(&state)[0].status, "disabled");
    assert!(state.accounts.modules().handle("test-echo").is_none());
    commands::set_module_enabled(&state, "test-echo", true).await.unwrap();
    assert!(state.accounts.modules().handle("test-echo").is_some());
    assert!(commands::set_module_enabled(&state, "nope", true).await.is_err());
}

#[tokio::test]
async fn rescan_keeps_running_version_until_approved() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    let waiting = state.pending.create(Kind::Send, "test-echo:a", "c1", json!({"conversation": "c1", "text": "hi"})).await.unwrap();

    // A rebuild that may reach one more host replaces the file.
    install(d.dir.path(), "echo.wasm", &echo_with_another_host());
    commands::rescan_modules(&state).await.unwrap();
    let running = d.accounts.modules().handle("test-echo").unwrap();
    assert_eq!(running.manifest().unwrap().hosts, ["localhost"]);
    let list = modules(&state);
    assert_eq!(list.len(), 2);
    assert_eq!(module(&list, "active").hosts, ["localhost"]);
    let replacement = module(&list, "replacement").clone();
    assert_eq!(replacement.hosts, ["localhost", "extra.example.com"]);
    assert_eq!(replacement.changes, ["nowy host: extra.example.com"]);
    assert!(replacement.takes_over);
    // The account and its waiting action are untouched.
    assert_eq!(commands::accounts(&state).unwrap()[0].state, "online");
    assert_eq!(state.pending.state(&waiting.action).unwrap(), State::Waiting);
    // Another rescan changes nothing.
    commands::rescan_modules(&state).await.unwrap();
    assert_eq!(modules(&state).len(), 2);

    // Approved, the new file takes over; what the old one described for approval expires.
    commands::approve_module(&state, "test-echo", &replacement.file_sha256, &replacement.manifest_sha256).await.unwrap();
    let list = modules(&state);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, "active");
    assert_eq!(d.accounts.modules().handle("test-echo").unwrap().manifest().unwrap().hosts, ["localhost", "extra.example.com"]);
    assert_eq!(state.pending.state(&waiting.action).unwrap(), State::Expired);
    assert!(state.pending.waiting().is_empty());
    // test-echo restores only its "known" account; the account was asked again.
    assert_eq!(commands::accounts(&state).unwrap()[0].state, "auth-required");
}

#[tokio::test]
async fn remove_account_from_window() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    d.accounts.add("test-echo", info("b")).unwrap();
    let state = state(&d);
    let accounts = commands::accounts(&state).unwrap();
    assert_eq!(
        accounts.iter().map(|account| (account.id.as_str(), account.platform.as_deref(), account.display_name.as_str())).collect::<Vec<_>>(),
        [("test-echo:a", Some("test"), "User a"), ("test-echo:b", Some("test"), "User b")]
    );

    commands::remove_account(&state, "test-echo:a").await.unwrap();
    let ids: Vec<String> = commands::accounts(&state).unwrap().into_iter().map(|account| account.id).collect();
    assert_eq!(ids, ["test-echo:b"]);
    assert!(commands::remove_account(&state, "test-echo:a").await.is_err());
}

#[tokio::test]
async fn status_and_mcp_command() {
    let d = daemon(&[ECHO]).await;
    install(d.dir.path(), "broken.wasm", b"not a component");
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    commands::rescan_modules(&state).await.unwrap();
    let status = commands::status(&state).unwrap();
    assert_eq!((status.accounts, status.accounts_online, status.modules, status.modules_active), (1, 1, 2, 1));
    assert_eq!(status.port, 7341);
    assert_eq!(status.problems.len(), 1);
    assert!(status.problems[0].starts_with("broken.wasm (broken.wasm): to nie jest prawidłowy moduł"), "{:?}", status.problems);
    let commands = commands::mcp_command(&state);
    assert_eq!(
        commands.claude,
        "claude mcp add --transport http --scope user open_mess http://127.0.0.1:7341/mcp --header \"Authorization: Bearer t0ken\""
    );
    // For Codex's config.toml: the token stays in Codex's own configuration, never in an environment
    // variable, which Codex hands on to the commands it runs, nor in a shell's history.
    assert_eq!(commands.codex, "[mcp_servers.open_mess]\nurl = \"http://127.0.0.1:7341/mcp\"\nhttp_headers = { Authorization = \"Bearer t0ken\" }");
    assert!(!commands.codex.contains("setx") && !commands.codex.contains("OPEN_MESS_TOKEN"));
}

#[tokio::test]
async fn set_and_clear_override() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    let send = |conversation: &str| d.registry.call("send", json!({"account": "test-echo:a", "conversation": conversation, "text": "hi"}));

    let defaults = commands::permissions(&state, "test-echo:a").unwrap();
    assert_eq!((defaults.read, defaults.write.as_str(), defaults.overrides.len()), (true, "confirm", 0));

    commands::set_permissions(&state, "test-echo:a", None, false, "never").unwrap();
    commands::set_permissions(&state, "test-echo:a", Some("c1".into()), true, "autonomous").unwrap();
    let set = commands::permissions(&state, "test-echo:a").unwrap();
    assert_eq!((set.read, set.write.as_str()), (false, "never"));
    assert_eq!(set.overrides.iter().map(|o| (o.conversation.as_str(), o.read, o.write.as_str())).collect::<Vec<_>>(), [("c1", true, "autonomous")]);
    // The core enforces what the window set.
    assert_eq!(send("c1").await.unwrap()["state"], "done");
    assert!(matches!(send("c2").await, Err(ToolError::NotAllowed(_))));

    commands::clear_permission_override(&state, "test-echo:a", "c1").unwrap();
    assert!(commands::permissions(&state, "test-echo:a").unwrap().overrides.is_empty());
    assert!(matches!(send("c1").await, Err(ToolError::NotAllowed(_))));

    // Nonsense is refused, not stored.
    assert!(commands::set_permissions(&state, "test-echo:a", None, true, "sometimes").is_err());
    assert!(commands::set_permissions(&state, "test-echo:nobody", None, true, "never").is_err());
    assert!(commands::set_permissions(&state, "test-echo:a", Some(" ".into()), true, "never").is_err());
    assert!(commands::permissions(&state, "test-echo:nobody").is_err());
}

#[tokio::test]
async fn decide_from_window_updates_state() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    let changed = state.pending.subscribe();
    let text = format!("<img src=x onerror=alert(1)>\n{}", "line\n".repeat(3000));
    let sent = d.registry.call("send", json!({"account": "test-echo:a", "conversation": "c1", "text": text, "reply_to": "m7"})).await.unwrap();
    let action = sent["action"].as_str().unwrap().to_owned();
    assert!(changed.has_changed().unwrap());

    // The window gets all of it, as the toasts would show it.
    let waiting = commands::waiting(&state);
    assert_eq!(waiting.len(), 1);
    let preview = &waiting[0];
    assert_eq!((preview.action.as_str(), preview.kind), (action.as_str(), "send"));
    assert_eq!(preview.account, "User a (test-echo:a)");
    assert_eq!(preview.conversation, "Echo c1");
    assert_eq!(preview.participants, ["Pat"]);
    assert_eq!(preview.text.as_deref(), Some(text.as_str()));
    assert_eq!(preview.target_text.as_deref(), Some("message m7"));

    assert_eq!(commands::decide(&state, &action, true).await.unwrap(), "done");
    assert!(commands::waiting(&state).is_empty());
    // A second click changes nothing.
    assert_eq!(commands::decide(&state, &action, false).await.unwrap(), "done");

    let rejected = d.registry.call("send", json!({"account": "test-echo:a", "conversation": "c1", "text": "no"})).await.unwrap();
    assert_eq!(commands::decide(&state, rejected["action"].as_str().unwrap(), false).await.unwrap(), "rejected");
    assert!(commands::decide(&state, "nope", true).await.is_err());
}

#[tokio::test]
async fn long_target_can_only_be_rejected() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    // The message replied to is longer than an approval shows.
    let echo = d.accounts.modules().handle("test-echo").unwrap();
    echo.call("a", "set-target", &json!({"conversation": "c1", "text": "x".repeat(28_001)}).to_string()).await.unwrap();
    let sent = d.registry.call("send", json!({"account": "test-echo:a", "conversation": "c1", "text": "ok", "reply_to": "m1"})).await.unwrap();
    let action = sent["action"].as_str().unwrap();
    assert!(commands::waiting(&state)[0].target_cut);

    assert_eq!(commands::decide(&state, action, true).await, Err(commands::TARGET_CUT.to_string()));
    assert_eq!(state.pending.state(action).unwrap(), State::Waiting);
    assert_eq!(commands::decide(&state, action, false).await.unwrap(), "rejected");
}

#[tokio::test]
async fn removing_account_clears_its_waiting_actions() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    d.accounts.add("test-echo", info("b")).unwrap();
    let state = state(&d);
    let (shown, several, withdrawn) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(0)), Arc::new(Mutex::new(0)));
    tokio::spawn(approval::run(
        Arc::clone(&state.pending),
        {
            let shown = Arc::clone(&shown);
            move |preview: &Preview| shown.lock().unwrap().push(preview.action.clone())
        },
        {
            let several = Arc::clone(&several);
            move |_| *several.lock().unwrap() += 1
        },
        {
            let withdrawn = Arc::clone(&withdrawn);
            move || *withdrawn.lock().unwrap() += 1
        },
    ));
    let send = |account: &str| d.registry.call("send", json!({"account": account, "conversation": "c1", "text": "hi"}));
    send("test-echo:a").await.unwrap();
    send("test-echo:a").await.unwrap();
    let b1 = send("test-echo:b").await.unwrap()["action"].as_str().unwrap().to_owned();
    // Three wait: one notice says so.
    wait_for(|| *several.lock().unwrap() == 1).await;
    let withdrawn_before = *withdrawn.lock().unwrap();

    commands::remove_account(&state, "test-echo:a").await.unwrap();
    // Gone from the list, the notice taken off the screen, and the one left shown on its own.
    assert_eq!(commands::waiting(&state).iter().map(|preview| preview.action.clone()).collect::<Vec<_>>(), std::slice::from_ref(&b1));
    wait_for(|| shown.lock().unwrap().last() == Some(&b1)).await;
    assert_eq!(*withdrawn.lock().unwrap(), withdrawn_before + 1);
    assert_eq!(*several.lock().unwrap(), 1);
}

async fn wait_for(done: impl Fn() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out");
}

#[tokio::test]
async fn disabling_stops_a_module_whose_replacement_waits() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    install(d.dir.path(), "echo.wasm", &echo_with_another_host());
    commands::rescan_modules(&state).await.unwrap();
    assert!(d.accounts.modules().handle("test-echo").is_some());

    commands::set_module_enabled(&state, "test-echo", false).await.unwrap();
    assert!(d.accounts.modules().handle("test-echo").is_none());
    assert_eq!(commands::accounts(&state).unwrap()[0].state, "offline");
}

#[tokio::test]
async fn unreadable_file_leaves_the_running_version() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("known")).unwrap();
    let state = state(&d);
    // Read while it is being copied over, or broken.
    install(d.dir.path(), "echo.wasm", b"\0asm half a file");
    commands::rescan_modules(&state).await.unwrap();
    assert!(d.accounts.modules().handle("test-echo").is_some());
    let list = modules(&state);
    assert_eq!(module(&list, "active").name, "test-echo");
    assert_eq!(module(&list, "invalid").file, "echo.wasm");
    assert_eq!(commands::accounts(&state).unwrap()[0].state, "online");
    // So does a file that is gone.
    std::fs::remove_file(d.dir.path().join("modules").join("echo.wasm")).unwrap();
    commands::rescan_modules(&state).await.unwrap();
    assert!(d.accounts.modules().handle("test-echo").is_some());
}

#[tokio::test]
async fn crashed_module_restarts_only_when_asked() {
    let d = daemon(&[("hog.wasm", "open_mess_test_hog")]).await;
    d.accounts.add("test-hog", info("h")).unwrap();
    let state = state(&d);
    // The hog crashes on every call; after a few the host stops it.
    for _ in 0..open_mess_host::limits::MAX_CRASHES {
        let _ = d.registry.call("read_messages", json!({"account": "test-hog:h", "conversation": "c1"})).await;
    }
    assert_eq!(modules(&state)[0].status, "crashed");

    // Refreshing the folder does not bring it back by itself.
    commands::rescan_modules(&state).await.unwrap();
    assert_eq!(modules(&state)[0].status, "crashed");
    // The user does.
    commands::set_module_enabled(&state, "test-hog", true).await.unwrap();
    assert_eq!(modules(&state)[0].status, "active");
}

#[tokio::test]
async fn replacing_a_disabled_module_shows_what_changed() {
    let (dir, state) = unapproved(&[("echo.wasm", echo())]).await;
    let shown = modules(&state)[0].clone();
    commands::approve_module(&state, "test-echo", &shown.file_sha256, &shown.manifest_sha256).await.unwrap();
    commands::set_module_enabled(&state, "test-echo", false).await.unwrap();

    install(dir.path(), "echo.wasm", &echo_with_another_host());
    commands::rescan_modules(&state).await.unwrap();
    let replacement = modules(&state)[0].clone();
    assert_eq!(replacement.detail, "czeka na zatwierdzenie: zmieniony manifest");
    assert_eq!(replacement.changes, ["nowy host: extra.example.com"]);
    assert!(replacement.takes_over);
}

fn rule(id: &str, condition: Condition, action: Action) -> RuleDto {
    RuleDto { id: id.into(), account: "test-echo:a".into(), enabled: true, rule: Rule::new(id, "test-echo:a", condition, action) }
}

fn channel() -> Action {
    Action::Channel { fallback_spawn: None }
}

/// A daemon with test-echo's account "a" (the module is not loaded: rules need only the account).
async fn with_account() -> (Daemon, UiState) {
    let d = daemon(&[]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let state = state(&d);
    (d, state)
}

#[tokio::test]
async fn put_rule_returns_autonomous_warning() {
    let (_d, state) = with_account().await;
    let direct = rule("r1", Condition::Direct, channel());
    assert_eq!(commands::put_rule(&state, direct.clone()).unwrap(), None);
    commands::set_permissions(&state, "test-echo:a", None, true, "autonomous").unwrap();
    assert_eq!(commands::put_rule(&state, direct).unwrap().as_deref(), Some(commands::AUTONOMOUS_RULE));
    // Saved either way, and the window gets it back as it sends it.
    let listed = commands::rules(&state).unwrap();
    assert_eq!(listed.len(), 1);
    let sent = serde_json::to_value(&listed[0].rule).unwrap();
    assert_eq!(
        (sent["id"].as_str(), sent["condition"]["type"].as_str(), sent["action"]["type"].as_str()),
        (Some("r1"), Some("direct"), Some("channel"))
    );
    assert_eq!(serde_json::to_value(serde_json::from_value::<RuleDto>(sent.clone()).unwrap()).unwrap(), sent);

    commands::delete_rule(&state, "r1").unwrap();
    assert!(commands::rules(&state).unwrap().is_empty());
    assert!(commands::delete_rule(&state, "r1").is_err());
    // Only for an account open_mess has.
    let other = RuleDto { account: "test-echo:nobody".into(), ..rule("r2", Condition::Mention, channel()) };
    assert!(commands::put_rule(&state, other).is_err());
}

#[tokio::test]
async fn relative_program_is_rejected_from_the_window() {
    let (_d, state) = with_account().await;
    let spawn =
        |program: &str| Action::Spawn(Spawn { program: program.into(), args: vec!["-p".into(), "check open_mess".into()], working_dir: None });
    assert!(commands::put_rule(&state, rule("r", Condition::Mention, spawn("claude"))).is_err());
    // As the window sends it.
    let sent = json!({
        "id": "r", "account": "test-echo:a", "enabled": true,
        "condition": {"type": "mention"},
        "action": {"type": "spawn", "program": "claude", "args": [], "working_dir": null},
        "quiet_hours": ["22:00:00", "07:00:00"], "max_per_hour": 6, "quiet_ms": 60000, "max_delay_ms": 300000,
    });
    assert!(commands::put_rule(&state, serde_json::from_value(sent).unwrap()).is_err());
    assert!(commands::rules(&state).unwrap().is_empty());
    // The full path of a program that exists is fine.
    let program = std::env::current_exe().unwrap();
    commands::put_rule(&state, rule("r", Condition::Mention, spawn(program.to_str().unwrap()))).unwrap();
}

struct Delivered;

impl Delivery for Delivered {
    async fn deliver(&self, _rule: Rule, _batch: Batch) -> DeliveryResult {
        DeliveryResult::Delivered
    }
}

#[tokio::test(start_paused = true)]
async fn counters_are_listed() {
    let (d, state) = with_account().await;
    commands::put_rule(&state, rule("r1", Condition::Direct, channel())).unwrap();
    let bus = Bus::default();
    let noon = || NaiveTime::from_hms_opt(12, 0, 0).unwrap();
    let _engine = engine::spawn(&bus, Arc::clone(&d.accounts), Arc::new(Delivered), state.counters.clone(), Arc::default(), noon);
    let message = |id: &str| Event::Message {
        account: "test-echo:a".into(),
        conversation: "c1".into(),
        message: id.into(),
        sender: "p1".into(),
        own: false,
        mentions_me: false,
        direct: true,
    };
    bus.publish(message("m1"));
    tokio::time::sleep(Duration::from_secs(61)).await;
    let listed = commands::rules(&state).unwrap();
    assert_eq!((listed[0].counters.delivered, listed[0].counters.failed_to_start), (1, 0));
}

#[tokio::test]
async fn channel_setup_names_the_bridge() {
    let (_d, state) = with_account().await;
    let setup = commands::channel_setup(&state).unwrap();
    let config: Value = serde_json::from_str(&setup.json).unwrap();
    assert_eq!(config["mcpServers"]["open_mess_channel"]["args"], json!(["mcp-stdio"]));
    // The bridge finds this daemon through its data folder, wherever the session starts.
    let data_dir = state.accounts.data_dir().display().to_string();
    assert_eq!(config["mcpServers"]["open_mess_channel"]["env"]["OPEN_MESS_DATA_DIR"], json!(data_dir));
    assert_eq!(std::fs::read_to_string(&setup.file).unwrap(), setup.json);
    assert_eq!(setup.command, format!(r#"claude --mcp-config "{}" --dangerously-load-development-channels server:open_mess_channel"#, setup.file));
    assert_eq!(setup.sessions, 0);
}

/// The window's language is a setting of its own: `auto`, `pl` or `en`, and nothing else.
#[tokio::test]
async fn window_language_is_saved_and_checked() {
    let d = daemon(&[]).await;
    let state = state(&d);
    assert_eq!(commands::language(&state), LanguageDto { setting: "pl", lang: "pl" });
    // Nonsense is refused, in the window's language, and nothing is stored.
    for nonsense in ["de", "PL", "", "auto "] {
        assert_eq!(commands::set_language(&state, nonsense), Err("nieznany język".to_string()), "{nonsense:?}");
    }
    assert_eq!(store(d.dir.path()).get_config("language").unwrap().as_deref(), Some("pl"));
    // What is stored is what the window shows as the setting; saving settles the language again.
    store(d.dir.path()).set_config("language", "auto").unwrap();
    assert_eq!(commands::language(&state).setting, "auto");
    assert_eq!(commands::set_language(&state, "pl"), Ok((Lang::Pl, false)));
    assert_eq!(commands::language(&state), LanguageDto { setting: "pl", lang: "pl" });
    assert_eq!(store(d.dir.path()).get_config("language").unwrap().as_deref(), Some("pl"));
}

/// Every window command in build.rs has its `allow-*` in the capability file and a handler in
/// window.rs, and nothing more: a command left out of either is refused by the window, or never
/// reached (spec 2026-10-27-self-update, 9).
#[test]
fn window_commands_are_registered() {
    let read = |file: &str| std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file)).unwrap();
    let build = read("build.rs");
    let listed = build.split("const COMMANDS: &[&str] = &[").nth(1).unwrap().split("];").next().unwrap();
    let commands: Vec<&str> = listed.split(',').map(|item| item.trim().trim_matches('"')).filter(|item| !item.is_empty()).collect();
    for command in ["update_status", "check_update", "install_update", "cancel_update", "set_auto_install", "language", "set_language"] {
        assert!(commands.contains(&command), "{command} is not in build.rs");
    }
    let capability: Value = serde_json::from_str(&read("capabilities/main.json")).unwrap();
    let mut allowed: Vec<String> = capability["permissions"].as_array().unwrap().iter().map(|item| item.as_str().unwrap().to_owned()).collect();
    let mut expected: Vec<String> = commands.iter().map(|command| format!("allow-{}", command.replace('_', "-"))).collect();
    allowed.sort();
    expected.sort();
    assert_eq!(allowed, expected, "capabilities/main.json grants exactly the commands of build.rs");
    let window = read("src/ui/window.rs");
    let handlers = window.split("generate_handler![").nth(1).unwrap().split(']').next().unwrap();
    let mut handled: Vec<&str> = handlers.split(',').map(|item| item.trim().trim_start_matches("ipc::")).filter(|item| !item.is_empty()).collect();
    let mut commands = commands.clone();
    handled.sort_unstable();
    commands.sort_unstable();
    assert_eq!(handled, commands, "window.rs handles exactly the commands of build.rs");
    for command in &commands {
        assert!(window.contains(&format!("pub async fn {command}(")), "no handler fn {command}");
    }
}
