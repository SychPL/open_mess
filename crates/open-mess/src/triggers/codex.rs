//! Experimental delivery into an existing Codex desktop conversation. Protocol evidence and
//! compatibility boundary: docs/protocols/codex.md. No agent credentials or history files are read.

#[cfg(any(windows, test))]
use std::time::Duration;

#[cfg(any(windows, test))]
use serde_json::{Value, json};
#[cfg(any(windows, test))]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[cfg(any(windows, test))]
const MAX_FRAME: usize = 8 * 1024 * 1024;
#[cfg(any(windows, test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Fixed, safe failure categories; never carry a response, path, identifier or message text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    #[error("Codex desktop is unavailable; open the application")]
    Offline,
    #[error("more than one Codex desktop instance matches; close the extra instance")]
    Ambiguous,
    #[error("this Codex desktop interface is incompatible")]
    Incompatible,
    #[error("Codex refused the delivery")]
    Rejected,
    #[error("delivery may have reached Codex; it will not be retried automatically")]
    Uncertain,
    #[error("the delivery no longer has permission or its rule changed")]
    Voided,
    #[error("delivery is waiting for its rate limit or update hold")]
    Deferred,
    #[error("Codex desktop delivery is supported only on Windows")]
    Unsupported,
}

/// The existing desktop accepts UUIDs and may introduce other opaque identifier prefixes.
pub fn valid_thread(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

#[cfg(any(windows, test))]
fn identifier() -> Result<String, Failure> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| Failure::Incompatible)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(any(windows, test))]
pub(super) async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    method: &str,
    params: Value,
    before_write: impl FnOnce() -> Result<(), Failure>,
) -> Result<Value, Failure> {
    let id = identifier()?;
    let encoded = serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).map_err(|_| Failure::Incompatible)?;
    if encoded.len() > MAX_FRAME {
        return Err(Failure::Incompatible);
    }
    let length = u32::try_from(encoded.len()).map_err(|_| Failure::Incompatible)?;
    // No await after this check until writing starts. The caller atomically reserves its send here.
    before_write()?;
    stream.write_all(&length.to_le_bytes()).await.map_err(|_| Failure::Uncertain)?;
    stream.write_all(&encoded).await.map_err(|_| Failure::Uncertain)?;
    let length = stream.read_u32_le().await.map_err(|_| Failure::Uncertain)?;
    let length = usize::try_from(length).map_err(|_| Failure::Uncertain)?;
    if length > MAX_FRAME {
        return Err(Failure::Uncertain);
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.map_err(|_| Failure::Uncertain)?;
    let mut response: Value = serde_json::from_slice(&body).map_err(|_| Failure::Uncertain)?;
    if response.get("id").and_then(Value::as_str) != Some(id.as_str()) {
        return Err(Failure::Uncertain);
    }
    if let Some(error) = response.get("error") {
        return Err(if matches!(error.get("code").and_then(Value::as_i64), Some(-32601 | -32602)) { Failure::Rejected } else { Failure::Uncertain });
    }
    response.as_object_mut().and_then(|fields| fields.remove("result")).ok_or(Failure::Uncertain)
}

#[cfg(any(windows, test))]
fn tool_params(thread: &str, tool: &str, arguments: Value) -> Result<Value, Failure> {
    let id = identifier()?;
    Ok(json!({
        "namespace": "codex_app", "tool": tool, "arguments": arguments,
        "callerSource": "codex", "hostId": "local", "threadId": thread,
        "callId": format!("open-mess-{id}"), "turnId": format!("open-mess-{id}"),
    }))
}

#[cfg(any(windows, test))]
fn snapshot(result: &Value, thread: &str) -> Result<bool, Failure> {
    match result.get("success").and_then(Value::as_bool) {
        Some(false) => return Ok(false),
        Some(true) => {}
        None => return Err(Failure::Incompatible),
    }
    let items = result.get("contentItems").and_then(Value::as_array).ok_or(Failure::Incompatible)?;
    if items.is_empty() || items.len() > 16 {
        return Err(Failure::Incompatible);
    }
    let mut found = false;
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("inputText") {
            continue;
        }
        let text = item.get("text").and_then(Value::as_str).ok_or(Failure::Incompatible)?;
        let metadata: Value = serde_json::from_str(text).map_err(|_| Failure::Incompatible)?;
        let id = metadata.pointer("/thread/id").and_then(Value::as_str).ok_or(Failure::Incompatible)?;
        found |= id == thread;
    }
    Ok(found)
}

#[cfg(any(windows, test))]
fn probe_outcome(result: Result<Result<bool, Failure>, tokio::time::error::Elapsed>) -> Result<bool, Failure> {
    match result {
        Ok(Ok(found)) => Ok(found),
        Ok(Err(Failure::Offline)) => Ok(false),
        // A connected but unresolved instance may also own the configured conversation.
        // Never select a different instance while this one's result is unknown.
        _ => Err(Failure::Incompatible),
    }
}

#[cfg(any(windows, test))]
fn capabilities(listed: &Value) -> Result<(), Failure> {
    let Some(tools) = listed.get("tools").and_then(Value::as_array) else { return Err(Failure::Incompatible) };
    for name in ["send_message_to_thread", "read_thread"] {
        if !tools
            .iter()
            .any(|tool| tool.get("namespace").and_then(Value::as_str) == Some("codex_app") && tool.get("name").and_then(Value::as_str) == Some(name))
        {
            return Err(Failure::Incompatible);
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
async fn send_exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    params: Value,
    before_write: impl FnOnce() -> Result<(), Failure>,
) -> Result<(), Failure> {
    let result =
        tokio::time::timeout(REQUEST_TIMEOUT, exchange(stream, "tools/call", params, before_write)).await.map_err(|_| Failure::Uncertain)??;
    match result.get("success").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        Some(false) => Err(Failure::Rejected),
        None => Err(Failure::Uncertain),
    }
}

#[cfg(windows)]
async fn probe(endpoint: &std::path::Path, thread: &str) -> Result<bool, Failure> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let mut pipe = ClientOptions::new().open(endpoint).map_err(|_| Failure::Offline)?;
    let listed = exchange(&mut pipe, "tools/list", json!({"threadStartKind": "all"}), || Ok(())).await?;
    capabilities(&listed)?;
    let params = tool_params(thread, "read_thread", json!({"threadId": thread, "hostId": "local", "includeTurns": false}))?;
    drop(pipe);
    let mut pipe = ClientOptions::new().open(endpoint).map_err(|_| Failure::Offline)?;
    let read = exchange(&mut pipe, "tools/call", params, || Ok(())).await?;
    snapshot(&read, thread)
}

#[cfg(windows)]
async fn locate(thread: &str) -> Result<std::path::PathBuf, Failure> {
    if !valid_thread(thread) {
        return Err(Failure::Rejected);
    }
    let candidates = std::fs::read_dir(r"\\.\pipe\").map_err(|_| Failure::Offline)?;
    let candidates = candidates
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("codex-app-tools-"))
        .map(|entry| std::path::PathBuf::from(r"\\.\pipe\").join(entry.file_name()));
    discover(candidates, |endpoint| async move { probe(&endpoint, thread).await }).await
}

#[cfg(any(windows, test))]
async fn discover<F, Fut>(candidates: impl IntoIterator<Item = std::path::PathBuf>, mut probe: F) -> Result<std::path::PathBuf, Failure>
where
    F: FnMut(std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<bool, Failure>>,
{
    let started = tokio::time::Instant::now();
    let mut found = None;
    let mut count = 0;
    for endpoint in candidates {
        count += 1;
        if count > 32 {
            return Err(Failure::Ambiguous);
        }
        let remaining = Duration::from_secs(10).saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(Failure::Incompatible);
        }
        if probe_outcome(tokio::time::timeout(remaining.min(Duration::from_secs(2)), probe(endpoint.clone())).await)? {
            if found.is_some() {
                return Err(Failure::Ambiguous);
            }
            found = Some(endpoint);
        }
    }
    found.ok_or(Failure::Offline)
}

/// Read-only capability and exact-destination check, exposed only to the configuration window.
pub async fn check(thread: &str) -> Result<(), Failure> {
    if !valid_thread(thread) {
        return Err(Failure::Rejected);
    }
    #[cfg(windows)]
    {
        locate(thread).await.map(drop)
    }
    #[cfg(not(windows))]
    {
        Err(Failure::Unsupported)
    }
}

/// Acknowledgement means accepted by Codex; this does not wait for the model or send a reply.
pub async fn send(thread: &str, prompt: &str, before_write: impl FnOnce() -> Result<(), Failure>) -> Result<(), Failure> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let endpoint = locate(thread).await?;
        let params = tool_params(thread, "send_message_to_thread", json!({"threadId": thread, "hostId": "local", "prompt": prompt}))?;
        let mut pipe = ClientOptions::new().open(endpoint).map_err(|_| Failure::Offline)?;
        send_exchange(&mut pipe, params, before_write).await
    }
    #[cfg(not(windows))]
    {
        let _ = (thread, prompt, before_write);
        Err(Failure::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn answer(mut peer: tokio::io::DuplexStream, wrong_id: bool) {
        let size = peer.read_u32_le().await.unwrap();
        let mut body = vec![0; size as usize];
        peer.read_exact(&mut body).await.unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        let id = if wrong_id { json!("wrong") } else { request.get("id").unwrap().clone() };
        let response = serde_json::to_vec(&json!({"id": id, "result": {"success": true}})).unwrap();
        let length = (response.len() as u32).to_le_bytes();
        for byte in length {
            peer.write_all(&[byte]).await.unwrap();
        }
        for part in response.chunks(3) {
            peer.write_all(part).await.unwrap();
        }
    }

    #[tokio::test]
    async fn fragmented_reply_and_matching_id() {
        let (mut client, peer) = duplex(1024);
        let server = tokio::spawn(answer(peer, false));
        let result = exchange(&mut client, "tools/call", json!({}), || Ok(())).await.unwrap();
        assert_eq!(result, json!({"success": true}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn mismatched_id_is_uncertain() {
        let (mut client, peer) = duplex(1024);
        let server = tokio::spawn(answer(peer, true));
        assert_eq!(exchange(&mut client, "tools/call", json!({}), || Ok(())).await, Err(Failure::Uncertain));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn final_authorization_refusal_writes_nothing() {
        let (mut client, mut peer) = duplex(1024);
        assert_eq!(exchange(&mut client, "tools/call", json!({}), || Err(Failure::Voided)).await, Err(Failure::Voided));
        drop(client);
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
    }

    async fn request(peer: &mut tokio::io::DuplexStream) -> Value {
        let length = peer.read_u32_le().await.unwrap();
        let mut body = vec![0; length as usize];
        peer.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn malformed_oversized_and_disconnected_replies_are_uncertain() {
        for case in ["json", "missing_result", "oversized", "partial_header", "partial_body"] {
            let (mut client, mut peer) = duplex(1024);
            let server = tokio::spawn(async move {
                let incoming = request(&mut peer).await;
                match case {
                    "oversized" => {
                        // A complete valid response would succeed if the frame guard disappeared.
                        let mut body = serde_json::to_vec(&json!({"id": incoming["id"], "result": {"success": true}})).unwrap();
                        body.resize(MAX_FRAME + 1, b' ');
                        peer.write_u32_le(body.len() as u32).await.unwrap();
                        assert!(peer.write_all(&body).await.is_err(), "the rejected oversized body must not be consumed");
                    }
                    "partial_header" => peer.write_all(&[1, 2]).await.unwrap(),
                    "partial_body" => {
                        peer.write_u32_le(20).await.unwrap();
                        peer.write_all(b"{").await.unwrap();
                    }
                    _ => {
                        let body =
                            if case == "json" { b"invalid JSON".to_vec() } else { serde_json::to_vec(&json!({"id": incoming["id"]})).unwrap() };
                        peer.write_u32_le(body.len() as u32).await.unwrap();
                        peer.write_all(&body).await.unwrap();
                    }
                }
            });
            let result = exchange(&mut client, "tools/call", json!({}), || Ok(())).await;
            drop(client);
            assert_eq!(result, Err(Failure::Uncertain), "{case}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn serialized_unicode_limit_refuses_before_authorization_or_write() {
        let (mut client, mut peer) = duplex(1024);
        let mut authorized = false;
        let result = exchange(&mut client, "tools/call", json!({"prompt": "😀".repeat(MAX_FRAME / 4 + 1)}), || {
            authorized = true;
            Ok(())
        })
        .await;
        assert_eq!(result, Err(Failure::Incompatible));
        assert!(!authorized);
        drop(client);
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn send_acknowledgement_and_deadline_keep_uncertain_distinct_from_refusal() {
        for (response, expected) in
            [(json!({"success": true}), Ok(())), (json!({"success": false}), Err(Failure::Rejected)), (json!({}), Err(Failure::Uncertain))]
        {
            let (mut client, mut peer) = duplex(1024);
            let server = tokio::spawn(async move {
                let incoming = request(&mut peer).await;
                let body = serde_json::to_vec(&json!({"id": incoming["id"], "result": response})).unwrap();
                peer.write_u32_le(body.len() as u32).await.unwrap();
                peer.write_all(&body).await.unwrap();
            });
            assert_eq!(send_exchange(&mut client, json!({}), || Ok(())).await, expected);
            server.await.unwrap();
        }
        let (mut client, mut peer) = duplex(1024);
        let server = tokio::spawn(async move {
            request(&mut peer).await;
            std::future::pending::<()>().await;
        });
        assert_eq!(send_exchange(&mut client, json!({}), || Ok(())).await, Err(Failure::Uncertain));
        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_requires_capabilities_and_refuses_two_matching_instances() {
        let supported = json!({"tools": [
            {"namespace": "codex_app", "name": "send_message_to_thread"},
            {"namespace": "codex_app", "name": "read_thread"}
        ]});
        assert_eq!(capabilities(&supported), Ok(()));
        for listed in [
            json!({}),
            json!({"tools": []}),
            json!({"tools": [{"namespace":"codex_app","name":"read_thread"}]}),
            json!({"tools": [{"namespace":"another","name":"send_message_to_thread"}]}),
        ] {
            assert_eq!(capabilities(&listed), Err(Failure::Incompatible));
        }
        let candidates = vec![std::path::PathBuf::from("one"), std::path::PathBuf::from("two")];
        assert_eq!(discover(candidates.clone(), |_| std::future::ready(Ok(true))).await, Err(Failure::Ambiguous));
        assert_eq!(
            discover(candidates, |path| std::future::ready(if path == std::path::Path::new("one") { Ok(true) } else { Err(Failure::Incompatible) }))
                .await,
            Err(Failure::Incompatible)
        );
        let mut probed = 0;
        let candidates = (0..33).map(|n| std::path::PathBuf::from(n.to_string()));
        assert_eq!(
            discover(candidates, |_| {
                probed += 1;
                std::future::ready(Ok(false))
            })
            .await,
            Err(Failure::Ambiguous)
        );
        assert_eq!(probed, 32);
    }

    #[test]
    fn validates_thread_and_actual_desktop_envelope() {
        assert!(valid_thread("01a1ee4b-0000-0000-0000-000000000000"));
        for bad in ["", "../x", "a b", "😀"] {
            assert!(!valid_thread(bad));
        }
        assert!(!valid_thread(&"a".repeat(129)));
        for length in [1, 127, 128] {
            assert!(valid_thread(&"a".repeat(length)));
        }
        let result = json!({"success": true, "contentItems": [{"type": "inputText", "text": "{\"thread\":{\"id\":\"thread_test\"}}"}]});
        assert_eq!(snapshot(&result, "thread_test"), Ok(true));
        assert_eq!(snapshot(&result, "different"), Ok(false));
        assert_eq!(snapshot(&json!({"success": true, "content": []}), "thread_test"), Err(Failure::Incompatible));
        let params = tool_params("thread_test", "send_message_to_thread", json!({"threadId": "thread_test", "prompt": "test"})).unwrap();
        assert_eq!(params.pointer("/arguments/threadId"), Some(&json!("thread_test")));
        assert!(params.pointer("/arguments/model").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn broken_or_unresolved_probe_is_not_offline() {
        assert_eq!(probe_outcome(Ok(Err(Failure::Uncertain))), Err(Failure::Incompatible));
        assert_eq!(probe_outcome(Ok(Err(Failure::Offline))), Ok(false));
        let elapsed = tokio::time::timeout(std::time::Duration::from_secs(1), std::future::pending::<Result<bool, Failure>>()).await;
        assert_eq!(probe_outcome(elapsed), Err(Failure::Incompatible));
        let malformed = json!({"success":true,"contentItems":[{"type":"inputText","text":"invalid JSON"}]});
        assert_eq!(snapshot(&malformed, "thread_test"), Err(Failure::Incompatible));
    }
}
