//! A test guest for `crates/open-mess-host`: each tool exercises one host import and reports what
//! came back, and the event exports forward what they receive to `host.log`.
//!
//! Tools (`call`): `echo` returns its arguments; `fetch` GETs `{"url"}` and returns `{"status"}`
//! or `{"error"}` (the `http-error` case name); `ws` opens a WebSocket to `{"url"}`, sends "ping"
//! and returns `{"conn"}` or `{"error"}`; `store` sets key "k" to `{"size"}` bytes and returns the
//! length read back (`{"len"}`, plus `{"error"}` when the set failed); `timer` sets timer 7 to
//! fire after 100 ms; `message` reports account "a" online and a message `{"text"}` from "p1" in
//! conversation "c" that mentions the user; `sign-in` shows a device code and reports what
//! `open-page` and `wam-token` answered; `trap` crashes the module; `ws-burst` opens a socket to
//! `{"url"}`, stays busy for 500 ms, then sends 40 frames and returns `{"sent"}`. The shared tools
//! `read_messages` and `unread` return one synthetic message or conversation that names the
//! account; `list_conversations` returns the group chats "c1" and "c2". The writes `send`,
//! `edit_message` and `react` append their tool and arguments to the storage key "writes" (the
//! tool `writes` returns them) and succeed, except a `text` of "uncertain" (the module error), "fail"
//! (a platform error) or "slow" (recorded, then busy until the call limit). `describe-target`
//! returns the target the tool `set-target` stored for `{"conversation"}` (`{"participants"}` as
//! `[id, name]` pairs, `{"text"}` for any message), else participant "p1" (Pat) and "message <id>".
//! The `restore` export succeeds only for account "known"; the `sign-in` export shows a device code,
//! then opens the page `https://login.example.com/start` with the redirect prefix
//! `https://login.example.com/done` and signs in the account named by the redirect's `account=`
//! parameter (a sign-in error ends it as a platform error naming the case); after the tool
//! `sign-in-with` `{"how": "windows"}` it signs in account "windows" with a WAM token for the
//! manifest's identity (the Teams client id; for the manual check only) instead, and after
//! `{"how": "device-code"}` it shows a device code and polls `http.fetch` until the host refuses
//! with "sign-in ...", which it returns as a platform error (`{"how": "qr-refresh"}` the same after a
//! QR code replaced once); `{"how": "qr-loop"}` shows QR codes
//! forever without polling (a misbehaving module); and after `{"how": "microsoft"}` its
//! page is Microsoft's own sign-in page (for the manual check of the sign-in window); `sign-out` reports the
//! account offline; `get-attachment` downloads an `http://` attachment id and returns fixed bytes
//! named after any other.

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
    /// The Windows account identity: the Teams desktop client id and the chat service's scope
    /// (docs/protocols/teams.md, "Live results" item 1), for the manual check of WAM.
    const TEAMS_CLIENT: &str = "1fec8e78-bce4-4aaf-ab1b-5451cc387264";
    const CHAT_SCOPE: &str = "https://ic3.teams.office.com/.default";

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
                    Err(e) => sign_in_error(&e),
                };
                sign_in::show_device_code("ABCD-1234", "https://login.example.com/device");
                json!({
                    "open-page": describe(sign_in::open_page("https://login.example.com/", "https://login.example.com/done")),
                    "wam-token": describe(sign_in::wam_token(&["scope".to_string()], true, None)),
                })
            }
            "sign-in-with" => {
                storage::set("sign-in-with", text("how").as_bytes()).map_err(|_| types::ModuleError::Platform("storage".to_string()))?;
                json!({})
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
            "list_conversations" => {
                let items = ["c1", "c2"].map(|id| json!({"id": id, "kind": "group", "untrusted": {"title": format!("Echo {id}")}}));
                json!({ "items": items })
            }
            "unread" => json!({"items": [{
                "conversation": "c",
                "kind": "direct",
                "unread": "unread",
                "untrusted": {"title": format!("Echo {account}")},
            }]}),
            "send" | "edit_message" | "react" => {
                let mut writes = stored("writes").as_array().cloned().unwrap_or_default();
                writes.push(json!({ "tool": tool, "args": args }));
                storage::set("writes", Value::from(writes).to_string().as_bytes())
                    .map_err(|_| types::ModuleError::Platform("storage".to_string()))?;
                match text("text").as_str() {
                    "uncertain" => return Err(types::ModuleError::Uncertain("no answer from the server".to_string())),
                    "fail" => return Err(types::ModuleError::Platform("refused".to_string())),
                    "slow" => loop {
                        std::hint::spin_loop();
                    },
                    _ => json!({}),
                }
            }
            "writes" => json!({ "items": stored("writes").as_array().cloned().unwrap_or_default() }),
            "set-target" => {
                storage::set(&format!("target:{}", text("conversation")), args_json.as_bytes())
                    .map_err(|_| types::ModuleError::Platform("storage".to_string()))?;
                json!({})
            }
            "message" => {
                host::session("a", host::SessionState::Online);
                host::message_arrived(&host::NewMessage {
                    account: "a".to_string(),
                    conversation: "c".to_string(),
                    message: text("text"),
                    sender: "p1".to_string(),
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

    fn sign_in_error(e: &sign_in::SignInError) -> String {
        match e {
            sign_in::SignInError::Cancelled => "cancelled".to_string(),
            sign_in::SignInError::TimedOut => "timed-out".to_string(),
            sign_in::SignInError::Refused(why) => format!("refused: {why}"),
            sign_in::SignInError::Blocked(why) => format!("blocked: {why}"),
        }
    }

    /// Polls, as a device-code sign-in would, until the host says the code is abandoned.
    fn poll_until_abandoned() -> types::ModuleError {
        loop {
            let poll = http::Request {
                method: "POST".to_string(),
                url: "https://login.example.com/token".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            };
            if let Err(http::HttpError::Blocked(why)) = http::fetch(&poll)
                && why.starts_with("sign-in")
            {
                return types::ModuleError::Platform(why);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// A JSON value the module stored under `key`, else null.
    fn stored(key: &str) -> Value {
        storage::get(key).and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
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
                sign_in_hosts: vec!["login.example.com".to_string(), "login.microsoftonline.com".to_string()],
                wam: Some(types::WamIdentity { client_id: TEAMS_CLIENT.to_string(), scopes: vec![CHAT_SCOPE.to_string()] }),
                extra_tools: Vec::new(),
            }
        }

        fn sign_in() -> Result<types::AccountInfo, types::ModuleError> {
            let how = storage::get("sign-in-with").unwrap_or_default();
            if how == b"device-code" {
                sign_in::show_device_code("WXYZ-5678", "https://login.example.com/device");
                return Err(poll_until_abandoned());
            }
            if how == b"qr-refresh" {
                // A code that is replaced once before the module polls.
                sign_in::show_qr("otpauth://open-mess-test/1");
                std::thread::sleep(std::time::Duration::from_millis(300));
                sign_in::show_qr("otpauth://open-mess-test/2");
                return Err(poll_until_abandoned());
            }
            if how == b"qr-loop" {
                // Misbehaves: shows a new QR code every 100 ms and never stops.
                loop {
                    sign_in::show_qr("otpauth://open-mess-test");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            if how == b"windows" {
                // The manual check of WAM: only the token's length is logged.
                let token =
                    sign_in::wam_token(&[CHAT_SCOPE.to_string()], false, None).map_err(|e| types::ModuleError::Platform(sign_in_error(&e)))?;
                host::log(2, &format!("Windows token: {} bytes", token.len()));
                return Ok(types::AccountInfo { id: "windows".to_string(), display_name: format!("Windows token of {} bytes", token.len()) });
            }
            sign_in::show_device_code("ABCD-1234", "https://login.example.com/device");
            // The manual check of the sign-in window opens Microsoft's real sign-in page.
            let (start, redirect) = match how.as_slice() {
                b"microsoft" => ("https://login.microsoftonline.com/", "https://login.microsoftonline.com/common/oauth2/nativeclient"),
                _ => ("https://login.example.com/start", "https://login.example.com/done"),
            };
            let url = sign_in::open_page(start, redirect).map_err(|e| types::ModuleError::Platform(sign_in_error(&e)))?;
            let id = url.split_once("account=").map(|(_, id)| id.split('&').next().unwrap_or(id)).unwrap_or_default();
            Ok(types::AccountInfo { id: id.to_string(), display_name: format!("Echo {id}") })
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

        // An `http://` id is downloaded; any other id gets fixed bytes named after it.
        fn get_attachment(
            _account: String,
            _conversation: String,
            _message: String,
            attachment: String,
        ) -> Result<types::Attachment, types::ModuleError> {
            let data = if attachment.starts_with("http://") {
                let request = http::Request { method: "GET".to_string(), url: attachment.clone(), headers: Vec::new(), body: Vec::new() };
                http::fetch(&request).map_err(|e| types::ModuleError::Platform(http_error(&e).to_string()))?.body
            } else {
                b"attachment bytes from test-echo".to_vec()
            };
            Ok(types::Attachment { file_name: attachment, content_type: "text/plain".to_string(), data })
        }

        fn describe_target(_account: String, conversation: String, message: Option<String>) -> Result<types::Target, types::ModuleError> {
            let target = stored(&format!("target:{conversation}"));
            let participant = |pair: &Value| {
                let (id, name) = (pair.get(0)?.as_str()?, pair.get(1)?.as_str()?);
                Some(types::Participant { id: id.to_string(), display_name: name.to_string() })
            };
            let participants = match target.get("participants").and_then(Value::as_array) {
                Some(pairs) => pairs.iter().filter_map(participant).collect(),
                None => vec![types::Participant { id: "p1".to_string(), display_name: "Pat".to_string() }],
            };
            let text = |id: String| target.get("text").and_then(Value::as_str).map_or_else(|| format!("message {id}"), str::to_string);
            Ok(types::Target { conversation_title: format!("Echo {conversation}"), participants, target_text: message.map(text) })
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
