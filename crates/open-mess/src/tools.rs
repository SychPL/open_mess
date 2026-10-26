//! The tool registry shared by MCP and the CLI.
//!
//! Every capability is a tool here, with one name, one parameter schema and one result shape for
//! both front ends (AGENTS.md, "One tool registry").

use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A tool as both front ends describe it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub input_schema: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    /// A module could not run the tool, or failed; the message is safe to show.
    #[error("{0}")]
    Module(String),
}

/// The tools of this daemon.
#[derive(Debug)]
pub struct Registry {
    started: Instant,
}

impl Registry {
    /// The built-in tools; `started` is when the daemon started, for `status`.
    pub fn with_builtin(started: Instant) -> Registry {
        Registry { started }
    }

    pub fn list(&self) -> Vec<ToolInfo> {
        vec![ToolInfo {
            name: "status".into(),
            description: "State of the open_mess daemon, its modules and accounts.".into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }]
    }

    /// Runs a tool. `args` is the arguments object.
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        match name {
            "status" => {
                no_params(&args)?;
                Ok(json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "uptime_seconds": self.started.elapsed().as_secs(),
                    "modules": [],
                    "accounts": [],
                }))
            }
            _ => Err(ToolError::UnknownTool(name.into())),
        }
    }
}

fn no_params(args: &Value) -> Result<(), ToolError> {
    match args {
        Value::Object(map) => match map.keys().next() {
            None => Ok(()),
            Some(key) => Err(ToolError::InvalidArgs(format!("unknown parameter {key:?}"))),
        },
        _ => Err(ToolError::InvalidArgs("arguments must be a JSON object".into())),
    }
}
