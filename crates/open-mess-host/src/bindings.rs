//! The host side of `wit/module.wit`: generated bindings, the imports a module may call and the
//! conversions between the generated types and the crate's plain ones.

use crate::error::ModuleError;
use crate::limits;
use crate::net;
use crate::runtime::{AccountInfo, HostEvent, HostState, Manifest, NewMessage, SessionState, ToolDecl, WamIdentity};

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
    async fn open(&mut self, url: String, headers: Vec<(String, String)>) -> Result<u32, http::HttpError> {
        Ok(self.sockets.open(&self.env.net, &self.env.hosts, &url, &headers, &self.guest_events).await?)
    }

    async fn send(&mut self, conn: u32, text: String) -> Result<(), http::HttpError> {
        Ok(self.sockets.send(conn, text).await?)
    }

    async fn close(&mut self, conn: u32) {
        self.sockets.close(conn);
    }
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
    /// 0 error, 1 warn, 2 info, 3 debug, anything higher trace.
    async fn log(&mut self, level: u8, mut message: String) {
        message.truncate(message.floor_char_boundary(limits::MAX_LOG_MESSAGE));
        let module = self.env.name.as_str();
        match level {
            0 => tracing::error!(module, "{message}"),
            1 => tracing::warn!(module, "{message}"),
            2 => tracing::info!(module, "{message}"),
            3 => tracing::debug!(module, "{message}"),
            _ => tracing::trace!(module, "{message}"),
        }
    }

    async fn set_timer(&mut self, id: u32, after_ms: u32) {
        if !self.timers.set(id, after_ms, &self.guest_events) {
            tracing::warn!(module = %self.env.name, "timer not set: {} timers are pending", limits::MAX_TIMERS);
        }
    }

    async fn session(&mut self, account: String, state: host::SessionState) {
        let state = match state {
            host::SessionState::Online => SessionState::Online,
            host::SessionState::Offline => SessionState::Offline,
            host::SessionState::AuthRequired => SessionState::AuthRequired,
        };
        self.env.events.send(HostEvent::Session { module: self.env.name.clone(), account, state }).await.ok();
    }

    async fn message_arrived(&mut self, msg: host::NewMessage) {
        let msg = NewMessage {
            module: self.env.name.clone(),
            account: msg.account,
            conversation: msg.conversation,
            message: msg.message,
            own: msg.own,
            mentions_me: msg.mentions_me,
            direct: msg.direct,
        };
        self.env.events.send(HostEvent::MessageArrived(msg)).await.ok();
    }
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
