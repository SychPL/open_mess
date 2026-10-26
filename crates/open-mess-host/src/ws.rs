//! Module WebSockets (spec 8.3). The core holds the connections; each incoming text frame and the
//! close reach the module through its `events` export, one short call each.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use hyper_util::client::proxy::matcher::Matcher;
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, Uri, header};
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
/// The status line and headers of a proxy's answer to CONNECT.
const MAX_PROXY_HEAD: usize = 8 * 1024;

fn network(e: impl ToString) -> HttpError {
    HttpError::Network(e.to_string())
}

/// A TCP connection to one of the checked addresses; all are tried at once and the first to
/// answer wins: trying them in turn would wait out an unreachable IPv6 address before IPv4
/// (localhost on Windows takes seconds).
async fn direct(addrs: &[SocketAddr]) -> Result<TcpStream, HttpError> {
    if addrs.is_empty() {
        return Err(network("no address"));
    }
    let attempts = addrs.iter().map(|addr| Box::pin(TcpStream::connect(*addr)));
    let (stream, _) = futures_util::future::select_ok(attempts).await.map_err(network)?;
    Ok(stream)
}

/// An HTTP CONNECT tunnel through the http:// `proxy` to `host:port`. The proxy's own address is
/// set by the system or the organisation and is not checked (spec 8.3, R5).
async fn tunnel(proxy: &Uri, auth: Option<&HeaderValue>, host: &str, port: u16) -> Result<TcpStream, HttpError> {
    let proxy_host = proxy.host().ok_or_else(|| network("the proxy has no host"))?.trim_matches(['[', ']']);
    let mut stream = TcpStream::connect((proxy_host, proxy.port_u16().unwrap_or(80))).await.map_err(network)?;
    let mut request = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(auth) = auth {
        request.push_str(&format!("Proxy-Authorization: {}\r\n", auth.to_str().map_err(network)?));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.map_err(network)?;
    // The answer's head, a byte at a time so nothing of the tunnel is read past it.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_PROXY_HEAD {
            return Err(network("the proxy's answer is too long"));
        }
        head.push(stream.read_u8().await.map_err(network)?);
    }
    if !(head.starts_with(b"HTTP/1.1 200") || head.starts_with(b"HTTP/1.0 200")) {
        let status = head.split(|&b| b == b'\r').next().unwrap_or_default();
        return Err(network(format!("the proxy refused: {}", String::from_utf8_lossy(status))));
    }
    Ok(stream)
}

/// Connects the way the system proxy settings say: through the proxy (HTTP CONNECT) when they
/// send this host through an http:// one, else directly to a checked address.
/// ponytail: the static settings, as for http.fetch; PAC/WPAD needs WinHTTP (see net.rs). An
/// https:// or SOCKS proxy is not spoken here and the socket is refused rather than sent around
/// the proxy the user set up; speak them if a network needs that.
async fn tcp(url: &url::Url, addrs: &[SocketAddr]) -> Result<TcpStream, HttpError> {
    let host = url.host_str().ok_or_else(|| blocked("the URL has no host"))?;
    let port = url.port_or_known_default().ok_or_else(|| blocked("the URL has no port"))?;
    // The settings know http and https; a WebSocket goes the way its handshake would.
    let scheme = if url.scheme() == "wss" { "https" } else { "http" };
    let target: Uri = format!("{scheme}://{host}:{port}/").parse().map_err(network)?;
    match Matcher::from_system().intercept(&target) {
        Some(proxy) if proxy.uri().scheme_str() == Some("http") => tunnel(proxy.uri(), proxy.basic_auth(), host, port).await,
        Some(_) => Err(network("the system proxy is not an http:// proxy, which WebSockets cannot use yet")),
        None => direct(addrs).await,
    }
}

/// Connects to a host on the module's list after the address check, with the system trust store.
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
    let handshake = async {
        let stream = tcp(&url, &addrs).await?;
        let connector = Connector::Rustls(Arc::new(tls));
        let (ws, _) = tokio_tungstenite::client_async_tls_with_config(request, stream, Some(config), Some(connector)).await.map_err(network)?;
        Ok(ws)
    };
    tokio::time::timeout(net.limits.connect_timeout, handshake).await.map_err(|_| HttpError::Timeout)?
}

/// Moves frames between one connection and the module until either side ends it.
async fn serve(conn: u32, mut ws: Stream, mut outgoing: mpsc::Receiver<String>, events: mpsc::Sender<GuestEvent>) {
    // A received frame waits here for room in the module's queue, without blocking the frames
    // the module sends meanwhile: a busy module that sends never waits on its own full queue.
    let mut waiting: Option<GuestEvent> = None;
    let reason = loop {
        tokio::select! {
            permit = events.reserve(), if waiting.is_some() => match (permit, waiting.take()) {
                (Ok(permit), Some(event)) => permit.send(event),
                _ => return,
            },
            frame = ws.next(), if waiting.is_none() => match frame {
                Some(Ok(Message::Text(text))) => waiting = Some(GuestEvent::WsMessage { conn, text: text.as_str().to_owned() }),
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
    // Sends fail from here on instead of waiting for a socket that is gone. A frame that arrived
    // before the end still goes to the module, ahead of the close.
    drop(outgoing);
    if let Some(event) = waiting {
        events.send(event).await.ok();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A proxy that answers one CONNECT with `answer`, reports the request head and then relays
    /// bytes to `upstream`.
    async fn proxy(answer: &'static str, upstream: SocketAddr) -> (Uri, tokio::sync::oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
        let (report, head) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(client.read_u8().await.unwrap());
            }
            report.send(String::from_utf8(request).unwrap()).unwrap();
            client.write_all(answer.as_bytes()).await.unwrap();
            let mut server = TcpStream::connect(upstream).await.unwrap();
            tokio::io::copy_bidirectional(&mut client, &mut server).await.ok();
        });
        (uri, head)
    }

    #[tokio::test]
    async fn websocket_goes_through_an_http_proxy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                if message.is_text() {
                    ws.send(Message::text("pong")).await.unwrap();
                }
            }
        });
        let (uri, head) = proxy("HTTP/1.1 200 Connection established\r\n\r\n", server).await;
        let stream = tunnel(&uri, None, "chat.example.com", server.port()).await.unwrap();
        let url = format!("ws://chat.example.com:{}/", server.port());
        let (mut ws, _) = tokio_tungstenite::client_async(url, stream).await.unwrap();
        ws.send(Message::text("ping")).await.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap(), Message::text("pong"));
        assert!(head.await.unwrap().starts_with(&format!("CONNECT chat.example.com:{} HTTP/1.1\r\n", server.port())));
    }

    #[tokio::test]
    async fn proxy_refusal_is_a_network_error() {
        let (uri, _head) = proxy("HTTP/1.1 407 Proxy Authentication Required\r\n\r\n", "127.0.0.1:9".parse().unwrap()).await;
        let result = tunnel(&uri, None, "chat.example.com", 443).await;
        assert!(matches!(&result, Err(HttpError::Network(why)) if why.contains("407")), "{:?}", result.err());
    }
}
