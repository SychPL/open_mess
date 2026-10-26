//! The open_mess module host: loads platform modules (WebAssembly components implementing
//! `wit/module.wit`) and runs them under time, memory and size limits (spec 8.3).

mod actor;
mod bindings;
mod error;
pub mod limits;
mod runtime;

pub use actor::ModuleState;
pub use error::{HostError, ModuleError};
pub use runtime::{AccountInfo, Engine, HostEvent, Manifest, ModuleContext, ModuleHandle, NewMessage, SessionState, ToolDecl, WamIdentity};
