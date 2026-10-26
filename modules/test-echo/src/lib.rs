//! A test guest for `crates/open-mess-host`: each tool exercises one host import and reports what
//! came back, and the event exports forward what they receive to `host.log`.
//!
//! Tools (`call`): `echo` returns its arguments; `fetch` GETs `{"url"}` and returns `{"status"}`
//! or `{"error"}` (the `http-error` case name); `ws` opens a WebSocket to `{"url"}`, sends "ping"
//! and returns `{"conn"}` or `{"error"}`; `store` sets key "k" to `{"size"}` bytes and returns the
//! length read back (`{"len"}`, plus `{"error"}` when the set failed); `timer` sets timer 7 to
//! fire after 100 ms; `message` reports account "a" online and a message `{"text"}` in
//! conversation "c" that mentions the user; `sign-in` shows a device code and reports what
//! `open-page` and `wam-token` answered; `trap` crashes the module; `ws-burst` opens a socket to
//! `{"url"}`, stays busy for 500 ms, then sends 40 frames and returns `{"sent"}`. The shared tools
//! `read_messages` and `unread` return one synthetic message or conversation that names the
//! account. The `restore` export succeeds only for account "known"; the `sign-in` export is not
//! supported; `sign-out` reports the account offline; `get-attachment` returns fixed bytes named
//! after the attachment id.

#[cfg(target_arch = "wasm32")]
mod guest {
    // The ABI glue wit-bindgen generates (bindings and the `export!` expansion) uses `unsafe`; no
    // unsafe is written by hand. Listed in AGENTS.md, "Never crash".
    #[allow(unsafe_code)]
    mod bindings {
        wit_bindgen::generate!({ world: "platform-module", path: "../../wit" });

        use super::Echo;
        export!(Echo);
    }

    use bindings::exports::open_mess::module::{events, module};
    use bindings::open_mess::module::{host, http, sign_in, storage, types, websocket};
    use serde_json::{Value, json};

    struct Echo;

    /// The manifest's hosts, padded so the core's tests can add one by patching the built file
    /// (a rebuild with another manifest under the same name).
    const HOSTS: &str = "localhost                               ";

    fn http_error(e: &http::HttpError) -> &'static str {
        match e {
            http::HttpError::Blocked(_) => "blocked",
            http::HttpError::Timeout => "timeout",
            http::HttpError::TooLarge => "too-large",
            http::HttpError::Throttled(_) => "throttled",
            http::HttpError::Network(_) => "network",
        }
    }

    fn call(account: &str, tool: &str, args_json: &str) -> Result<String, types::ModuleError> {
        let args: Value = serde_json::from_str(args_json).map_err(|e| types::ModuleError::InvalidArgs(e.to_string()))?;
        let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let result = match tool {
            "echo" => return Ok(args_json.to_string()),
            "fetch" => match http::fetch(&http::Request { method: "GET".to_string(), url: text("url"), headers: Vec::new(), body: Vec::new() }) {
                Ok(response) => json!({ "status": response.status }),
                Err(e) => json!({ "error": http_error(&e) }),
            },
            "ws" => match websocket::open(&text("url"), &[]) {
                Ok(conn) => match websocket::send(conn, "ping") {
                    Ok(()) => json!({ "conn": conn }),
                    Err(e) => json!({ "error": http_error(&e) }),
                },
                Err(e) => json!({ "error": http_error(&e) }),
            },
            "store" => {
                let size = args.get("size").and_then(Value::as_u64).unwrap_or_default();
                let set = storage::set("k", &vec![7; usize::try_from(size).unwrap_or_default()]);
                let len = storage::get("k").map(|v| v.len()).unwrap_or_default();
                match set {
                    Ok(()) => json!({ "len": len }),
                    Err(storage::StorageError::QuotaExceeded) => json!({ "error": "quota-exceeded", "len": len }),
                    Err(storage::StorageError::Failed(_)) => json!({ "error": "failed", "len": len }),
                }
            }
            "timer" => {
                host::set_timer(7, 100);
                json!({})
            }
            "sign-in" => {
                let describe = |result: Result<String, sign_in::SignInError>| match result {
                    Ok(_) => "ok".to_string(),
                    Err(sign_in::SignInError::Cancelled) => "cancelled".to_string(),
                    Err(sign_in::SignInError::TimedOut) => "timed-out".to_string(),
                    Err(sign_in::SignInError::Refused(why)) => format!("refused: {why}"),
                    Err(sign_in::SignInError::Blocked(why)) => format!("blocked: {why}"),
                };
                sign_in::show_device_code("ABCD-1234", "https://login.example.com/device");
                json!({
                    "open-page": describe(sign_in::open_page("https://login.example.com/", "https://login.example.com/done")),
                    "wam-token": describe(sign_in::wam_token(&["scope".to_string()], true)),
                })
            }
            "trap" => std::process::abort(),
            // Opens a socket, stays busy while the server's frames pile up, then sends 40 frames.
            "ws-burst" => match websocket::open(&text("url"), &[]) {
                Ok(conn) => {
                    let start = std::time::Instant::now();
                    while start.elapsed() < std::time::Duration::from_millis(500) {
                        std::hint::spin_loop();
                    }
                    json!({ "sent": (0..40).take_while(|_| websocket::send(conn, "x").is_ok()).count() })
                }
                Err(e) => json!({ "error": http_error(&e) }),
            },
            "read_messages" => json!({"items": [{
                "id": "m1",
                "conversation": text("conversation"),
                "sent_at": "2026-10-27T10:00:00Z",
                "edited": false,
                "own": false,
                "untrusted": {"sender_name": "Echo", "text": format!("hello {account}")},
            }]}),
            "unread" => json!({"items": [{
                "conversation": "c",
                "kind": "direct",
                "unread": "unread",
                "untrusted": {"title": format!("Echo {account}")},
            }]}),
            "message" => {
                host::session("a", host::SessionState::Online);
                host::message_arrived(&host::NewMessage {
                    account: "a".to_string(),
                    conversation: "c".to_string(),
                    message: text("text"),
                    own: false,
                    mentions_me: true,
                    direct: false,
                });
                json!({})
            }
            _ => return Err(types::ModuleError::NotSupported(tool.to_string())),
        };
        Ok(result.to_string())
    }

    impl module::Guest for Echo {
        // Also tries storage and a host event: the host must let neither through while the
        // manifest is read at load. (Secrets are refused the same way; trying them here could
        // reach the real credential store if that broke.)
        fn manifest() -> types::Manifest {
            storage::set("loaded", b"1").ok();
            host::session("a", host::SessionState::Online);
            types::Manifest {
                name: "test-echo".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                platform: "test".to_string(),
                // black_box: the padded string stays in the file as written.
                hosts: std::hint::black_box(HOSTS).split_whitespace().map(str::to_string).collect(),
                sign_in_hosts: Vec::new(),
                wam: None,
                extra_tools: Vec::new(),
            }
        }

        fn sign_in() -> Result<types::AccountInfo, types::ModuleError> {
            Err(types::ModuleError::NotSupported("sign-in".to_string()))
        }

        // Only account "known" has a session, so tests can tell restore from sign-out.
        fn restore(account: String) -> Result<(), types::ModuleError> {
            if account == "known" { Ok(()) } else { Err(types::ModuleError::AuthRequired(account)) }
        }

        fn sign_out(account: String) {
            host::session(&account, host::SessionState::Offline);
        }

        fn call(account: String, tool: String, args_json: String) -> Result<String, types::ModuleError> {
            call(&account, &tool, &args_json)
        }

        // Fixed bytes, named after the attachment id, so tests choose the file name.
        fn get_attachment(
            _account: String,
            _conversation: String,
            _message: String,
            attachment: String,
        ) -> Result<types::Attachment, types::ModuleError> {
            Ok(types::Attachment { file_name: attachment, content_type: "text/plain".to_string(), data: b"attachment bytes from test-echo".to_vec() })
        }
    }

    impl events::Guest for Echo {
        fn on_ws_message(conn: u32, text: String) {
            host::log(2, &format!("ws {conn} message {text}"));
        }

        fn on_ws_closed(conn: u32, reason: String) {
            host::log(2, &format!("ws {conn} closed {reason}"));
        }

        fn on_timer(id: u32) {
            host::log(2, &format!("timer {id}"));
        }
    }
}
