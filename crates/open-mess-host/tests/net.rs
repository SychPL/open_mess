//! The module network policy in `Net::fetch`, against a local test server.

// clippy.toml allows unwrap in #[test] functions only; the server and list helpers are test code too.
#![allow(clippy::unwrap_used)]

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::routing::get;
use common::load;
use open_mess_host::hosts::HostList;
use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{Engine, HttpError, ModuleState, Net, Request, limits};

/// Serves `/ok`, `/redirect` (302 to its own `/ok`), `/endless` (a body that never ends),
/// `/slow` (answers after 10 s), `/trickle` (a byte every 300 ms, forever), `/throttle/<secs>`
/// (429 with Retry-After) and `/big` (one byte over the general body limit).
async fn server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app =
        Router::new()
            .route("/ok", get(|| async { "ok" }))
            .route("/big", get(|| async { vec![b'x'; limits::MAX_BODY + 1] }))
            .route("/redirect", get(move || async move { (StatusCode::FOUND, [(header::LOCATION, format!("http://{addr}/ok"))]) }))
            .route(
                "/endless",
                get(|| async { Body::from_stream(futures_util::stream::repeat_with(|| Ok::<_, Infallible>(Bytes::from(vec![b'x'; 64 * 1024])))) }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    "late"
                }),
            )
            .route(
                "/trickle",
                get(|| async {
                    Body::from_stream(futures_util::stream::unfold((), |()| async {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Some((Ok::<_, Infallible>(Bytes::from_static(b"x")), ()))
                    }))
                }),
            )
            .route(
                "/throttle/{secs}",
                get(|axum::extract::Path(secs): axum::extract::Path<String>| async move {
                    (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, secs)])
                }),
            );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn hosts(patterns: &[&str]) -> HostList {
    HostList::parse(&patterns.iter().map(|p| p.to_string()).collect::<Vec<_>>()).unwrap()
}

fn get_request(url: String) -> Request {
    Request { method: "GET".to_string(), url, headers: Vec::new(), body: Vec::new() }
}

fn test_net() -> Net {
    Net::with_limits(AddressPolicy::AllowLoopbackForTests, limits::for_tests())
}

#[tokio::test]
async fn allowed_host_is_fetched() {
    let addr = server().await;
    let response = test_net().fetch(&hosts(&["127.0.0.1"]), get_request(format!("http://{addr}/ok"))).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"ok");
}

#[tokio::test]
async fn host_not_on_list_is_blocked() {
    let addr = server().await;
    let result = test_net().fetch(&hosts(&["example.com"]), get_request(format!("http://{addr}/ok"))).await;
    assert!(matches!(result, Err(HttpError::Blocked(_))), "{result:?}");
    let result = test_net().fetch(&hosts(&["127.0.0.1"]), get_request("ftp://127.0.0.1/ok".to_string())).await;
    assert!(matches!(result, Err(HttpError::Blocked(_))), "{result:?}");
}

#[tokio::test]
async fn loopback_is_blocked_by_the_strict_policy() {
    let addr = server().await;
    let result = Net::new(AddressPolicy::Strict).fetch(&hosts(&["127.0.0.1", "localhost"]), get_request(format!("http://{addr}/ok"))).await;
    assert!(matches!(result, Err(HttpError::Blocked(_))), "{result:?}");
    let url = format!("http://localhost:{}/ok", addr.port());
    let result = Net::new(AddressPolicy::Strict).fetch(&hosts(&["localhost"]), get_request(url)).await;
    assert!(matches!(result, Err(HttpError::Blocked(_))), "{result:?}");
}

#[tokio::test]
async fn redirect_is_returned_not_followed() {
    let addr = server().await;
    let response = test_net().fetch(&hosts(&["127.0.0.1"]), get_request(format!("http://{addr}/redirect"))).await.unwrap();
    assert_eq!(response.status, 302);
    let location = response.headers.iter().find(|(name, _)| name == "location").map(|(_, value)| value.clone());
    assert_eq!(location, Some(format!("http://{addr}/ok")));
}

#[tokio::test]
async fn redirect_target_is_checked_on_its_own_fetch() {
    let addr = server().await;
    let list = hosts(&["127.0.0.1"]);
    let response = test_net().fetch(&list, get_request(format!("http://{addr}/redirect"))).await.unwrap();
    let (_, location) = response.headers.into_iter().find(|(name, _)| name == "location").unwrap();
    let result = Net::new(AddressPolicy::Strict).fetch(&list, get_request(location)).await;
    assert!(matches!(result, Err(HttpError::Blocked(_))), "{result:?}");
}

#[tokio::test]
async fn oversized_body_is_cut() {
    let addr = server().await;
    let result = test_net().fetch(&hosts(&["127.0.0.1"]), get_request(format!("http://{addr}/endless"))).await;
    assert!(matches!(result, Err(HttpError::TooLarge)), "{:?}", result.map(|r| r.status));
}

#[tokio::test]
async fn slow_server_times_out() {
    let addr = server().await;
    let started = Instant::now();
    let result = test_net().fetch(&hosts(&["127.0.0.1"]), get_request(format!("http://{addr}/slow"))).await;
    assert!(matches!(result, Err(HttpError::Timeout)), "{:?}", result.map(|r| r.status));
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}

#[tokio::test]
async fn long_retry_after_is_throttled() {
    let addr = server().await;
    let list = hosts(&["127.0.0.1"]);
    let result = test_net().fetch(&list, get_request(format!("http://{addr}/throttle/301"))).await;
    assert!(matches!(result, Err(HttpError::Throttled(301))), "{:?}", result.map(|r| r.status));
    // Up to the maximum the module gets the 429 and decides itself.
    let response = test_net().fetch(&list, get_request(format!("http://{addr}/throttle/300"))).await.unwrap();
    assert_eq!(response.status, 429);
}

#[tokio::test]
async fn module_fetch_follows_its_host_list() {
    let addr = server().await;
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let echo = load(&engine, "open_mess_test_echo").await;
    // test-echo lists only "localhost".
    let url = format!("http://localhost:{}/ok", addr.port());
    assert_eq!(echo.call("a", "fetch", &format!(r#"{{"url":"{url}"}}"#)).await.unwrap(), r#"{"status":200}"#);
    let url = format!("http://{addr}/ok");
    assert_eq!(echo.call("a", "fetch", &format!(r#"{{"url":"{url}"}}"#)).await.unwrap(), r#"{"error":"blocked"}"#);
}

#[tokio::test]
async fn module_fetch_ends_before_the_call_limit() {
    // Each byte comes within the read timeout, so only the call's own limit can end the fetch:
    // it ends as `timeout` before the call does, and the module answers and keeps running.
    let addr = server().await;
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let echo = load(&engine, "open_mess_test_echo").await;
    let url = format!("http://localhost:{}/trickle", addr.port());
    let started = Instant::now();
    assert_eq!(echo.call("a", "fetch", &format!(r#"{{"url":"{url}"}}"#)).await.unwrap(), r#"{"error":"timeout"}"#);
    assert!(started.elapsed() < limits::for_tests().call_timeout, "{:?}", started.elapsed());
    assert_eq!(echo.state(), ModuleState::Running);
}

#[tokio::test]
async fn attachments_have_their_own_size_limit() {
    let addr = server().await;
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let echo = load(&engine, "open_mess_test_echo").await;
    let url = format!("http://localhost:{}/big", addr.port());
    // An ordinary call stops at the general limit...
    let result = echo.call("a", "fetch", &format!(r#"{{"url":"{url}"}}"#)).await.unwrap();
    assert_eq!(result, r#"{"error":"too-large"}"#);
    // ...an attachment download does not, and the next call has the general limit again.
    assert_eq!(echo.get_attachment("a", "c", "m", &url).await.unwrap().data.len(), limits::MAX_BODY + 1);
    assert_eq!(echo.call("a", "fetch", &format!(r#"{{"url":"{url}"}}"#)).await.unwrap(), r#"{"error":"too-large"}"#);
}
