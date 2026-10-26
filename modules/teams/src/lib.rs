//! The Microsoft Teams platform module of open_mess.
//!
//! The plain modules hold the protocol logic: no network and no clock (time comes in as a
//! parameter), so they are tested natively. `component` wires them to the `platform-module` world
//! of `wit/module.wit` when built for wasm32-wasip2. Protocol facts: `docs/protocols/teams.md`.

pub mod manifest;

#[cfg(target_arch = "wasm32")]
mod component;
