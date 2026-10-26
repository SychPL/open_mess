//! The host side of `wit/module.wit`: generated bindings, the imports a module may call and the
//! conversions between the generated types and the crate's plain ones.

use crate::error::ModuleError;
use crate::net;
use crate::runtime::{AccountInfo, HostState, Manifest, ToolDecl, WamIdentity};

wasmtime::component::bindgen!({
    world: "platform-module",
    path: "../../wit",
    // Imports may wait on the network or the user; exports are called with the async API so a
    // call can be interrupted.
    imports: { default: async },
    exports: { default: async },
});

use open_mess::module::{host, http, secrets, sign_in, storage, types, websocket};

fn not_yet(what: &str) -> String {
    format!("not supported yet: {what}")
}

impl types::Host for HostState {}

impl http::Host for HostState {
    async fn fetch(&mut self, req: http::Request) -> Result<http::Response, http::HttpError> {
        let req = net::Request { method: req.method, url: req.url, headers: req.headers, body: req.body };
        let response = self.env.net.fetch(&self.env.hosts, req).await?;
        Ok(http::Response { status: response.status, headers: response.headers, body: response.body })
    }
}

impl websocket::Host for HostState {
    async fn open(&mut self, _url: String, _headers: Vec<(String, String)>) -> Result<u32, http::HttpError> {
        Err(http::HttpError::Network(not_yet("websocket")))
    }

    async fn send(&mut self, _conn: u32, _text: String) -> Result<(), http::HttpError> {
        Err(http::HttpError::Network(not_yet("websocket")))
    }

    async fn close(&mut self, _conn: u32) {}
}

impl storage::Host for HostState {
    async fn get(&mut self, _key: String) -> Option<Vec<u8>> {
        None
    }

    async fn set(&mut self, _key: String, _value: Vec<u8>) -> Result<(), storage::StorageError> {
        Err(storage::StorageError::Failed(not_yet("storage")))
    }

    async fn delete(&mut self, _key: String) {}
}

impl secrets::Host for HostState {
    async fn get(&mut self, _account: String, _key: String) -> Option<String> {
        None
    }

    async fn set(&mut self, _account: String, _key: String, _value: String) -> Result<(), String> {
        Err(not_yet("secrets"))
    }

    async fn delete(&mut self, _account: String, _key: String) {}
}

impl sign_in::Host for HostState {
    async fn show_device_code(&mut self, _code: String, _verification_url: String) {}

    async fn show_qr(&mut self, _data: String) {}

    async fn open_page(&mut self, _url: String, _redirect_prefix: String) -> Result<String, sign_in::SignInError> {
        Err(sign_in::SignInError::Refused(not_yet("sign-in")))
    }

    async fn wam_token(&mut self, _scopes: Vec<String>, _interactive: bool) -> Result<String, sign_in::SignInError> {
        Err(sign_in::SignInError::Refused(not_yet("sign-in")))
    }
}

impl host::Host for HostState {
    async fn log(&mut self, _level: u8, _message: String) {}

    async fn set_timer(&mut self, _id: u32, _after_ms: u32) {}

    async fn session(&mut self, _account: String, _state: host::SessionState) {}

    async fn message_arrived(&mut self, _msg: host::NewMessage) {}
}

impl From<net::HttpError> for http::HttpError {
    fn from(e: net::HttpError) -> Self {
        match e {
            net::HttpError::Blocked(s) => http::HttpError::Blocked(s),
            net::HttpError::Timeout => http::HttpError::Timeout,
            net::HttpError::TooLarge => http::HttpError::TooLarge,
            net::HttpError::Throttled(secs) => http::HttpError::Throttled(secs),
            net::HttpError::Network(s) => http::HttpError::Network(s),
        }
    }
}

impl From<types::Manifest> for Manifest {
    fn from(m: types::Manifest) -> Self {
        Manifest {
            name: m.name,
            version: m.version,
            platform: m.platform,
            hosts: m.hosts,
            sign_in_hosts: m.sign_in_hosts,
            wam: m.wam.map(|w| WamIdentity { client_id: w.client_id, scopes: w.scopes }),
            extra_tools: m
                .extra_tools
                .into_iter()
                .map(|t| ToolDecl { name: t.name, description: t.description, input_schema: t.input_schema, writes: t.writes })
                .collect(),
        }
    }
}

impl From<types::AccountInfo> for AccountInfo {
    fn from(a: types::AccountInfo) -> Self {
        AccountInfo { id: a.id, display_name: a.display_name }
    }
}

impl From<types::ModuleError> for ModuleError {
    fn from(e: types::ModuleError) -> Self {
        match e {
            types::ModuleError::AuthRequired(s) => ModuleError::AuthRequired(s),
            types::ModuleError::Incompatible(s) => ModuleError::Incompatible(s),
            types::ModuleError::Platform(s) => ModuleError::Platform(s),
            types::ModuleError::InvalidArgs(s) => ModuleError::InvalidArgs(s),
            types::ModuleError::NotSupported(s) => ModuleError::NotSupported(s),
        }
    }
}
