//! The local HTTP API: MCP on `/mcp` and plain JSON for the CLI on `/api`.
//!
//! Listens on 127.0.0.1 only and answers nothing without the bearer token (spec 8.1), so a web page
//! in the user's browser can't call a tool even though it can reach the port.

use std::future::Future;
use std::net::Ipv4Addr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::Error;
use crate::tools::{Registry, ToolError, ToolInfo};

/// Binds `127.0.0.1:<port>`. A taken port is [`Error::PortInUse`]; there is no fallback port.
pub async fn bind(port: u16) -> Result<TcpListener, Error> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await.map_err(|source| match source.kind() {
        std::io::ErrorKind::AddrInUse => Error::PortInUse(port),
        _ => Error::Listen { port, source },
    })
}

/// Serves `/api/tools`, `/api/call/{tool}` and `/mcp` until `shutdown` completes.
pub async fn serve(
    listener: TcpListener,
    token: String,
    registry: Arc<Registry>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Error> {
    let mcp_config = StreamableHttpServerConfig::default();
    // Open MCP streams would otherwise keep the graceful shutdown waiting forever.
    let close_mcp_sessions = mcp_config.cancellation_token.clone();
    let mcp = StreamableHttpService::new(
        {
            let registry = Arc::clone(&registry);
            move || Ok(McpServer { registry: Arc::clone(&registry) })
        },
        Arc::new(LocalSessionManager::default()),
        mcp_config,
    );
    let app = Router::new()
        .route("/api/tools", get(list_tools))
        .route("/api/call/{tool}", post(call_tool))
        .with_state(registry)
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
        // The daemon is fine; the module behind the tool is not.
        Err(err @ ToolError::Module(_)) => error_response(StatusCode::BAD_GATEWAY, err.to_string()),
    }
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// The MCP face of the registry: `tools/list` and `tools/call` delegate to it unchanged.
#[derive(Clone)]
struct McpServer {
    registry: Arc<Registry>,
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("open_mess", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(&self, _request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
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
        let args = Value::Object(request.arguments.unwrap_or_default());
        match self.registry.call(&request.name, args).await {
            Ok(result) => Ok(CallToolResult::structured(result).into()),
            // MCP: an unknown tool is a protocol error; bad arguments and module failures are tool
            // errors the model can see and act on.
            Err(err @ ToolError::UnknownTool(_)) => Err(ErrorData::invalid_params(err.to_string(), None)),
            Err(err @ (ToolError::InvalidArgs(_) | ToolError::Module(_))) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(err.to_string())]).into())
            }
        }
    }
}
