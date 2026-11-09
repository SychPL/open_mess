//! A test guest for `crates/open-mess-host`: each tool exercises one host import and reports what
//! came back, and the event exports forward what they receive to `host.log`.
//!
//! Tools (`call`): `echo` returns its arguments; `fetch` GETs `{"url"}` and returns `{"status"}`
//! or `{"error"}` (the `http-error` case name); `ws` opens a WebSocket to `{"url"}`, sends "ping"
//! and returns `{"conn"}` or `{"error"}`; `store` sets key "k" to `{"size"}` bytes and returns the
//! length read back (`{"len"}`, plus `{"error"}` when the set failed); `timer` sets timer 7 to
//! fire after 100 ms; `timer-at` the same for timer `{"id"}` (timer 8 is busy for 1 s and then logs "timer 8 done"); `message` reports account "a" online and a message `{"text"}` from "p1" in
//! conversation "c" that mentions the user; `sign-in` shows a device code and reports what
//! `open-page` and `wam-token` answered; `trap` crashes the module; `ws-burst` opens a socket to
//! `{"url"}`, stays busy for 500 ms, then sends 40 frames and returns `{"sent"}`. The shared tools
//! `read_messages` and `unread` return one synthetic message or conversation that names the
//! account; `list_conversations` returns the group chats "c1" and "c2". The writes `send`,
//! `edit_message` and `react` append their tool and arguments to the storage key "writes" (the
//! tool `writes` returns them) and succeed, except a `text` of "uncertain" (the module error), "fail"
//! (a platform error) or "slow" (recorded, then busy until the call limit). `describe-target`
//! returns the target the tool `set-target` stored for `{"conversation"}` (`{"participants"}` as
//! `[id, name]` pairs, `{"text"}` for any message, `{"title"}`), else participant "p1" (Pat),
//! "message <id>" and the title "Echo <conversation>"; for conversation "slow" it is busy for 1 s
//! first.
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
//! named after any other. `wam-token` asks WAM for `{"scopes"}` and returns `{"token"}` or
//! `{"error"}`. `rtc-open` opens a media connection with the data channel `{"label"}` and returns
//! `{"conn", "offer"}`; `rtc-accept` gives connection `{"conn"}` the answer `{"answer"}` and returns
//! `{}`; both return `{"error", "why"}` instead when refused, and `rtc-close` closes `{"conn"}`. The
//! manifest's media range is 52.112.0.0/14; data channel batches and states go to `host.log`. The
//! `end-sessions` and `restore` exports append their account to the storage keys "ended" and
//! "restored", which the tool `ended` returns as `{"ended", "restored"}`; `end-sessions` of account
//! "slow" is busy for 2 s first.
//!
//! Extra tools (the manifest's, for the core's registry): `peek` reads; it returns two items, one in
//! `{"conversation"}` (else "c1") and one in "c2", one item without a conversation and some
//! untrusted text; `{"answer": "big"}` returns a result over 1 MiB, `{"answer": "list"}` a JSON
//! array and `{"answer": "slow"}` the items after 1 s. Each call counts in the tool `peeks`. `poke`
//! writes to its required `{"conversation"}` as `send` does (recorded under "writes", `{"text"}`
//! picks the outcome). The others are never exposed: `nudge` writes without naming a conversation,
//! `Bad-Name` breaks the name rule, `with_account` declares `account`, `loose` takes any property,
//! and `twin` is declared twice.
//!
//! The Teams module's captions tools, faked for the core's captions archive (spec
//! 2026-11-04-captions-archive): `list_calls` returns what `set-calls` stored for the account
//! (`{"items"}`, or `{"error"}` to fail); `join_captions` records its arguments (the tool
//! `captions-log` returns them with the leaves) and starts a session in `{"conversation"}` for
//! `{"call"}`, live at once, unless `set-join` stored `{"error"}` (the join fails with that text),
//! `{"state"}` (the session starts in it) or `{"slow": true}` (busy for 1 s first); a session of the
//! same call going on answers `done`, one of another call in the conversation ends first
//! (`meeting_ended`). `read_captions` pages a session (by `{"session"}`, else the conversation's
//! latest) from `{"cursor"}`, 100 cues at most, with `skipped`, and `leave_captions` ends it (`left`). `captions-say`
//! changes a session: it adds `{"cues"}`, drops the `{"drop"}` oldest, and sets `state`, `reason`,
//! `captions_on`, `captions_seen`, `threads` and `missed` when given; the session is then found by
//! each of its `threads` too; with `{"forget": true}` its reads fail, and with `{"fail_reads": n}`
//! its next n reads. `end-sessions` ends the account's sessions (`update`).

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
    use bindings::open_mess::module::{host, http, rtc, sign_in, storage, types, websocket};
    use serde_json::{Value, json};

    struct Echo;

    /// The manifest's hosts, padded so the core's tests can add one by patching the built file
    /// (a rebuild with another manifest under the same name).
    const HOSTS: &str = "localhost                               ";
    /// The manifest's version, padded the same way so the update tests can change only the version.
    const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "                ");
    /// The Windows account identity: the Teams desktop client id and the chat service's scope
    /// (docs/protocols/teams.md, "Live results" item 1), for the manual check of WAM.
    const TEAMS_CLIENT: &str = "1fec8e78-bce4-4aaf-ab1b-5451cc387264";
    const CHAT_SCOPE: &str = "https://ic3.teams.office.com/.default";
    /// A scope pattern, for the host's tests of WAM scope patterns; its hosts are in the manifest.
    const SHAREPOINT_SCOPE: &str = "https://*.sharepoint.com/.default";

    fn http_error(e: &http::HttpError) -> &'static str {
        match e {
            http::HttpError::Blocked(_) => "blocked",
            http::HttpError::Timeout => "timeout",
            http::HttpError::TooLarge => "too-large",
            http::HttpError::Throttled(_) => "throttled",
            http::HttpError::Network(_) => "network",
        }
    }

    fn http_why(e: &http::HttpError) -> String {
        match e {
            http::HttpError::Blocked(why) | http::HttpError::Network(why) => why.clone(),
            _ => String::new(),
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
            "timer-at" => {
                host::set_timer(args.get("id").and_then(Value::as_u64).and_then(|id| u32::try_from(id).ok()).unwrap_or_default(), 100);
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
                    "wam-token": describe(sign_in::wam_token(&["scope".to_string()], true, None, None)),
                })
            }
            "sign-in-with" => {
                storage::set("sign-in-with", text("how").as_bytes()).map_err(|_| types::ModuleError::Platform("storage".to_string()))?;
                json!({})
            }
            "wam-token" => {
                let scopes: Vec<String> =
                    args.get("scopes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
                match sign_in::wam_token(&scopes, false, None, None) {
                    Ok(token) => json!({ "token": token }),
                    Err(e) => json!({ "error": sign_in_error(&e) }),
                }
            }
            "peeks" => json!({ "count": stored("peeks").as_u64().unwrap_or_default() }),
            "peek" => {
                let count = stored("peeks").as_u64().unwrap_or_default() + 1;
                storage::set("peeks", count.to_string().as_bytes()).map_err(|_| types::ModuleError::Platform("storage".to_string()))?;
                if text("answer") == "slow" {
                    let start = std::time::Instant::now();
                    while start.elapsed() < std::time::Duration::from_secs(1) {
                        std::hint::spin_loop();
                    }
                }
                let conversation = Some(text("conversation")).filter(|c| !c.is_empty()).unwrap_or_else(|| "c1".to_string());
                match text("answer").as_str() {
                    "big" => json!({ "items": [], "untrusted": { "text": "x".repeat(1024 * 1024 + 1) } }),
                    "list" => json!([1, 2]),
                    _ => json!({
                        "items": [{ "conversation": conversation }, { "conversation": "c2" }, { "note": "no conversation" }],
                        "untrusted": { "text": format!("peeked at {account}") },
                    }),
                }
            }
            "rtc-open" => match rtc::open(&text("label")) {
                Ok((conn, offer)) => json!({ "conn": conn, "offer": offer }),
                Err(e) => json!({ "error": http_error(&e), "why": http_why(&e) }),
            },
            "rtc-accept" => {
                let conn = args.get("conn").and_then(Value::as_u64).and_then(|c| u32::try_from(c).ok()).unwrap_or_default();
                match rtc::accept(conn, &text("answer")) {
                    Ok(()) => json!({}),
                    Err(e) => json!({ "error": http_error(&e), "why": http_why(&e) }),
                }
            }
            "rtc-close" => {
                rtc::close(args.get("conn").and_then(Value::as_u64).and_then(|c| u32::try_from(c).ok()).unwrap_or_default());
                json!({})
            }
            "ended" => json!({ "ended": stored("ended"), "restored": stored("restored") }),
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
            "send" | "edit_message" | "react" | "poke" => {
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
            "list_calls" => {
                let calls = stored(&format!("calls:{account}"));
                if let Some(error) = calls.get("error").and_then(Value::as_str) {
                    return Err(types::ModuleError::Platform(error.to_string()));
                }
                json!({ "items": calls.get("items").cloned().unwrap_or_else(|| json!([])) })
            }
            "set-calls" => {
                put(&format!("calls:{account}"), &args)?;
                json!({})
            }
            "set-join" => {
                put(&format!("join:{account}"), &args)?;
                json!({})
            }
            "join_captions" => captions::join(account, &args)?,
            "read_captions" => captions::read(account, &args)?,
            "leave_captions" => captions::leave(account, &args)?,
            "captions-say" => captions::say(account, &args)?,
            "captions-log" => json!({ "joins": stored("joins"), "leaves": stored("leaves") }),
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

    /// The manifest's own tools: `peek`, `poke`, and the ones the core never exposes.
    fn extra_tools() -> Vec<types::ToolDecl> {
        let string = json!({"type": "string"});
        let schema = |properties: Value| json!({"type": "object", "properties": properties, "additionalProperties": false}).to_string();
        let tool = |name: &str, input_schema: String, writes: bool| types::ToolDecl {
            name: name.to_string(),
            description: format!("test-echo's {name}"),
            input_schema,
            writes,
        };
        let peek = schema(json!({"conversation": string, "answer": string}));
        let poke = json!({
            "type": "object",
            "properties": {"conversation": string, "text": string},
            "required": ["conversation"],
            "additionalProperties": false,
        });
        let poke = types::ToolDecl {
            description: "Pokes the conversation. It records its arguments as a write.".to_string(),
            ..tool("poke", poke.to_string(), true)
        };
        let with = |properties: Value, required: &[&str]| {
            json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false}).to_string()
        };
        let number = json!({"type": "integer"});
        let join = with(
            json!({"conversation": string, "call": string, "language": string, "start_captions": {"type": "boolean"}}),
            &["conversation", "call"],
        );
        let read = with(json!({"conversation": string, "session": string, "cursor": number, "max_bytes": number}), &["conversation"]);
        vec![
            tool("list_calls", schema(json!({})), false),
            tool("join_captions", join, true),
            tool("read_captions", read, false),
            tool("leave_captions", with(json!({"conversation": string}), &["conversation"]), false),
            tool("peek", peek.clone(), false),
            poke,
            tool("nudge", peek.clone(), true),
            tool("Bad-Name", peek.clone(), false),
            tool("with_account", schema(json!({"account": string})), false),
            tool("loose", json!({"type": "object", "properties": {}, "additionalProperties": true}).to_string(), false),
            tool("twin", peek.clone(), false),
            tool("twin", peek, false),
        ]
    }

    /// Stores `value` as JSON under `key`.
    fn put(key: &str, value: &Value) -> Result<(), types::ModuleError> {
        storage::set(key, value.to_string().as_bytes()).map_err(|_| types::ModuleError::Platform("storage".to_string()))
    }

    /// The fake captions sessions, in storage: `session:<token>`, and the latest of a conversation
    /// and of a call under `conversation:<account>:<id>` and `call:<account>:<id>`.
    mod captions {
        use serde_json::{Value, json};

        use super::bindings::open_mess::module::types;
        use super::{put, stored};

        /// The most cues of a page.
        const PAGE: usize = 100;

        fn text<'a>(args: &'a Value, key: &str) -> &'a str {
            args.get(key).and_then(Value::as_str).unwrap_or_default()
        }

        fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
            value.get(key).unwrap_or(&Value::Null)
        }

        fn set(value: &mut Value, key: &str, to: Value) {
            if let Some(map) = value.as_object_mut() {
                map.insert(key.to_string(), to);
            }
        }

        fn active(session: &Value) -> bool {
            matches!(get(session, "state").as_str(), Some("joining" | "live"))
        }

        fn end(token: &str, reason: &str) -> Result<(), types::ModuleError> {
            let mut session = stored(&format!("session:{token}"));
            if active(&session) {
                set(&mut session, "state", json!("ended"));
                set(&mut session, "reason", json!(reason));
                put(&format!("session:{token}"), &session)?;
            }
            Ok(())
        }

        fn push(key: &str, item: Value) -> Result<(), types::ModuleError> {
            let mut list = stored(key).as_array().cloned().unwrap_or_default();
            list.push(item);
            put(key, &Value::from(list))
        }

        pub fn join(account: &str, args: &Value) -> Result<Value, types::ModuleError> {
            let (conversation, call) = (text(args, "conversation"), text(args, "call"));
            if call.is_empty() {
                return Err(types::ModuleError::InvalidArgs("call is required".to_string()));
            }
            push("joins", json!({"account": account, "args": args}))?;
            let outcome = stored(&format!("join:{account}"));
            if get(&outcome, "slow") == &json!(true) {
                let start = std::time::Instant::now();
                while start.elapsed() < std::time::Duration::from_secs(1) {
                    std::hint::spin_loop();
                }
            }
            if let Some(error) = get(&outcome, "error").as_str() {
                return Err(types::ModuleError::Platform(error.to_string()));
            }
            let start_captions = args.get("start_captions").and_then(Value::as_bool).unwrap_or(true);
            if let Some(token) = stored(&format!("call:{account}:{call}")).as_str() {
                let mut session = stored(&format!("session:{token}"));
                if active(&session) {
                    if start_captions {
                        set(&mut session, "start_captions", json!(true));
                        put(&format!("session:{token}"), &session)?;
                    }
                    return Ok(json!({"state": "done"}));
                }
            }
            if let Some(token) = stored(&format!("conversation:{account}:{conversation}")).as_str() {
                end(token, "meeting_ended")?;
            }
            let count = stored("tokens").as_u64().unwrap_or_default() + 1;
            put("tokens", &json!(count))?;
            let token = format!("s{count}");
            let session = json!({
                "account": account, "conversation": conversation, "call": call,
                "state": get(&outcome, "state").as_str().unwrap_or("live"), "reason": null,
                "captions_on": true, "captions_seen": false, "threads": null, "start_captions": start_captions,
                "cues": [], "first": 0, "missed": 0,
            });
            put(&format!("session:{token}"), &session)?;
            put(&format!("call:{account}:{call}"), &json!(token))?;
            put(&format!("conversation:{account}:{conversation}"), &json!(token))?;
            Ok(json!({"state": "joining"}))
        }

        fn token(account: &str, args: &Value) -> Option<String> {
            match args.get("session").and_then(Value::as_str) {
                Some(token) => Some(token.to_string()),
                None => stored(&format!("conversation:{account}:{}", text(args, "conversation"))).as_str().map(str::to_string),
            }
        }

        pub fn read(account: &str, args: &Value) -> Result<Value, types::ModuleError> {
            let Some(token) = token(account, args) else {
                return Ok(json!({"state": "not_joined", "session": null, "next_cursor": 0, "missed": 0, "untrusted": {"cues": []}}));
            };
            let mut session = stored(&format!("session:{token}"));
            if session.is_null() {
                return Err(types::ModuleError::InvalidArgs("the captions session changed".to_string()));
            }
            // Reads that fail for a while, the session going on.
            let failing = get(&session, "fail_reads").as_u64().unwrap_or_default();
            if failing > 0 {
                set(&mut session, "fail_reads", json!(failing - 1));
                put(&format!("session:{token}"), &session)?;
                return Err(types::ModuleError::Platform("busy".to_string()));
            }
            let first = get(&session, "first").as_u64().unwrap_or_default();
            let cursor = args.get("cursor").and_then(Value::as_u64);
            let start = cursor.unwrap_or(0).max(first);
            let cues: Vec<Value> = get(&session, "cues").as_array().cloned().unwrap_or_default();
            let page: Vec<Value> = cues.into_iter().skip(usize::try_from(start - first).unwrap_or(usize::MAX)).take(PAGE).collect();
            let mut result = json!({
                "state": get(&session, "state"), "session": token, "call": get(&session, "call"),
                "captions_on": get(&session, "captions_on"), "captions_seen": get(&session, "captions_seen"),
                "next_cursor": start + page.len() as u64, "missed": get(&session, "missed"),
                "skipped": cursor.map_or(0, |cursor| first.saturating_sub(cursor)), "reason": get(&session, "reason"),
                "untrusted": {"cues": page},
            });
            if get(&session, "threads").is_array() {
                set(&mut result, "threads", get(&session, "threads").clone());
            }
            Ok(result)
        }

        pub fn leave(account: &str, args: &Value) -> Result<Value, types::ModuleError> {
            push("leaves", json!({"account": account, "conversation": text(args, "conversation")}))?;
            if let Some(token) = token(account, args) {
                end(&token, "left")?;
            }
            Ok(json!({"state": "ended"}))
        }

        pub fn say(account: &str, args: &Value) -> Result<Value, types::ModuleError> {
            let Some(token) = token(account, args) else { return Err(types::ModuleError::Platform("no session".to_string())) };
            let mut session = stored(&format!("session:{token}"));
            let mut cues = get(&session, "cues").as_array().cloned().unwrap_or_default();
            cues.extend(args.get("cues").and_then(Value::as_array).cloned().unwrap_or_default());
            let drop = usize::try_from(args.get("drop").and_then(Value::as_u64).unwrap_or_default()).unwrap_or_default().min(cues.len());
            cues.drain(..drop);
            let first = get(&session, "first").as_u64().unwrap_or_default() + drop as u64;
            set(&mut session, "first", json!(first));
            set(&mut session, "cues", Value::from(cues));
            for key in ["state", "reason", "captions_on", "captions_seen", "threads", "missed", "fail_reads"] {
                if let Some(value) = args.get(key) {
                    set(&mut session, key, value.clone());
                }
            }
            // A session is found by each of its threads, as by a call that gained a chat.
            for thread in args.get("threads").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                put(&format!("conversation:{account}:{thread}"), &json!(token))?;
            }
            // Forgotten, as by a module that lost track of it: reads of it fail from now on.
            let forget = args.get("forget") == Some(&json!(true));
            put(&format!("session:{token}"), if forget { &Value::Null } else { &session })?;
            Ok(json!({"session": token}))
        }

        /// The `end-sessions` export: what others see of the account's sessions ends.
        pub fn end_all(account: &str) {
            for count in 1..=stored("tokens").as_u64().unwrap_or_default() {
                let token = format!("s{count}");
                if get(&stored(&format!("session:{token}")), "account") == &json!(account) {
                    let _ = end(&token, "update");
                }
            }
        }
    }

    /// Appends `account` to the list stored under `key`.
    fn remember(key: &str, account: &str) {
        let mut list: Vec<Value> = stored(key).as_array().cloned().unwrap_or_default();
        list.push(Value::from(account));
        storage::set(key, Value::from(list).to_string().as_bytes()).ok();
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
                version: std::hint::black_box(VERSION).trim_end().to_string(),
                platform: "test".to_string(),
                // black_box: the padded string stays in the file as written.
                hosts: std::hint::black_box(HOSTS).split_whitespace().chain(["*.sharepoint.com"]).map(str::to_string).collect(),
                sign_in_hosts: vec!["login.example.com".to_string(), "login.microsoftonline.com".to_string()],
                wam: Some(types::WamIdentity {
                    client_id: TEAMS_CLIENT.to_string(),
                    scopes: vec![CHAT_SCOPE.to_string(), SHAREPOINT_SCOPE.to_string()],
                }),
                extra_tools: extra_tools(),
                media: Some(types::MediaAccess { peers: vec!["52.112.0.0/14".to_string()] }),
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
                    sign_in::wam_token(&[CHAT_SCOPE.to_string()], false, None, None).map_err(|e| types::ModuleError::Platform(sign_in_error(&e)))?;
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
            remember("restored", &account);
            if account == "known" { Ok(()) } else { Err(types::ModuleError::AuthRequired(account)) }
        }

        fn sign_out(account: String) {
            host::session(&account, host::SessionState::Offline);
        }

        fn end_sessions(account: String) {
            if account == "slow" {
                let start = std::time::Instant::now();
                while start.elapsed() < std::time::Duration::from_secs(2) {
                    std::hint::spin_loop();
                }
            }
            captions::end_all(&account);
            remember("ended", &account);
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
            if conversation == "slow" {
                let start = std::time::Instant::now();
                while start.elapsed() < std::time::Duration::from_secs(1) {
                    std::hint::spin_loop();
                }
            }
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
            let title = target.get("title").and_then(Value::as_str).map_or_else(|| format!("Echo {conversation}"), str::to_string);
            Ok(types::Target { conversation_title: title, participants, target_text: message.map(text) })
        }
    }

    impl events::Guest for Echo {
        fn on_ws_message(conn: u32, text: String) {
            host::log(2, &format!("ws {conn} message {text}"));
        }

        fn on_ws_closed(conn: u32, reason: String) {
            host::log(2, &format!("ws {conn} closed {reason}"));
        }

        fn on_rtc_data(conn: u32, messages: Vec<Vec<u8>>, dropped: u32) {
            let texts: Vec<String> = messages.iter().map(|m| String::from_utf8_lossy(m).into_owned()).collect();
            host::log(2, &format!("rtc {conn} data {} dropped {dropped}: {}", messages.len(), texts.join(",")));
        }

        fn on_rtc_state(conn: u32, state: String) {
            host::log(2, &format!("rtc {conn} state {state}"));
        }

        fn on_timer(id: u32) {
            host::log(2, &format!("timer {id}"));
            if id == 8 {
                let start = std::time::Instant::now();
                while start.elapsed() < std::time::Duration::from_secs(1) {
                    std::hint::spin_loop();
                }
                host::log(2, "timer 8 done");
            }
        }
    }
}
