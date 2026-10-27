//! The host side of `wit/module.wit`: generated bindings, the imports a module may call and the
//! conversions between the generated types and the crate's plain ones.

use std::sync::Arc;

use keyring_core::api::CredentialStore;

use crate::error::ModuleError;
use crate::limits;
use crate::net;
use crate::runtime::{AccountInfo, Attachment, HostEvent, HostState, Manifest, NewMessage, Participant, SessionState, Target, ToolDecl, WamIdentity};
use crate::sign_in::{UNAVAILABLE, wait_for_user};
use crate::storage::{Storage, StorageError};

wasmtime::component::bindgen!({
    world: "platform-module",
    path: "../../wit",
    // Imports may wait on the network or the user; exports are called with the async API so a
    // call can be interrupted.
    imports: { default: async },
    exports: { default: async },
});

use open_mess::module::{host, http, secrets, sign_in, storage, types, websocket};

impl HostState {
    /// The module's storage, opened on first use. Not while the manifest is read at load: the
    /// module has no name yet.
    fn storage(&mut self) -> Result<&mut Storage, StorageError> {
        if self.env.name.is_empty() {
            return Err(StorageError::Failed("storage is not available while the module loads".to_string()));
        }
        if self.storage.is_none() {
            self.storage = Some(Storage::open(&self.env.data_dir, &self.env.name)?);
        }
        self.storage.as_mut().ok_or_else(|| StorageError::Failed("storage did not open".to_string()))
    }

    fn secret_store(&self) -> Result<Arc<CredentialStore>, String> {
        if self.env.name.is_empty() {
            return Err("secrets are not available while the module loads".to_string());
        }
        crate::secrets::platform_store()
    }
}

impl types::Host for HostState {}

impl http::Host for HostState {
    async fn fetch(&mut self, req: http::Request) -> Result<http::Response, http::HttpError> {
        let req = net::Request { method: req.method, url: req.url, headers: req.headers, body: req.body };
        let fetch = self.env.net.fetch_up_to(&self.env.hosts, req, self.max_body);
        // A body that keeps trickling in never trips the read timeout: the fetch ends before the
        // call does, so a slow server costs the module an error, not an unload.
        let response = match self.deadline.remaining() {
            Some(left) => tokio::time::timeout(left.saturating_sub(limits::FETCH_MARGIN), fetch).await.map_err(|_| net::HttpError::Timeout)??,
            None => fetch.await?,
        };
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
    async fn get(&mut self, key: String) -> Option<Vec<u8>> {
        match self.storage().and_then(|storage| storage.get(&key)) {
            Ok(value) => value,
            Err(e) => {
                tracing::warn!(module = %self.env.name, "storage read failed: {e:?}");
                None
            }
        }
    }

    async fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), storage::StorageError> {
        self.storage().and_then(|storage| storage.set(&key, &value)).map_err(|e| match e {
            StorageError::QuotaExceeded => storage::StorageError::QuotaExceeded,
            StorageError::Failed(why) => storage::StorageError::Failed(why),
        })
    }

    async fn delete(&mut self, key: String) {
        if let Err(e) = self.storage().and_then(|storage| storage.delete(&key)) {
            tracing::warn!(module = %self.env.name, "storage delete failed: {e:?}");
        }
    }
}

impl secrets::Host for HostState {
    async fn get(&mut self, account: String, key: String) -> Option<String> {
        let store = self.secret_store().ok()?;
        crate::secrets::get(&*store, &self.env.name, &account, &key)
    }

    async fn set(&mut self, account: String, key: String, value: String) -> Result<(), String> {
        let store = self.secret_store()?;
        let env = self.env.clone();
        let storage = self.storage().map_err(|e| format!("{e:?}"))?;
        crate::secrets::set(&*store, storage, &env.name, &account, &key, &value)
    }

    async fn delete(&mut self, account: String, key: String) {
        let env = self.env.clone();
        if let Ok(store) = self.secret_store() {
            crate::secrets::delete(&*store, self.storage().ok(), &env.name, &account, &key);
        }
    }
}

/// Refused until the configuration window exists; codes and URLs are never logged.
impl sign_in::Host for HostState {
    async fn show_device_code(&mut self, _code: String, _verification_url: String) {
        tracing::debug!(module = %self.env.name, "device code not shown: {UNAVAILABLE}");
    }

    async fn show_qr(&mut self, _data: String) {
        tracing::debug!(module = %self.env.name, "QR code not shown: {UNAVAILABLE}");
    }

    async fn open_page(&mut self, _url: String, _redirect_prefix: String) -> Result<String, sign_in::SignInError> {
        wait_for_user(&self.deadline, async { Err(sign_in::SignInError::Refused(UNAVAILABLE.to_string())) }).await
    }

    async fn wam_token(&mut self, _scopes: Vec<String>, _interactive: bool, _claims: Option<String>) -> Result<String, sign_in::SignInError> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(wam) = &self.env.fake_wam {
            return wam(&_scopes, _interactive, _claims.as_deref()).map_err(sign_in::SignInError::Refused);
        }
        wait_for_user(&self.deadline, async { Err(sign_in::SignInError::Refused(UNAVAILABLE.to_string())) }).await
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
        if account.len() > limits::MAX_EVENT_TEXT {
            tracing::warn!(module = %self.env.name, "session event dropped: account id over {} bytes", limits::MAX_EVENT_TEXT);
            return;
        }
        let state = match state {
            host::SessionState::Online => SessionState::Online,
            host::SessionState::Offline => SessionState::Offline,
            host::SessionState::AuthRequired => SessionState::AuthRequired,
        };
        self.env.events.send(HostEvent::Session { module: self.env.name.clone(), account, state }).await.ok();
    }

    async fn message_arrived(&mut self, msg: host::NewMessage) {
        if [&msg.account, &msg.conversation, &msg.message, &msg.sender].iter().any(|text| text.len() > limits::MAX_EVENT_TEXT) {
            tracing::warn!(module = %self.env.name, "message event dropped: a field is over {} bytes", limits::MAX_EVENT_TEXT);
            return;
        }
        let msg = NewMessage {
            module: self.env.name.clone(),
            account: msg.account,
            conversation: msg.conversation,
            message: msg.message,
            sender: msg.sender,
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

impl From<types::Attachment> for Attachment {
    fn from(a: types::Attachment) -> Self {
        Attachment { file_name: a.file_name, content_type: a.content_type, data: a.data }
    }
}

impl From<types::Target> for Target {
    fn from(t: types::Target) -> Self {
        Target {
            conversation_title: t.conversation_title,
            participants: t.participants.into_iter().map(|p| Participant { id: p.id, display_name: p.display_name }).collect(),
            target_text: t.target_text,
        }
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
            types::ModuleError::Uncertain(s) => ModuleError::Uncertain(s),
        }
    }
}
