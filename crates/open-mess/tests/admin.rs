//! The administrative API of a server daemon (spec 2026-11-03-paperclip-plugin, 5.4): the window's
//! commands over HTTP, behind a token of their own.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Daemon, ECHO, daemon};
use open_mess::api::{self, Admin};
use open_mess::ui::{Opener, UiState};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::oneshot;

const AGENTS: &str = "agents-t0ken";
const ADMIN: &str = "admin-t0ken";

fn state(d: &Daemon) -> UiState {
    UiState {
        accounts: Arc::clone(&d.accounts),
        pending: Arc::clone(d.registry.pending().unwrap()),
        started: Instant::now(),
        port: 7341,
        token: AGENTS.into(),
        sign_ins: d.sign_ins.clone(),
        counters: d.registry.trigger_counters().clone(),
        sessions: d.registry.channel_sessions().clone(),
        gate: d.registry.gate().clone(),
        activity: Arc::default(),
        runs: open_mess::triggers::spawn::Runs::new(d.dir.path().join("runs"), open_mess::triggers::spawn::RUN_TIMEOUT, Default::default()),
        host_events: tokio::sync::mpsc::channel(1).0,
        ui_ready: Arc::default(),
        exit: Arc::default(),
        probation: d.registry.pending().unwrap().probation().clone(),
        update: None,
        models: None,
        recorder: None,
    }
}

/// Serves test-echo's daemon, with the administrative API when `admin`, until the sender drops.
async fn serve(admin: bool) -> (SocketAddr, oneshot::Sender<()>, Daemon) {
    // The test clients use reqwest, built without a crypto provider in this workspace.
    rustls::crypto::ring::default_provider().install_default().ok();
    let d = daemon(&[ECHO]).await;
    let admin = admin.then(|| Admin { token: ADMIN.into(), ui: Arc::new(state(&d)) });
    let listener = api::bind(0).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let registry = Arc::new(open_mess::tools::Registry::new(Instant::now(), Arc::clone(&d.accounts)));
    tokio::spawn(api::serve_with_admin(listener, AGENTS.into(), admin, registry, Opener::default(), async {
        let _ = stopped.await;
    }));
    (addr, stop, d)
}

async fn post(addr: SocketAddr, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = reqwest::Client::new().post(format!("http://{addr}{path}")).bearer_auth(token).json(&body).send().await.unwrap();
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn each_token_opens_only_its_own_routes() {
    let (addr, _stop, _d) = serve(true).await;
    let (status, body) = post(addr, ADMIN, "/admin/status", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("version").is_some(), "{body}");
    // The agents' token is not the administrator's, and the other way round.
    assert_eq!(post(addr, AGENTS, "/admin/status", json!({})).await.0, StatusCode::UNAUTHORIZED);
    let client = reqwest::Client::new();
    let tools = client.get(format!("http://{addr}/api/tools")).bearer_auth(ADMIN).send().await.unwrap();
    assert_eq!(tools.status(), StatusCode::UNAUTHORIZED);
    let mcp = client
        .post(format!("http://{addr}/mcp"))
        .bearer_auth(ADMIN)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(mcp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(client.get(format!("http://{addr}/api/tools")).bearer_auth(AGENTS).send().await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_commands_and_bad_arguments_are_refused() {
    let (addr, _stop, _d) = serve(true).await;
    assert_eq!(post(addr, ADMIN, "/admin/install_everything", json!({})).await.0, StatusCode::NOT_FOUND);
    let (status, body) = post(addr, ADMIN, "/admin/remove_account", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("invalid arguments"), "{body}");
    let response = reqwest::Client::new().post(format!("http://{addr}/admin/accounts")).bearer_auth(ADMIN).body("{not json").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // A window command's own refusal comes back as the error.
    let (status, body) = post(addr, ADMIN, "/admin/permissions", json!({"account": "nobody"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn without_the_administrative_api_admin_paths_are_404() {
    let (addr, _stop, _d) = serve(false).await;
    for token in [ADMIN, AGENTS, "nope"] {
        assert_eq!(post(addr, token, "/admin/status", json!({})).await.0, StatusCode::NOT_FOUND);
    }
}

/// Waits for the sign-in prompt of `kind`.
async fn prompt(addr: SocketAddr, kind: &str) -> Value {
    for _ in 0..1500 {
        let (_, prompt) = post(addr, ADMIN, "/admin/sign_in_request", json!({})).await;
        if prompt["kind"] == kind {
            return prompt;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no {kind} prompt");
}

/// Waits until the sign-in started last has ended.
async fn result(addr: SocketAddr) -> Value {
    for _ in 0..1500 {
        let (_, result) = post(addr, ADMIN, "/admin/sign_in_result", json!({})).await;
        if result["state"] != "running" {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the sign-in never ended");
}

#[tokio::test]
async fn a_sign_in_starts_at_once_and_reports_its_result() {
    let (addr, _stop, _d) = serve(true).await;
    assert_eq!(post(addr, ADMIN, "/admin/sign_in_result", json!({})).await.1["state"], "idle");
    let (status, body) = post(addr, ADMIN, "/admin/sign_in_start", json!({"module": "test-echo"})).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"started": true})));
    // A second sign-in meanwhile is refused at once, and the first one keeps its state.
    assert_eq!(post(addr, ADMIN, "/admin/sign_in_start", json!({"module": "test-echo"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(post(addr, ADMIN, "/admin/sign_in_result", json!({})).await.1["state"], "running");

    let page = prompt(addr, "page").await;
    assert_eq!(page["open_url"], "https://login.example.com/start");
    let id = page["id"].clone();
    // An address off the redirect prefix is not the answer; the prompt still waits.
    let wrong = json!({"id": id, "answer": {"done": "https://evil.example/done?account=x"}});
    assert_eq!(post(addr, ADMIN, "/admin/sign_in_answer", wrong).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(prompt(addr, "page").await["id"], id);

    let pasted = json!({"id": id, "answer": {"done": " https://login.example.com/done?account=a1\n"}});
    let (status, _) = post(addr, ADMIN, "/admin/sign_in_answer", pasted).await;
    if cfg!(windows) {
        // On Windows only the sign-in window answers a page.
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(post(addr, ADMIN, "/admin/sign_in_answer", json!({"id": id, "answer": "cancelled"})).await.0, StatusCode::OK);
        assert_eq!(result(addr).await["state"], "failed");
    } else {
        assert_eq!(status, StatusCode::OK);
        let result = result(addr).await;
        assert_eq!((result["state"].as_str(), result["account"]["id"].as_str()), (Some("done"), Some("test-echo:a1")), "{result}");
    }
}
