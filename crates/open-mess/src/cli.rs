//! The `om` command line.
//!
//! Besides `om daemon` and `om mcp-command`, every command is a tool from the daemon's registry:
//! `om <tool> [--<param> <value>]... [--json-args '<json>'] [--json]`. The CLI asks the running
//! daemon which tools exist, so the CLI and MCP always offer the same tools.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use reqwest::StatusCode;
use serde_json::{Map, Value};

use crate::daemon::{self, LAST_START_ERROR_FILE};
use crate::store::{DEFAULT_PORT, Store};
use crate::tools::ToolInfo;
use crate::{paths, secrets};

#[derive(Debug, Parser)]
#[command(name = "om", bin_name = "om", version, about = "open_mess: your messengers, for AI agents", arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Run the open_mess daemon (one per Windows user).
    Daemon,
    /// Print the command that connects Claude Code to open_mess. It contains the local API token.
    McpCommand,
    /// A tool: om <tool> [--<param> <value>]... [--json-args '<json>'] [--json]
    #[command(external_subcommand)]
    Tool(Vec<String>),
}

/// The longest a script waits for one answer from the daemon.
// ponytail: one limit for every tool; a tool that legitimately runs longer needs its own.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Why `om` stopped, with its exit code.
struct Failure {
    code: u8,
    message: String,
}

/// A tool error, or anything else that went wrong talking to the daemon.
fn failed(message: impl Into<String>) -> Failure {
    Failure { code: 1, message: message.into() }
}

fn bad_usage(message: impl Into<String>) -> Failure {
    Failure { code: 3, message: message.into() }
}

fn not_running() -> Failure {
    let mut message = String::from("open_mess daemon is not running - start it with: om daemon");
    if let Some(reason) = last_start_error() {
        message.push_str("\nlast start failed: ");
        message.push_str(&reason);
    }
    Failure { code: 2, message }
}

/// Why the daemon's last start failed, if it did. A successful start deletes the file.
fn last_start_error() -> Option<String> {
    let text = std::fs::read_to_string(paths::data_dir().ok()?.join(LAST_START_ERROR_FILE)).ok()?;
    text.lines().next().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned)
}

/// Runs the `om` command line and returns its exit code.
pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            // Help and --version go to stdout and succeed; anything else is bad usage.
            return if err.use_stderr() { ExitCode::from(3) } else { ExitCode::SUCCESS };
        }
    };
    let result = match cli.command {
        Action::Daemon => daemon::run().map_err(|err| failed(err.to_string())),
        Action::McpCommand => mcp_command(),
        Action::Tool(args) => run_tool(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            let _ = writeln!(std::io::stderr(), "{}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

fn mcp_command() -> Result<(), Failure> {
    let port = port()?;
    let token = token()?;
    written(writeln!(
        std::io::stdout(),
        "claude mcp add --transport http --scope user open_mess http://127.0.0.1:{port}/mcp --header \"Authorization: Bearer {token}\""
    ))
}

/// Output goes through writeln!, never println!, which panics when stdout is closed. A reader that
/// went away (`om status | head -1`) is fine; any other write error means lost output.
fn written(result: std::io::Result<()>) -> Result<(), Failure> {
    match result {
        Err(err) if err.kind() != std::io::ErrorKind::BrokenPipe => Err(failed(format!("cannot write the result: {err}"))),
        _ => Ok(()),
    }
}

/// `OPEN_MESS_PORT`, else the port stored in the data folder, else the default.
fn port() -> Result<u16, Failure> {
    if let Some(port) = std::env::var_os("OPEN_MESS_PORT") {
        return port
            .to_str()
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| bad_usage(format!("OPEN_MESS_PORT must be a number from 1 to 65535, found {port:?}")));
    }
    let db = paths::data_dir().map_err(|err| failed(err.to_string()))?.join("state.sqlite3");
    // Don't create the daemon's database from the CLI: no file means nothing was configured yet.
    if !db.exists() {
        return Ok(DEFAULT_PORT);
    }
    Store::open(&db).and_then(|store| store.port()).map_err(|err| failed(err.to_string()))
}

/// `OPEN_MESS_TOKEN`, else the token the daemon stored at its first start.
fn token() -> Result<String, Failure> {
    if let Ok(token) = std::env::var("OPEN_MESS_TOKEN") {
        return Ok(token);
    }
    // No token means the daemon has never started.
    secrets::stored_local_api_token().map_err(|err| failed(err.to_string()))?.ok_or_else(not_running)
}

/// The arguments of one tool call, from what follows the tool name.
struct ToolArgs {
    args: Value,
    json_output: bool,
}

fn parse_tool_args(words: &[String]) -> Result<ToolArgs, Failure> {
    let mut params = Map::new();
    let mut json_args = None;
    let mut json_output = false;
    let mut words = words.iter();
    while let Some(word) = words.next() {
        let Some(name) = word.strip_prefix("--").filter(|name| !name.is_empty()) else {
            return Err(bad_usage(format!("unexpected {word:?}: tool parameters are --<name> <value>")));
        };
        if name == "json" {
            json_output = true;
            continue;
        }
        let value = words.next().ok_or_else(|| bad_usage(format!("--{name} needs a value")))?;
        if name == "json-args" {
            json_args = Some(value);
        } else {
            // A value that parses as JSON (a number, true, a list...) is passed as such.
            params.insert(name.into(), serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.clone())));
        }
    }
    let args = match json_args {
        None => Value::Object(params),
        Some(_) if !params.is_empty() => return Err(bad_usage("use either --json-args or --<name> <value> parameters, not both")),
        Some(json) => match serde_json::from_str(json) {
            Ok(Value::Object(args)) => Value::Object(args),
            Ok(_) => return Err(bad_usage("--json-args must be a JSON object")),
            Err(err) => return Err(bad_usage(format!("--json-args is not valid JSON: {err}"))),
        },
    };
    Ok(ToolArgs { args, json_output })
}

fn run_tool(words: &[String]) -> Result<(), Failure> {
    let Some((tool, rest)) = words.split_first() else {
        return Err(bad_usage("missing tool name"));
    };
    let ToolArgs { args, json_output } = parse_tool_args(rest)?;
    // The last start failed and none has succeeded since, so whatever holds the port now is not
    // the daemon: don't send it the token.
    if last_start_error().is_some() {
        return Err(not_running());
    }
    let port = port()?;
    let token = token()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|err| failed(err.to_string()))?;
    let result = runtime.block_on(call(port, &token, tool, &args))?;
    written(print_result(&result, json_output))
}

async fn call(port: u16, token: &str, tool: &str, args: &Value) -> Result<Value, Failure> {
    let client = reqwest::Client::builder()
        // HTTP_PROXY and friends would hand the token to a proxy.
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|err| failed(err.to_string()))?;
    let base = format!("http://127.0.0.1:{port}/api");

    let response = client.get(format!("{base}/tools")).bearer_auth(token).send().await.map_err(request_failed)?;
    let tools: Vec<ToolInfo> =
        check(response).await?.json().await.map_err(|_| failed(format!("unexpected answer from 127.0.0.1:{port} - is it open_mess?")))?;
    if !tools.iter().any(|known| known.name == tool) {
        let names: Vec<&str> = tools.iter().map(|known| known.name.as_str()).collect();
        return Err(bad_usage(format!("unknown tool {tool:?} - available: {}", names.join(", "))));
    }

    let response = client.post(format!("{base}/call/{tool}")).bearer_auth(token).json(args).send().await.map_err(request_failed)?;
    check(response).await?.json().await.map_err(|err| failed(format!("unreadable result: {err}")))
}

fn request_failed(err: reqwest::Error) -> Failure {
    if err.is_connect() {
        not_running()
    } else if err.is_timeout() {
        failed(format!("the daemon did not answer within {} s", REQUEST_TIMEOUT.as_secs()))
    } else {
        failed(format!("cannot reach the daemon: {err}"))
    }
}

/// Turns an error status into a failure with the API's message.
async fn check(response: reqwest::Response) -> Result<reqwest::Response, Failure> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status == StatusCode::UNAUTHORIZED {
        return Err(failed("the daemon refused the token (HTTP 401) - is OPEN_MESS_TOKEN right?"));
    }
    let body: Value = response.json().await.unwrap_or(Value::Null);
    let message = body.get("error").and_then(Value::as_str).map_or_else(|| format!("the daemon answered HTTP {status}"), str::to_owned);
    match status {
        StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND => Err(bad_usage(message)),
        _ => Err(failed(message)),
    }
}

fn print_result(result: &Value, json_output: bool) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    match result {
        Value::Object(fields) if !json_output => {
            for (key, value) in fields {
                match value {
                    Value::String(text) => writeln!(stdout, "{key}: {text}")?,
                    other => writeln!(stdout, "{key}: {other}")?,
                }
            }
            Ok(())
        }
        _ => writeln!(stdout, "{}", serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())),
    }
}
