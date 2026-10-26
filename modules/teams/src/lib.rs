//! The Microsoft Teams platform module of open_mess.
//!
//! The plain modules hold the protocol logic: no network and no clock (time comes in as a
//! parameter), so they are tested natively. `component` wires them to the `platform-module` world
//! of `wit/module.wit` when built for wasm32-wasip2. Protocol facts: `docs/protocols/teams.md`.

pub mod authsvc;
pub mod chat;
pub mod http;
pub mod manifest;
mod time;

#[cfg(target_arch = "wasm32")]
mod component;

/// Why a response could not be decoded or a request was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum ProtoError {
    /// The server answered in a shape the module does not understand; the string is the path of
    /// the missing or mistyped field (spec 9.3). Nothing is guessed.
    Incompatible(String),
    /// A write open_mess never sends, such as a read-state property (spec 9.2).
    Forbidden(String),
}

fn incompatible(path: &str) -> ProtoError {
    ProtoError::Incompatible(path.to_string())
}

/// Parses a response body; anything that is not JSON is incompatible at the root (`$`).
fn parse_json(body: &[u8]) -> Result<serde_json::Value, ProtoError> {
    serde_json::from_slice(body).map_err(|_| incompatible("$"))
}
