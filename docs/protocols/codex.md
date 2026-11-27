# Codex desktop delivery

Verified on 2026-11-26 with Codex desktop 26.1117.4957.0. This is an experimental
local desktop interface; it is distinct from the documented Codex app-server protocol.

The documented app-server accepts initialize and thread/read for desktop history, but
thread/resume from a separate process returns -32600 when the desktop owns the active
writer. Do not take ownership or edit Codex history to bypass that protection.

The installed desktop exposes local Windows named pipes beginning with
`codex-app-tools-`. Frames contain a little-endian u32 byte length followed by UTF-8
JSON-RPC 2.0. Requests have method, params and id; responses must match the id. The
daemon caps each serialized frame at 8 MiB and never accepts a remote pipe path.

Discovery calls tools/list with threadStartKind all. Required capabilities are
codex_app.send_message_to_thread and codex_app.read_thread. A destination is an explicit
existing thread ID on host local. Multiple matching desktop instances are ambiguous.

tools/call uses namespace codex_app, tool, arguments, callerSource codex, hostId local,
threadId and fresh callId/turnId correlation IDs. The send arguments are threadId,
hostId and prompt; model and thinking are omitted. The send response's success true
means accepted by Codex, not that an agent completed work or replied in a messenger.

read_thread returns success and contentItems, whose inputText item contains JSON
thread metadata. It does not use the external MCP content wrapper. Validate the exact
thread ID and preserve all message content as untrusted data in outgoing prompts.

Synthetic idle and active-conversation deliveries were acknowledged in the same
destination. Controlled endpoint replacement passed. A real desktop restart remains
an opt-in verification check. No real message contents or account/thread IDs are
included in this protocol note.
