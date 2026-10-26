//! Module WebSockets (spec 8.3). The core holds the connections; each incoming text frame and the
//! close reach the module through its `events` export, one short call each.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, header};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::actor::{GuestEvent, Task};
use crate::hosts::HostList;
use crate::limits;
use crate::net::{HttpError, Net, blocked};

type Stream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Frames the module sent that the connection has not written yet.
const SEND_QUEUE: usize = 16;

fn network(e: impl ToString) -> HttpError {
    HttpError::Network(e.to_string())
}

/// Connects to a checked address of a host on the module's list, with the system trust store.
/// ponytail: direct connections only; WebSockets through the system proxy (HTTP CONNECT) when a
/// network that forces the proxy needs them.
async fn connect(net: &Net, hosts: &HostList, url: &str, headers: &[(String, String)]) -> Result<Stream, HttpError> {
    let url = url::Url::parse(url).map_err(|_| blocked("not a valid URL"))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(blocked("only ws and wss URLs"));
    }
    let addrs = net.resolve(hosts, &url).await?;
    let mut request = url.as_str().into_client_request().map_err(network)?;
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| network("invalid header name"))?;
        if name == header::HOST {
            return Err(blocked("the Host header comes from the URL"));
        }
        request.headers_mut().append(name, HeaderValue::from_str(value).map_err(|_| network("invalid header value"))?);
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .and_then(|builder| builder.with_platform_verifier())
        .map_err(|e| network(format!("TLS setup: {e}")))?
        .with_no_client_auth();
    let config = WebSocketConfig::default().max_message_size(Some(limits::MAX_WS_MESSAGE)).max_frame_size(Some(limits::MAX_WS_MESSAGE));
    if addrs.is_empty() {
        return Err(network("no address"));
    }
    let handshake = async {
        // All checked addresses at once, the first to answer wins: trying them in turn would wait
        // out an unreachable IPv6 address before IPv4 (localhost on Windows takes seconds).
        let attempts = addrs.iter().map(|addr| Box::pin(TcpStream::connect(*addr)));
        let (stream, _) = futures_util::future::select_ok(attempts).await.map_err(network)?;
        let connector = Connector::Rustls(Arc::new(tls));
        let (ws, _) = tokio_tungstenite::client_async_tls_with_config(request, stream, Some(config), Some(connector)).await.map_err(network)?;
        Ok(ws)
    };
    tokio::time::timeout(net.limits.connect_timeout, handshake).await.map_err(|_| HttpError::Timeout)?
}

/// Moves frames between one connection and the module until either side ends it.
async fn serve(conn: u32, mut ws: Stream, mut outgoing: mpsc::Receiver<String>, events: mpsc::Sender<GuestEvent>) {
    let reason = loop {
        tokio::select! {
            frame = ws.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    if events.send(GuestEvent::WsMessage { conn, text: text.as_str().to_owned() }).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Close(frame))) => break frame.map(|f| f.reason.as_str().to_owned()).unwrap_or_default(),
                // The contract carries text only; tungstenite answers pings itself.
                Some(Ok(_)) => {}
                Some(Err(tungstenite::Error::Capacity(_))) => break "message too large".to_string(),
                Some(Err(e)) => break e.to_string(),
                None => break "connection closed".to_string(),
            },
            text = outgoing.recv() => match text {
                Some(text) => {
                    if let Err(e) = ws.send(Message::text(text)).await {
                        break e.to_string();
                    }
                }
                None => return,
            },
        }
    };
    events.send(GuestEvent::WsClosed { conn, reason }).await.ok();
}

struct Socket {
    outgoing: mpsc::Sender<String>,
    _task: Task,
}

/// One module instance's open WebSockets; dropping it closes them.
#[derive(Default)]
pub(crate) struct Sockets {
    open: HashMap<u32, Socket>,
    last: u32,
}

impl Sockets {
    pub(crate) async fn open(
        &mut self,
        net: &Net,
        hosts: &HostList,
        url: &str,
        headers: &[(String, String)],
        events: &mpsc::Sender<GuestEvent>,
    ) -> Result<u32, HttpError> {
        if self.open.len() >= limits::MAX_SOCKETS {
            return Err(blocked(format!("at most {} open WebSockets", limits::MAX_SOCKETS)));
        }
        let ws = connect(net, hosts, url, headers).await?;
        self.last = self.last.wrapping_add(1);
        let conn = self.last;
        let (outgoing, queue) = mpsc::channel(SEND_QUEUE);
        let task = Task(tokio::spawn(serve(conn, ws, queue, events.clone())));
        self.open.insert(conn, Socket { outgoing, _task: task });
        Ok(conn)
    }

    pub(crate) async fn send(&mut self, conn: u32, text: String) -> Result<(), HttpError> {
        if text.len() > limits::MAX_WS_MESSAGE {
            return Err(HttpError::TooLarge);
        }
        let socket = self.open.get(&conn).ok_or_else(|| network("no such WebSocket"))?;
        socket.outgoing.send(text).await.map_err(|_| network("the WebSocket is closed"))
    }

    /// Drops a connection: one the module closes, or one the server has closed.
    pub(crate) fn close(&mut self, conn: u32) {
        self.open.remove(&conn);
    }
}
