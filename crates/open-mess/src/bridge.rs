//! `om mcp-stdio`: the MCP server Claude Code starts over stdio in a session with channels (spec
//! 8.7, R9). Claude Code takes channel notifications only from stdio servers, so this thin bridge
//! forwards the session's tool calls to the daemon's MCP endpoint and the daemon's channel
//! notifications back. It has no platform logic, no accounts and no state of its own, and ends
//! with its session (stdin closes). While the daemon is gone, tool calls get a clear error and the
//! bridge reconnects with a growing delay.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig, ClientRequest, ContentBlock, CustomNotification,
    DiscoverRequestMethod, DiscoverResult, Implementation, ListToolsResult, PaginatedRequestParams, PingRequest, ServerCapabilities, ServerConfig,
    ServerNotification, ToolListChangedNotification,
};
use rmcp::service::{NotificationContext, Peer, RequestContext, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, watch};

use crate::triggers::channel;

/// What a tool call gets while the daemon is unreachable.
pub const UNAVAILABLE: &str = "the open_mess daemon is not available - start it with: om daemon (this session reconnects by itself)";
/// How long a call waits for the daemon when the bridge has not reached it yet.
const CONNECT_WAIT: Duration = Duration::from_secs(5);
/// How often an idle bridge checks that the daemon is still the one it connected to.
const PING_EVERY: Duration = Duration::from_secs(15);
const FIRST_RETRY: Duration = Duration::from_millis(500);
/// How long a bridge whose session ended waits to close the daemon's session.
const QUIT_WAIT: Duration = Duration::from_secs(2);
const MAX_RETRY: Duration = Duration::from_secs(30);

/// The daemon's MCP URL and bearer token, looked up again before every connection: a daemon that
/// restarted may have another port.
pub type Locate = Arc<dyn Fn() -> Result<(String, String), String> + Send + Sync>;

/// Serves one session on `transport` (stdin and stdout for `om mcp-stdio`) until it closes.
pub async fn serve<T>(transport: T, locate: Locate) -> Result<(), String>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (publish, daemon) = watch::channel(None);
    let lost = Arc::new(Notify::new());
    let bridge = Bridge { daemon, lost: Arc::clone(&lost) };
    let session = bridge.serve(transport).await.map_err(|err| err.to_string())?;
    let quit = Arc::new(Notify::new());
    let connection = tokio::spawn(keep_connected(locate, session.peer().clone(), publish, lost, Arc::clone(&quit)));
    let _ = session.waiting().await;
    // Closing the daemon's session, rather than leaving it to expire, takes this session out of
    // the ones notifications go to at once.
    quit.notify_one();
    // A daemon that does not answer in time has its session expire by itself.
    let _ = tokio::time::timeout(QUIT_WAIT, connection).await;
    Ok(())
}

/// The session's side: the tools are the daemon's.
#[derive(Clone)]
struct Bridge {
    daemon: watch::Receiver<Option<Peer<RoleClient>>>,
    /// Told when a call found the connection broken.
    lost: Arc<Notify>,
}

impl Bridge {
    /// The daemon's connection, waiting a little for one being made.
    async fn daemon(&self) -> Option<Peer<RoleClient>> {
        let mut daemon = self.daemon.clone();
        let connected = tokio::time::timeout(CONNECT_WAIT, daemon.wait_for(Option::is_some)).await;
        connected.ok()?.ok()?.clone()
    }
}

impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        let capabilities =
            ServerCapabilities::builder().enable_experimental_with(channel::capability()).enable_tools().enable_tool_list_changed().build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("open_mess", env!("CARGO_PKG_VERSION")))
            .with_instructions(channel::INSTRUCTIONS)
    }

    /// Answered like a server older than the discover lifecycle: Claude Code then connects with
    /// `initialize`, keeps a session, and takes channel notifications, which it refuses on
    /// connections of the newer, sessionless protocol.
    async fn discover(&self, _context: RequestContext<RoleServer>) -> Result<DiscoverResult, ErrorData> {
        Err(ErrorData::method_not_found::<DiscoverRequestMethod>())
    }

    async fn list_tools(&self, request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        let Some(daemon) = self.daemon().await else {
            return Err(ErrorData::internal_error(UNAVAILABLE, None));
        };
        daemon.list_tools(request).await.map_err(|err| {
            self.lost.notify_one();
            ErrorData::internal_error(format!("{UNAVAILABLE}: {err}"), None)
        })
    }

    async fn call_tool(&self, request: CallToolRequestParams, _context: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let unavailable = || Ok(CallToolResult::error(vec![ContentBlock::text(UNAVAILABLE)]).into());
        let Some(daemon) = self.daemon().await else {
            return unavailable();
        };
        match daemon.call_tool_once(request).await {
            Ok(response) => Ok(response),
            // The daemon answered with an error (an unknown tool): pass it on.
            Err(ServiceError::McpError(err)) => Err(err),
            Err(_) => {
                self.lost.notify_one();
                unavailable()
            }
        }
    }
}

/// The daemon's side: channel notifications go on to the session.
struct Forward {
    session: Peer<RoleServer>,
}

impl ClientHandler for Forward {
    fn get_info(&self) -> ClientConfig {
        let capabilities = ClientCapabilities::builder().enable_experimental_with(channel::capability()).build();
        ClientConfig::new(capabilities, Implementation::new("open_mess mcp-stdio", env!("CARGO_PKG_VERSION")))
    }

    async fn on_custom_notification(&self, notification: CustomNotification, _context: NotificationContext<RoleClient>) {
        if notification.method == channel::METHOD {
            let _ = self.session.send_notification(ServerNotification::CustomNotification(notification)).await;
        }
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        tools_changed(&self.session).await;
    }
}

async fn tools_changed(session: &Peer<RoleServer>) {
    let _ = session.send_notification(ServerNotification::ToolListChangedNotification(ToolListChangedNotification::default())).await;
}

/// Connects to the daemon and connects again whenever the connection is lost, each failed attempt
/// waiting twice as long as the last, up to [`MAX_RETRY`]; closes the daemon's session and returns
/// when told to `quit`.
async fn keep_connected(
    locate: Locate,
    session: Peer<RoleServer>,
    publish: watch::Sender<Option<Peer<RoleClient>>>,
    lost: Arc<Notify>,
    quit: Arc<Notify>,
) {
    let mut retry = FIRST_RETRY;
    loop {
        let connected = tokio::select! {
            connected = connect(&locate, session.clone()) => connected,
            () = quit.notified() => return,
        };
        if let Ok(connection) = connected {
            retry = FIRST_RETRY;
            let daemon = connection.peer().clone();
            publish.send_replace(Some(daemon.clone()));
            // The daemon may have other tools than the one before it.
            tools_changed(&session).await;
            let quitting = loop {
                tokio::select! {
                    () = quit.notified() => break true,
                    () = lost.notified() => break false,
                    () = tokio::time::sleep(PING_EVERY) => {
                        if daemon.send_request(ClientRequest::PingRequest(PingRequest::default())).await.is_err() {
                            break false;
                        }
                    }
                }
            };
            publish.send_replace(None);
            let _ = connection.cancel().await;
            if quitting {
                return;
            }
        }
        tokio::select! {
            () = tokio::time::sleep(retry) => {}
            () = quit.notified() => return,
        }
        retry = (retry * 2).min(MAX_RETRY);
    }
}

async fn connect(locate: &Locate, session: Peer<RoleServer>) -> Result<RunningService<RoleClient, Forward>, String> {
    let (url, token) = locate()?;
    // reqwest is built without a TLS crypto provider (see Cargo.toml); Err means one is installed.
    rustls::crypto::ring::default_provider().install_default().ok();
    // HTTP_PROXY and friends would hand the token to a proxy.
    let client = reqwest::Client::builder().no_proxy().connect_timeout(Duration::from_secs(5)).build().map_err(|err| err.to_string())?;
    let transport = StreamableHttpClientTransport::with_client(client, StreamableHttpClientTransportConfig::with_uri(url).auth_header(token));
    Forward { session }.serve(transport).await.map_err(|err| err.to_string())
}
