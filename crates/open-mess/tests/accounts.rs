//! Accounts bound to modules: routing of the shared tools, module states, fan-out and removal.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{ECHO, daemon, info};
use open_mess::modules;
use open_mess::permissions::{self, Write};
use open_mess::tools::shared::{UNTRUSTED_NOTICE, attachments_dir};
use open_mess::tools::{Registry, ToolError};
use open_mess::triggers::rules::{self, Action, Condition, Rule};
use open_mess_host::{Engine, HostEvent, SessionState, limits};
use serde_json::{Value, json};

async fn read_messages(registry: &Registry, account: &str) -> Result<Value, ToolError> {
    registry.call("read_messages", json!({"account": account, "conversation": "c1"})).await
}

/// Downloads test-echo's attachment for `account` and returns where it was written.
async fn attachment(registry: &Registry, account: &str) -> std::path::PathBuf {
    let args = json!({"account": account, "conversation": "c1", "message": "m1", "attachment": "notes.txt"});
    std::path::PathBuf::from(registry.call("get_attachment", args).await.unwrap()["path"].as_str().unwrap())
}

#[tokio::test]
async fn routes_to_the_accounts_module() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    d.accounts.add("test-echo", info("b")).unwrap();

    let result = read_messages(&d.registry, "test-echo:b").await.unwrap();
    assert_eq!(result["items"][0]["account"], "test-echo:b");
    assert_eq!(result["items"][0]["conversation"], "c1");
    // The module got its own id for the account.
    assert_eq!(result["items"][0]["untrusted"]["text"], "hello b");
    assert_eq!(result["untrusted_notice"], UNTRUSTED_NOTICE);

    let accounts = d.registry.call("accounts", json!({})).await.unwrap();
    assert_eq!(
        accounts,
        json!({"items": [
            {"id": "test-echo:a", "module": "test-echo", "platform": "test", "display_name": "User a", "state": "online"},
            {"id": "test-echo:b", "module": "test-echo", "platform": "test", "display_name": "User b", "state": "online"},
        ]})
    );
    let status = d.registry.call("status", json!({})).await.unwrap();
    assert_eq!(
        status["accounts"],
        json!([{"id": "test-echo:a", "module": "test-echo", "state": "online"}, {"id": "test-echo:b", "module": "test-echo", "state": "online"}])
    );

    let attachment = d
        .registry
        .call("get_attachment", json!({"account": "test-echo:a", "conversation": "c1", "message": "m1", "attachment": "notes.txt"}))
        .await
        .unwrap();
    assert_eq!(attachment["path"], attachments_dir(d.dir.path(), "test-echo:a").join("notes.txt").display().to_string());
    assert_eq!(attachment["untrusted"]["file_name"], "notes.txt");
}

#[tokio::test]
async fn unknown_account_is_an_error() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    for account in ["test-echo:nobody", "a", ""] {
        let result = read_messages(&d.registry, account).await;
        assert!(matches!(&result, Err(ToolError::InvalidArgs(why)) if why.contains("unknown account")), "{account}: {result:?}");
    }
    let result = d.registry.call("unread", json!({"account": "teams:a"})).await;
    assert!(matches!(&result, Err(ToolError::InvalidArgs(why)) if why.contains("unknown account")), "{result:?}");
}

#[tokio::test]
async fn disabled_module_is_reported() {
    let d = daemon(&[ECHO, ("hog.wasm", "open_mess_test_hog")]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    d.accounts.add("test-hog", info("h")).unwrap();

    // The hog crashes on every call; after three the host stops it.
    for _ in 0..limits::MAX_CRASHES {
        let result = read_messages(&d.registry, "test-hog:h").await;
        assert!(matches!(&result, Err(ToolError::Module(why)) if why.contains("crashed")), "{result:?}");
    }
    let result = read_messages(&d.registry, "test-hog:h").await;
    assert!(matches!(&result, Err(ToolError::Module(why)) if why == "module test-hog is stopped after repeated crashes"), "{result:?}");

    // The other module's account works, alone and in a fan-out.
    assert_eq!(read_messages(&d.registry, "test-echo:a").await.unwrap()["items"][0]["untrusted"]["text"], "hello a");
    let unread = d.registry.call("unread", json!({})).await.unwrap();
    assert_eq!(unread["items"].as_array().unwrap().len(), 1);
    assert_eq!(unread["items"][0]["account"], "test-echo:a");
    assert_eq!(unread["errors"], json!([{"account": "test-hog:h", "error": "module test-hog is stopped after repeated crashes"}]));
    let accounts = d.registry.call("accounts", json!({})).await.unwrap();
    assert_eq!(accounts["items"][1]["state"], "offline");

    // A module the user turned off.
    let store = common::store(d.dir.path());
    modules::set_enabled(&store, "test-echo", false).unwrap();
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let modules = common::scan(&store, &engine, d.dir.path()).await;
    let registry = Registry::new(Instant::now(), common::accounts(d.dir.path(), modules, Arc::clone(&d.credentials)));
    let result = read_messages(&registry, "test-echo:a").await;
    assert!(matches!(&result, Err(ToolError::Module(why)) if why == "module test-echo is disabled"), "{result:?}");
}

#[tokio::test]
async fn fan_out_survives_one_slow_account() {
    let d = daemon(&[ECHO, ("spin.wasm", "open_mess_test_spin")]).await;
    d.accounts.add("test-echo", info("a1")).unwrap();
    d.accounts.add("test-echo", info("a2")).unwrap();
    d.accounts.add("test-spin", info("s")).unwrap();

    let started = Instant::now();
    let unread = d.registry.call("unread", json!({})).await.unwrap();
    assert!(started.elapsed() < limits::for_tests().call_timeout + Duration::from_secs(1), "{:?}", started.elapsed());
    let accounts: Vec<&Value> = unread["items"].as_array().unwrap().iter().map(|item| &item["account"]).collect();
    assert_eq!(accounts, ["test-echo:a1", "test-echo:a2"]);
    assert_eq!(unread["items"][1]["untrusted"]["title"], "Echo a2");
    let errors = unread["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["account"], "test-spin:s");
    assert!(errors[0]["error"].as_str().unwrap().contains("timed out"), "{errors:?}");
    assert_eq!(unread["untrusted_notice"], UNTRUSTED_NOTICE);
}

#[tokio::test]
async fn remove_account_deletes_secrets_and_signs_out() {
    let mut d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    d.accounts.add("test-echo", info("b")).unwrap();
    // Secrets as the module would have stored them: the credentials, and their names in the
    // module's storage.
    let secret = |account: &str| d.credentials.build("open_mess/module/test-echo", &format!("{account}/refresh"), None).unwrap();
    let names = rusqlite::Connection::open(d.dir.path().join("modules").join("test-echo.sqlite3")).unwrap();
    names.execute_batch("CREATE TABLE IF NOT EXISTS secrets (account TEXT NOT NULL, key TEXT NOT NULL, PRIMARY KEY (account, key))").unwrap();
    for account in ["a", "b"] {
        secret(account).set_password("token").unwrap();
        names.execute("INSERT INTO secrets (account, key) VALUES (?1, 'refresh')", [account]).unwrap();
    }

    let (attachment_a, attachment_b) = (attachment(&d.registry, "test-echo:a").await, attachment(&d.registry, "test-echo:b").await);
    let store = common::store(d.dir.path());
    permissions::set(&store, "test-echo:a", Some("c1"), true, Write::Autonomous).unwrap();
    let rule = |id: &str, account: &str| Rule::new(id, account, Condition::Direct, Action::Channel { fallback_spawn: None });
    rules::put(&store, &rule("ra", "test-echo:a")).unwrap();
    rules::put(&store, &rule("rb", "test-echo:b")).unwrap();

    d.accounts.remove("test-echo:a").await.unwrap();
    // The same account signed in again starts from the defaults (spec 8.6), without triggers.
    assert_eq!(permissions::effective(&store, "test-echo:a", Some("c1")).unwrap(), permissions::DEFAULT);
    assert_eq!(rules::list(&store).unwrap(), [rule("rb", "test-echo:b")]);

    // test-echo reports an account it signs out as offline.
    let event = tokio::time::timeout(Duration::from_secs(5), d.events.recv()).await.unwrap().unwrap();
    assert_eq!(event, HostEvent::Session { module: "test-echo".into(), account: "a".into(), state: SessionState::Offline });
    assert!(matches!(secret("a").get_password(), Err(keyring_core::Error::NoEntry)));
    assert_eq!(secret("b").get_password().unwrap(), "token");
    let ids: Vec<String> = d.accounts.list().unwrap().into_iter().map(|account| account.id).collect();
    assert_eq!(ids, ["test-echo:b"]);
    assert!(!attachment_a.exists() && attachment_b.exists());
    assert!(d.accounts.remove("test-echo:a").await.is_err());
}

#[tokio::test]
async fn restore_sets_account_states() {
    let d = daemon(&[ECHO]).await;
    // test-echo restores only account "known".
    for id in ["known", "other"] {
        d.accounts.add("test-echo", info(id)).unwrap();
    }
    d.accounts.add("gone", info("x")).unwrap();
    d.accounts.restore().await.unwrap();
    let states: Vec<(String, String)> = d.accounts.list().unwrap().into_iter().map(|account| (account.id, account.state)).collect();
    let expected = [("gone:x", "offline"), ("test-echo:known", "online"), ("test-echo:other", "auth-required")];
    assert_eq!(states, expected.map(|(id, state)| (id.to_string(), state.to_string())));
}

#[tokio::test]
async fn module_errors_over_the_api() {
    let d = daemon(&[ECHO]).await;
    // An account whose module is gone.
    d.accounts.add("gone", info("x")).unwrap();
    rustls::crypto::ring::default_provider().install_default().ok();
    let listener = open_mess::api::bind(0).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (_stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(open_mess::api::serve(listener, "t0ken".into(), Arc::new(d.registry), open_mess::ui::Opener::default(), async {
        let _ = stopped.await;
    }));

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/api/call/read_messages"))
        .bearer_auth("t0ken")
        .json(&json!({"account": "gone:x", "conversation": "c1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], "module gone is not installed");

    use rmcp::ServiceExt;
    let transport = rmcp::transport::StreamableHttpClientTransport::from_config(
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")).auth_header("t0ken"),
    );
    let client = rmcp::model::ClientConfig::default().serve(transport).await.unwrap();
    let args = json!({"account": "gone:x", "conversation": "c1"}).as_object().unwrap().clone();
    let result = client.call_tool(rmcp::model::CallToolRequestParams::new("read_messages").with_arguments(args)).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn secrets_left_behind_keep_the_account() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let names = rusqlite::Connection::open(d.dir.path().join("modules").join("test-echo.sqlite3")).unwrap();
    names.execute_batch("CREATE TABLE IF NOT EXISTS secrets (account TEXT NOT NULL, key TEXT NOT NULL, PRIMARY KEY (account, key))").unwrap();
    names.execute("INSERT INTO secrets (account, key) VALUES ('a', 'refresh')", []).unwrap();
    let entry = d.credentials.build("open_mess/module/test-echo", "a/refresh", None).unwrap();
    entry.set_password("token").unwrap();
    // The credential store refuses the delete, as a locked Credential Manager would.
    let cred: &keyring_core::mock::Cred = entry.as_any().downcast_ref().unwrap();
    cred.set_error(keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("locked"))));

    assert!(d.accounts.remove("test-echo:a").await.is_err());
    // The account stays, signed out.
    assert_eq!(d.accounts.get("test-echo:a").unwrap().unwrap().state, "offline");
    // Removing again works once the store does.
    d.accounts.remove("test-echo:a").await.unwrap();
    assert!(d.accounts.get("test-echo:a").unwrap().is_none());
}

#[tokio::test]
async fn interrupted_removal_can_be_run_again() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    d.accounts.add("test-echo", info("b")).unwrap();
    attachment(&d.registry, "test-echo:a").await;
    // The daemon stopped in the middle of removing "a": its row is marked.
    let state = rusqlite::Connection::open(d.dir.path().join("state.sqlite3")).unwrap();
    state.execute("UPDATE accounts SET state = 'removing' WHERE id = 'test-echo:a'", []).unwrap();
    assert!(read_messages(&d.registry, "test-echo:a").await.is_err());
    // The fan-out skips it without an error.
    let unread = d.registry.call("unread", json!({})).await.unwrap();
    assert_eq!(unread["items"].as_array().unwrap().len(), 1);
    assert_eq!(unread["items"][0]["account"], "test-echo:b");
    assert!(unread.get("errors").is_none(), "{unread}");
    // A module report doesn't bring it back, and a download that ends now saves nothing.
    d.accounts.set_state("test-echo", "a", SessionState::Online).unwrap();
    assert_eq!(d.accounts.get("test-echo:a").unwrap().unwrap().state, "removing");
    let late = open_mess_host::Attachment { file_name: "late.txt".into(), content_type: "text/plain".into(), data: b"late".to_vec() };
    assert!(d.accounts.save_attachment("test-echo:a", late).is_err());
    assert!(!attachments_dir(d.dir.path(), "test-echo:a").join("late.txt").exists());

    d.accounts.remove("test-echo:a").await.unwrap();
    assert!(d.accounts.get("test-echo:a").unwrap().is_none());
    assert!(!attachments_dir(d.dir.path(), "test-echo:a").exists());
}
