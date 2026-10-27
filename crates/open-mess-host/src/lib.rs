//! The open_mess module host: loads platform modules (WebAssembly components implementing
//! `wit/module.wit`) and runs them under time, memory and size limits (spec 8.3).

mod actor;
mod bindings;
mod error;
pub mod hosts;
pub mod ip_policy;
pub mod limits;
mod net;
mod runtime;
mod secrets;
mod sign_in;
mod storage;
mod timers;
mod wam;
mod ws;

pub use actor::ModuleState;
pub use error::{HostError, ModuleError};
pub use net::{HttpError, Net, Request, Response};
pub use runtime::{
    AccountInfo, Attachment, Engine, HostEvent, Manifest, ModuleContext, ModuleHandle, NewMessage, Participant, SessionState, Target, ToolDecl,
    WamIdentity,
};
pub use secrets::{delete_account_secrets, platform_store as credential_store};
#[cfg(any(test, feature = "test-support"))]
pub use sign_in::FakeWam;
pub use sign_in::{Prompt, SIGN_IN_BUDGET, SignInPrompts, SignInReply, SignInRequest, WAM_ACCOUNT};
pub use wam::WamAccount;
