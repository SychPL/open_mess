//! The local HTTP API: MCP on `/mcp`, plain JSON for the CLI on `/api`, and `POST /api/ui/open`,
//! which opens the configuration window (`om config`).
//!
//! Listens on 127.0.0.1 only and answers nothing without the bearer token (spec 8.1), so a web page
//! in the user's browser can't call a tool even though it can reach the port.

use std::collections::HashMap;
use std::future::Future;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_core::Stream;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientJsonRpcMessage, ContentBlock, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, ServerJsonRpcMessage, Tool,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::transport::streamable_http_server::session::ServerSseMessage;
use rmcp::transport::streamable_http_server::session::local::{LocalSessionManager, LocalSessionManagerError};
use rmcp::transport::streamable_http_server::{SessionId, SessionManager, StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::Error;
use crate::tools::{Registry, ToolError, ToolInfo};
use crate::triggers::channel;
use crate::ui::Opener;

/// Binds `127.0.0.1:<port>`. A taken port is [`Error::PortInUse`]; there is no fallback port.
pub async fn bind(port: u16) -> Result<TcpListener, Error> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await.map_err(|source| match source.kind() {
        std::io::ErrorKind::AddrInUse => Error::PortInUse(port),
        _ => Error::Listen { port, source },
    })
}

/// Serves `/api/tools`, `/api/call/{tool}`, `/api/ui/open` and `/mcp` until `shutdown` completes.
pub async fn serve(
    listener: TcpListener,
    token: String,
    registry: Arc<Registry>,
    window: Opener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Error> {
    let mcp_config = StreamableHttpServerConfig::default();
    // Open MCP streams would otherwise keep the graceful shutdown waiting forever.
    let close_mcp_sessions = mcp_config.cancellation_token.clone();
    let mcp = StreamableHttpService::new(
        {
            let registry = Arc::clone(&registry);
            move || Ok(McpServer { registry: Arc::clone(&registry), channel: Arc::default() })
        },
        Arc::new(McpSessions::default()),
        mcp_config,
    );
    let app = Router::new()
        .route("/api/tools", get(list_tools))
        .route("/api/call/{tool}", post(call_tool))
        .with_state(registry)
        // Not a tool: opening the window is harmless, and the window is where the user decides.
        .route(
            "/api/ui/open",
            post(move || {
                let window = window.clone();
                async move { open_window(&window).await }
            }),
        )
        .route_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(Arc::new(token), require_token));
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            close_mcp_sessions.cancel();
        })
        .await?;
    Ok(())
}

async fn require_token(State(token): State<Arc<String>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, presented)| presented.trim());
    match presented {
        Some(presented) if constant_time_eq(presented.as_bytes(), token.as_bytes()) => next.run(request).await,
        _ => (StatusCode::UNAUTHORIZED, [(WWW_AUTHENTICATE, "Bearer")], "missing or wrong bearer token").into_response(),
    }
}

/// Compares without stopping at the first difference, so timing doesn't reveal the token.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

async fn list_tools(State(registry): State<Arc<Registry>>) -> Json<Vec<ToolInfo>> {
    Json(registry.list())
}

/// The body is the arguments object; an empty body means no arguments.
async fn call_tool(State(registry): State<Arc<Registry>>, Path(tool): Path<String>, body: Bytes) -> Response {
    let args = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(args) => args,
            Err(err) => return error_response(StatusCode::BAD_REQUEST, format!("arguments are not valid JSON: {err}")),
        }
    };
    match registry.call(&tool, args).await {
        Ok(result) => Json(result).into_response(),
        Err(err @ ToolError::UnknownTool(_)) => error_response(StatusCode::NOT_FOUND, err.to_string()),
        Err(err @ ToolError::InvalidArgs(_)) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
        Err(err @ ToolError::NotAllowed(_)) => error_response(StatusCode::FORBIDDEN, err.to_string()),
        // The daemon is fine; the module behind the tool is not.
        Err(err @ ToolError::Module(_)) => error_response(StatusCode::BAD_GATEWAY, err.to_string()),
        Err(err @ ToolError::Failed(_)) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

/// Answers once the window is open, or with why it did not open.
async fn open_window(window: &Opener) -> Response {
    match window.open().await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err @ Error::NoWindow) => error_response(StatusCode::NOT_IMPLEMENTED, err.to_string()),
        Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// MCP sessions open at once. Each Claude Code or Codex session keeps one; a client that opens them
/// in a loop must not grow the daemon without end (spec 15).
pub const MAX_MCP_SESSIONS: usize = 64;

/// rmcp's session manager, with at most [`MAX_MCP_SESSIONS`] sessions: a new one closes the session
/// unused the longest. Its client opens another when it next asks for something.
#[derive(Default)]
pub struct McpSessions {
    inner: LocalSessionManager,
    /// Each session and when it was last used, in uses of any session.
    used: Mutex<(u64, HashMap<SessionId, u64>)>,
}

impl McpSessions {
    fn touch(&self, id: &SessionId) {
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        let (clock, sessions) = &mut *used;
        *clock += 1;
        if let Some(at) = sessions.get_mut(id) {
            *at = *clock;
        }
    }
}

impl SessionManager for McpSessions {
    type Error = LocalSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let idle = {
            let used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
            let sessions = &used.1;
            (sessions.len() >= MAX_MCP_SESSIONS).then(|| sessions.iter().min_by_key(|(_, at)| **at).map(|(id, _)| id.clone())).flatten()
        };
        if let Some(idle) = idle {
            // One that would not close is the inner manager's to end; the new one opens anyway.
            let _ = self.close_session(&idle).await;
        }
        let (id, transport) = self.inner.create_session().await?;
        {
            let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
            let (clock, sessions) = &mut *used;
            *clock += 1;
            sessions.insert(id.clone(), *clock);
        }
        Ok((id, transport))
    }

    async fn initialize_session(&self, id: &SessionId, message: ClientJsonRpcMessage) -> Result<ServerJsonRpcMessage, Self::Error> {
        self.touch(id);
        self.inner.initialize_session(id, message).await
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        self.inner.has_session(id).await
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        self.used.lock().unwrap_or_else(PoisonError::into_inner).1.remove(id);
        self.inner.close_session(id).await
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.touch(id);
        self.inner.create_stream(id, message).await
    }

    async fn accept_message(&self, id: &SessionId, message: ClientJsonRpcMessage) -> Result<(), Self::Error> {
        self.touch(id);
        self.inner.accept_message(id, message).await
    }

    async fn create_standalone_stream(&self, id: &SessionId) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.touch(id);
        self.inner.create_standalone_stream(id).await
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.touch(id);
        self.inner.resume(id, last_event_id).await
    }
}

/// The MCP face of the registry: `tools/list` and `tools/call` delegate to it unchanged. One per
/// MCP session.
#[derive(Clone)]
struct McpServer {
    registry: Arc<Registry>,
    /// The session's id among the channel sessions, when its client declared the capability
    /// (triggers/channel.rs).
    channel: Arc<OnceLock<u64>>,
}

impl McpServer {
    /// Notes a request: trigger notifications go to the most recently active session.
    fn touch(&self) {
        if let Some(id) = self.channel.get() {
            self.registry.channel_sessions().touch(*id);
        }
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_experimental_with(channel::capability()).enable_tools().build())
            .with_server_info(Implementation::new("open_mess", env!("CARGO_PKG_VERSION")))
            .with_instructions(channel::INSTRUCTIONS)
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let declared = context
            .peer
            .peer_info()
            .is_some_and(|info| info.capabilities.experimental.as_ref().is_some_and(|experimental| experimental.contains_key(channel::CAPABILITY)));
        if declared {
            // Once, however often a client says it is initialized.
            self.channel.get_or_init(|| self.registry.channel_sessions().register(context.peer));
        }
    }

    /// `om mcp-stdio` pings every few seconds: a channel session that missed the daemon's pings
    /// for a while is reachable again.
    async fn ping(&self, _context: RequestContext<RoleServer>) -> Result<(), ErrorData> {
        if let Some(id) = self.channel.get() {
            self.registry.channel_sessions().seen(*id);
        }
        Ok(())
    }

    async fn list_tools(&self, _request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        self.touch();
        let tools = self
            .registry
            .list()
            .into_iter()
            .map(|tool| {
                let schema = match tool.input_schema {
                    Value::Object(schema) => schema,
                    _ => serde_json::Map::new(),
                };
                Tool::new(tool.name, tool.description, schema)
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _context: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        self.touch();
        let args = Value::Object(request.arguments.unwrap_or_default());
        match self.registry.call(&request.name, args).await {
            Ok(result) => Ok(CallToolResult::structured(result).into()),
            // MCP: an unknown tool is a protocol error; bad arguments and module failures are tool
            // errors the model can see and act on.
            Err(err @ ToolError::UnknownTool(_)) => Err(ErrorData::invalid_params(err.to_string(), None)),
            Err(err @ (ToolError::InvalidArgs(_) | ToolError::NotAllowed(_) | ToolError::Module(_) | ToolError::Failed(_))) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(err.to_string())]).into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mcp_sessions_are_capped_closing_the_one_unused_longest() {
        let sessions = McpSessions::default();
        let mut open = Vec::new();
        for _ in 0..MAX_MCP_SESSIONS {
            open.push(sessions.create_session().await.unwrap());
        }
        let ids: Vec<SessionId> = open.iter().map(|(id, _)| id.clone()).collect();
        // The first is used again: the second has been unused the longest.
        sessions.touch(&ids[0]);
        let (newest, _transport) = sessions.create_session().await.unwrap();
        assert!(!sessions.has_session(&ids[1]).await.unwrap());
        for id in [&ids[0], &ids[2], &newest] {
            assert!(sessions.has_session(id).await.unwrap());
        }
        assert_eq!(sessions.used.lock().unwrap().1.len(), MAX_MCP_SESSIONS);
    }
}
