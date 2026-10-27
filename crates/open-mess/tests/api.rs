//! The local API: bearer token, the `/api` routes for the CLI and MCP on `/mcp`.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use open_mess::tools::Registry;
use reqwest::StatusCode;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use tokio::sync::oneshot;

const TOKEN: &str = "t0ken";

/// Serves the API on an ephemeral loopback port until the returned sender is dropped.
async fn start() -> (SocketAddr, oneshot::Sender<()>) {
    // The test clients use reqwest, built without a crypto provider in this workspace.
    rustls::crypto::ring::default_provider().install_default().ok();
    let listener = open_mess::api::bind(0).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let registry = Arc::new(Registry::with_builtin(Instant::now()));
    tokio::spawn(open_mess::api::serve(listener, TOKEN.into(), registry, async {
        let _ = stopped.await;
    }));
    (addr, stop)
}

#[tokio::test]
async fn missing_token_is_401() {
    let (addr, _stop) = start().await;
    let response = reqwest::get(format!("http://{addr}/api/tools")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = reqwest::Client::new().post(format!("http://{addr}/api/call/status")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_token_is_401() {
    let (addr, _stop) = start().await;
    let client = reqwest::Client::new();
    let response = client.get(format!("http://{addr}/api/tools")).bearer_auth("nope").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // A prefix of the right token is still wrong.
    let response = client.get(format!("http://{addr}/api/tools")).bearer_auth("t0k").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}
    });
    let response = client
        .post(format!("http://{addr}/mcp"))
        .bearer_auth("nope")
        .header("accept", "application/json, text/event-stream")
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn tools_are_listed() {
    let (addr, _stop) = start().await;
    let response = reqwest::Client::new().get(format!("http://{addr}/api/tools")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let tools: Value = response.json().await.unwrap();
    let status = tools.as_array().unwrap().iter().find(|tool| tool["name"] == "status").unwrap();
    assert_eq!(status["input_schema"]["type"], "object");
}

#[tokio::test]
async fn status_over_api() {
    let (addr, _stop) = start().await;
    let response = reqwest::Client::new().post(format!("http://{addr}/api/call/status")).bearer_auth(TOKEN).json(&json!({})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body["uptime_seconds"].is_u64());
    assert_eq!(body["modules"], json!([]));
    assert_eq!(body["accounts"], json!([]));
    assert_eq!(body["triggers"], json!({"channel_sessions": 0, "counters": {"lagged": 0, "rules": {}}}));
}

#[tokio::test]
async fn unknown_tool_is_404() {
    let (addr, _stop) = start().await;
    let response = reqwest::Client::new().post(format!("http://{addr}/api/call/nope")).bearer_auth(TOKEN).json(&json!({})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("nope"));
}

#[tokio::test]
async fn invalid_args_are_400() {
    let (addr, _stop) = start().await;
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/api/call/status");
    let response = client.post(&url).bearer_auth(TOKEN).json(&json!({"verbose": true})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = client.post(&url).bearer_auth(TOKEN).body("{not json").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn status_over_mcp() {
    let (addr, _stop) = start().await;
    let api: Value = reqwest::Client::new()
        .post(format!("http://{addr}/api/call/status"))
        .bearer_auth(TOKEN)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let transport =
        StreamableHttpClientTransport::from_config(StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")).auth_header(TOKEN));
    let client = ClientConfig::default().serve(transport).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();
    assert!(tools.iter().any(|tool| tool.name == "status"));
    let result = client.call_tool(CallToolRequestParams::new("status")).await.unwrap();
    assert_ne!(result.is_error, Some(true));
    let mcp = result.structured_content.unwrap();
    let keys = |value: &Value| value.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&mcp), keys(&api));
    assert_eq!(mcp["version"], api["version"]);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn port_in_use_is_an_error() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let err = open_mess::api::bind(port).await.unwrap_err();
    assert!(err.to_string().contains(&port.to_string()));
    assert!(matches!(err, open_mess::Error::PortInUse(p) if p == port));
}

#[tokio::test]
async fn listens_on_loopback_only() {
    let listener = open_mess::api::bind(0).await.unwrap();
    assert!(listener.local_addr().unwrap().ip().is_loopback());
}
