# Milestone C: permissions and approvals - implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the core enforces agent permissions on every read and write tool, the write tools `send`, `edit_message`, `react` and `pending` exist, and writes that need confirmation wait for the user's approval in a Windows notification that no agent can answer.

**Architecture:** `crates/open-mess` gains `permissions.rs` (the permission table and its checks), `pending.rs` (the pending-action store and executor) and `approval.rs` (Windows toasts with buttons, one at a time, plus a native dialog for long content). Write tools never call the module directly: they create a pending action; the executor runs it immediately (autonomous), after approval (confirmation) or never (refused). Before showing and before executing, the core asks the module to describe the target; an execution only happens if the target still matches what the user approved.

**Tech Stack:** what milestones A, B1, B2 use; `tauri-winrt-notification` (toasts with buttons and in-process activation), `rfd` (native message dialog), `winreg` (registering the toast AppUserModelID under HKCU).

**Spec:** `.specs/2026-10-26-open-mess-design.md` 8.5 (write tools, `pending`, uncertain results), 8.6 (whole section), 8.7 (trigger agents use the same permissions), 15. **Depends on:** milestone B2 (accounts, module routing, shared tool schemas, `untrusted` marking).

## Global Constraints

- One set of rules for every caller: MCP, CLI, agents started by triggers. No front end can change permissions, approve or reject an action (spec 8.6); `permissions::set` and `pending::decide` are called only by the approval UI here and the configuration window (milestone E), and by tests.
- Permissions per account with optional per-conversation overrides: `read: bool` (default `true`), `write: never | confirm | autonomous` (default `confirm` for a new account) (spec 8.6).
- Read tools filter out conversations the account or conversation permission does not allow to read; a direct `read_messages` / `get_attachment` on such a conversation is a tool error `reading this conversation is not allowed`.
- Every write (send, edit, reaction) is a pending action stored in SQLite in all its states: `waiting`, `approved`, `done`, `rejected`, `expired`, `voided`, `uncertain`, `failed`; kept 7 days, then deleted (spec 8.4, R9).
- `confirm`: the action waits at most `APPROVAL_TTL = 10 min`, then `expired`. At most `MAX_WAITING = 20` waiting actions per account; more is a tool error `too many actions waiting for approval` (spec 8.6, R5).
- The approval shows the exact account, conversation title, participants (direct and group chats), full text (for edits: before and after; for reactions: which and under which message). Approval binds to a fingerprint of what was shown: SHA-256 of the sorted participant ids plus the target message's current content (edits, reactions, replies). Before executing, the core describes the target again; a different fingerprint makes the action `voided` (spec 8.6, R5/R6/R7). Channels: participants are not compared.
- Toasts: one action at a time on screen; the next appears after a decision or expiry, so a burst never piles up notifications to approve blindly (spec 8.6, R5/R7). A toast shows at most 280 characters of text; longer content gets only a `Show all` button that opens a modal native dialog with the full content and `Approve` / `Reject` buttons. No toast or dialog approves more than one action.
- An autonomous write and an approved write run with the module call limits; a timeout or connection loss after the request may have reached the platform is `uncertain`, never retried automatically (spec 8.5, R7).
- No token, message text or participant name is ever logged.

## Review Focus

- An agent calls `send` 25 times in a loop on a `confirm` account: 20 actions wait, calls 21-25 fail with the "too many" error, and exactly one toast is on screen (Task 3 tests `waiting_cap_per_account`, `one_toast_at_a_time`).
- Someone joins the group chat between the toast and the click: the action becomes `voided`, nothing is sent (Task 2 test `changed_participants_void_the_action`).
- The author edits the message after a reaction was approved but before it runs: `voided` (Task 2 test `changed_target_text_voids_a_reaction`).
- The module times out after posting: `uncertain`, no second post, `pending` reports it (Task 2 test `timeout_after_send_is_uncertain`).
- An account with `read: false`: `unread` and `list_conversations` without `account` skip it silently with no `errors` entry, and `read_messages` on it fails with the not-allowed error (Task 1 test `unreadable_account_is_hidden`).

---

### Task 1: Permission table and read checks

**Files:** Create `crates/open-mess/src/permissions.rs`; modify `store.rs` (migration: `permissions(account TEXT NOT NULL, conversation TEXT NOT NULL DEFAULT '', read INTEGER NOT NULL, write TEXT NOT NULL, PRIMARY KEY (account, conversation))`), `tools/mod.rs`.

**Interfaces:**
- Produces: `Write::{Never, Confirm, Autonomous}`; `permissions::effective(store: &Store, account: &str, conversation: Option<&str>) -> Result<Effective { read: bool, write: Write }, Error>` (conversation row overrides the account row, the account row overrides the defaults); `permissions::set(store, account, conversation: Option<&str>, read: bool, write: Write) -> Result<(), Error>`.
- [ ] **Step 1: Failing tests:** `defaults_are_read_and_confirm`, `conversation_overrides_account`, `unreadable_account_is_hidden`, `unreadable_conversation_is_filtered_from_lists`.
- [ ] **Step 2-4:** Run (FAIL), implement (read tools from B2 consult `effective` before calling and filter results), run (PASS).
- [ ] **Step 5:** Commit `Enforce read permissions on the shared tools`.

### Task 2: Pending actions, target description and execution

**Files:** Create `crates/open-mess/src/pending.rs`; modify `store.rs` (migration: `pending(id TEXT PRIMARY KEY, account TEXT NOT NULL, conversation TEXT NOT NULL, kind TEXT NOT NULL, args TEXT NOT NULL, fingerprint TEXT NOT NULL, state TEXT NOT NULL, created_ms INTEGER NOT NULL, decided_ms INTEGER, detail TEXT)`), `wit/module.wit`, `modules/teams` and the test guests.

**Interfaces:**
- `wit/module.wit` additions: in `types`, `record target { conversation-title: string, participants: list<string>, target-text: option<string> }` and `module-error::uncertain(string)`; in `module`, `describe-target: func(account: string, conversation: string, message: option<string>) -> result<target, module-error>`. `modules/teams` returns `not-supported` for it until F2; test-echo returns a target from its storage so tests can change it.
- Produces: `pending::create(kind: Kind /* Send, Edit, React */, account, conversation, args: Value) -> Result<ActionId, ToolError>` (describes the target, computes the fingerprint, stores `waiting` or runs at once for `autonomous`); `pending::decide(id: &ActionId, approve: bool) -> Result<State, Error>`; `pending::execute(id)` (describe again, compare fingerprint -> `voided` or call the module tool; map `module-error::uncertain` and post-send timeouts to `uncertain`); expiry sweep every 30 s; retention sweep daily.
- [ ] **Step 1: Failing tests:** `autonomous_runs_immediately`, `confirm_waits_then_runs_on_approve`, `reject_never_runs`, `waiting_expires_after_ttl` (paused tokio clock), `changed_participants_void_the_action`, `changed_target_text_voids_a_reaction`, `timeout_after_send_is_uncertain`, `never_is_refused`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Run every write as a pending action bound to what was approved`.

### Task 3: Write tools and `pending`

**Files:** Modify `crates/open-mess/src/tools/shared.rs`, `tools/mod.rs`.

**Interfaces:**
- Produces: `send {account, conversation, text, reply_to?}`, `edit_message {account, conversation, message, text}`, `react {account, conversation, message, reaction}` -> `{action, state, detail?}` (state is `done` for autonomous success, `waiting` for confirm); `pending {account?, state?, limit? (1-100, default 20)}` -> `[{action, account, conversation, kind, state, created, decided?, detail?}]` (no message text: `pending` never echoes content). `text` max 28 000 characters.
- [ ] **Step 1: Failing tests:** `send_returns_waiting_on_confirm`, `waiting_cap_per_account`, `pending_lists_states_without_text`, `write_tools_exist_over_mcp_and_cli` (both front ends list the same four tools).
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Add the send, edit, react and pending tools`.

### Task 4: Approval in Windows notifications

**Files:** Create `crates/open-mess/src/approval.rs`; modify `daemon.rs`, `Cargo.toml` (Windows-only dependencies).

**Interfaces:**
- Produces: at daemon start, register the AppUserModelID `open_mess.daemon` under `HKCU\Software\Classes\AppUserModelId\open_mess.daemon` (`DisplayName = open_mess`); `approval::Queue` showing one toast at a time for `waiting` actions: title `open_mess: approve <send|edit|reaction>`, lines with account, conversation title, participants (first 5, then `+N`), the text (or `Show all` when over 280 characters); buttons `Approve` / `Reject` call `pending::decide`; dismiss or timeout leaves the action waiting until `APPROVAL_TTL`. `Show all` opens an `rfd` modal dialog with the full content and the same two choices. Non-Windows builds compile a stub that leaves actions waiting.
- [ ] **Step 1: Failing tests:** pure parts only - `toast_text_is_truncated_with_show_all`, `participants_are_shortened`, `queue_shows_one_at_a_time` (queue logic with a fake presenter trait-free closure). The toast and dialog themselves are checked manually: run the daemon, `om send` on a test account with `confirm`, approve and reject once each; record the result in the commit message.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS); manual check.
- [ ] **Step 5:** Commit `Ask for approval in Windows notifications, one action at a time`.
