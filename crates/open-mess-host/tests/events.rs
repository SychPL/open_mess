//! WebSocket frames, timers and host events, against a local WebSocket server. The test-echo
//! module logs what its `events` export receives; the tests read those logs.

// clippy.toml allows unwrap in #[test] functions only; the server and log helpers are test code too.
#![allow(clippy::unwrap_used)]

mod common;

use std::net::SocketAddr;
use std::sync::{Mutex, Once};
use std::time::Duration;

use common::{context_with_events, load, wasm};
use futures_util::{SinkExt, StreamExt};
use open_mess_host::{Engine, HostError, HostEvent, ModuleHandle, SessionState, limits};
use tokio_tungstenite::tungstenite::Message;

static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs() {
    static INIT: Once = Once::new();
    INIT.call_once(|| tracing_subscriber::fmt().with_writer(|| Capture).with_max_level(tracing::Level::DEBUG).init());
}

fn logs() -> String {
    String::from_utf8_lossy(&LOGS.lock().unwrap()).into_owned()
}

async fn wait_for_log(needle: &str) -> bool {
    for _ in 0..100 {
        if logs().contains(needle) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[derive(Clone, Copy)]
enum Server {
    /// Answers every text frame with "pong-<port>".
    Pong,
    /// Answers every text frame with one 5 MiB frame.
    Big,
    /// Sends 100 frames as soon as a client connects, then reads.
    Flood,
}

async fn ws_server(kind: Server) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
                if let Server::Flood = kind {
                    for _ in 0..100 {
                        if ws.send(Message::text("flood")).await.is_err() {
                            return;
                        }
                    }
                }
                while let Some(Ok(message)) = ws.next().await {
                    let reply = match kind {
                        Server::Pong => format!("pong-{}", addr.port()),
                        Server::Big => "x".repeat(5 * 1024 * 1024),
                        Server::Flood => continue,
                    };
                    if message.is_text() && ws.send(Message::text(reply)).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

async fn echo() -> ModuleHandle {
    capture_logs();
    load(&Engine::with_limits(limits::for_tests()).unwrap(), "open_mess_test_echo").await
}

async fn open(module: &ModuleHandle, url: String) -> String {
    module.call("a", "ws", &format!(r#"{{"url":"{url}"}}"#)).await.unwrap()
}

#[tokio::test]
async fn ws_frames_reach_the_module() {
    let addr = ws_server(Server::Pong).await;
    let module = echo().await;
    assert_eq!(open(&module, format!("ws://localhost:{}", addr.port())).await, r#"{"conn":1}"#);
    let line = format!("ws 1 message pong-{}", addr.port());
    assert!(wait_for_log(&line).await, "no {line:?} in:\n{}", logs());
}

#[tokio::test]
async fn ws_host_not_on_list_is_blocked() {
    let addr = ws_server(Server::Pong).await;
    let module = echo().await;
    // test-echo lists only "localhost".
    assert_eq!(open(&module, format!("ws://{addr}")).await, r#"{"error":"blocked"}"#);
    assert_eq!(open(&module, format!("http://localhost:{}", addr.port())).await, r#"{"error":"blocked"}"#);
}

#[tokio::test]
async fn fifth_socket_is_refused() {
    let addr = ws_server(Server::Pong).await;
    let module = echo().await;
    let url = format!("ws://localhost:{}", addr.port());
    for conn in 1..=limits::MAX_SOCKETS {
        assert_eq!(open(&module, url.clone()).await, format!(r#"{{"conn":{conn}}}"#));
    }
    assert_eq!(open(&module, url).await, r#"{"error":"blocked"}"#);
}

#[tokio::test]
async fn oversized_frame_closes_the_socket() {
    let addr = ws_server(Server::Big).await;
    let module = echo().await;
    assert_eq!(open(&module, format!("ws://localhost:{}", addr.port())).await, r#"{"conn":1}"#);
    assert!(wait_for_log("ws 1 closed message too large").await, "{}", logs());
}

#[tokio::test]
async fn a_busy_module_still_sends_when_its_events_pile_up() {
    // The server's 100 frames fill the module's event queue while it is busy; its 40 frames must
    // still go out instead of waiting for a socket that waits for the module.
    let addr = ws_server(Server::Flood).await;
    let module = echo().await;
    let url = format!("ws://localhost:{}", addr.port());
    assert_eq!(module.call("a", "ws-burst", &format!(r#"{{"url":"{url}"}}"#)).await.unwrap(), r#"{"sent":40}"#);
}

#[tokio::test]
async fn a_crash_closes_the_instances_sockets() {
    let addr = ws_server(Server::Pong).await;
    let module = echo().await;
    let url = format!("ws://localhost:{}", addr.port());
    for conn in 1..=limits::MAX_SOCKETS {
        assert_eq!(open(&module, url.clone()).await, format!(r#"{{"conn":{conn}}}"#));
    }
    assert!(matches!(module.call("a", "trap", "{}").await, Err(HostError::Crashed(_))));
    // A fresh instance: its sockets start over and the old ones no longer count.
    for conn in 1..=limits::MAX_SOCKETS {
        assert_eq!(open(&module, url.clone()).await, format!(r#"{{"conn":{conn}}}"#));
    }
}

#[tokio::test]
async fn timer_fires_once() {
    let module = echo().await;
    assert_eq!(module.call("a", "timer", "{}").await.unwrap(), "{}");
    assert!(wait_for_log("timer 7").await, "{}", logs());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(logs().matches("timer 7").count(), 1, "{}", logs());
}

#[tokio::test]
async fn message_arrived_becomes_host_event() {
    let (context, mut events) = context_with_events();
    let engine = Engine::with_limits(limits::for_tests()).unwrap();
    let module = engine.load(&wasm("open_mess_test_echo"), context).await.unwrap();
    assert_eq!(module.call("a", "message", r#"{"text":"hi"}"#).await.unwrap(), "{}");
    let session = HostEvent::Session { module: "test-echo".to_string(), account: "a".to_string(), state: SessionState::Online };
    assert_eq!(events.recv().await, Some(session));
    let Some(HostEvent::MessageArrived(message)) = events.recv().await else { panic!("no message event") };
    assert_eq!((message.module.as_str(), message.account.as_str(), message.conversation.as_str()), ("test-echo", "a", "c"));
    assert_eq!(message.message, "hi");
    assert!(message.mentions_me && !message.own && !message.direct);
    // An event larger than the cap is dropped: only the session event comes through.
    let huge = "x".repeat(limits::MAX_EVENT_TEXT + 1);
    assert_eq!(module.call("a", "message", &format!(r#"{{"text":"{huge}"}}"#)).await.unwrap(), "{}");
    assert!(matches!(events.recv().await, Some(HostEvent::Session { .. })));
    assert!(events.try_recv().is_err());
}
