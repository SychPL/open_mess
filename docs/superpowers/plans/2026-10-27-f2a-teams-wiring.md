# Milestone F2a: Teams module wiring - implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the Teams module performs real work through the host imports - sign-in and session restore through WAM, the service directory, the shared read tools, writes, target description, attachments and new-message events by polling - verified against a fake Teams server, ready for the live checks of F2b.

**Architecture:** `modules/teams/src/component.rs` stops answering `not-supported`: a small `session.rs` keeps tokens and the service directory in memory (never in storage), `api.rs` turns F1's request builders into `http.fetch` calls with the retry policy and CAE/401 handling, `tools.rs` maps the shared tools onto them, `poll.rs` produces `message-arrived` events from a timer. Tests load the built `open_mess_teams.wasm` in the real host against an axum fake server on loopback (test address policy), with the module's hosts remapped to the fake server by a test-only manifest override in the host.

**Tech Stack:** what F1 and B1 use.

**Spec:** `.specs/2026-10-26-open-mess-design.md` 8.5, 8.6 (`describe-target`), 9.1-9.3, 9.5. **Protocol:** `docs/protocols/teams.md` (sections 1-8 and Live results 1-5 - the live results win where they differ). **Depends on:** E2's WAM import (for the real sign-in; tests use a fake `wam-token` from the host's test support), D (sender field on `new-message`).

## Global Constraints

- Tokens and the skype token live only in the module instance's memory; a reloaded instance gets new ones silently through `wam-token(interactive=false)`; `auth-required` only when WAM says interaction is needed.
- Tokens per audience as in docs section 1.4 / Live results: chat service and its properties with `https://ic3.teams.office.com/.default` (plain AAD, Live result 3), authsvc and middle tier with `https://api.spaces.skype.com/.default`, search with `https://outlook.office.com/.default`, Graph (people lookup, 1:1 chat ids) with `https://graph.microsoft.com/.default`.
- Read-state writes are impossible by construction: every property write goes through F1's `property_write` (only `emotions`); no code path builds `consumptionhorizon`, `markRead`, `isread`, `l2ch` or mark-all-as-read requests; no endpoint registration, `activeendpoint`, presence or Trouter in F2a (those need live checks in F2b) (spec 9.2).
- Retry policy from F1 `retry::decide`; 401 with a CAE claims challenge -> one silent token refresh with the claims, then `auth-required`.
- 1:1 chat ids: check both oid orders, as Live result 5 shows.
- Events: a 60 s timer polls `v1/users/ME/conversations?view=msnp24Equivalent&pageSize=50&startTime=<last poll>`; each conversation whose `lastMessage.id` is new and not from the user emits one `message-arrived` (sender id, direct / mentions-me from the message) - read-only, no read-state change.
- `writing_context` keeps the filters of `teams-wrapper` (`C:\Projekty\sych.pl\teams_wraper\src\writing-style\`): only the user's own messages, no secrets, code, bare links, JSON or operational data.

## Review Focus

- A 401 with `WWW-Authenticate: Bearer ... claims=...` on a chat call: one silent refresh with the claims, the call is retried once, a second 401 is `auth-required` (Task 2 test `cae_challenge_refreshes_once`).
- A chat page whose `backwardLink` points at another host: refused as incompatible, the token is never sent there (Task 3 test `foreign_backward_link_is_refused`).
- `unread` across 300 conversations: paged with the service's page size, bounded by the call limit, partial results with an error rather than a timeout of the whole tool (Task 3 test `unread_pages_within_the_limit`).
- A message from the user themself arriving through polling: no `message-arrived` with `own = false` (Task 5 test `own_messages_are_flagged`).
- `send` to a group chat whose roster changed between `describe-target` calls: the core voids it (covered by C); here: `describe-target` returns the full roster from `v1/threads/<id>/members` (Task 4 test `describe_target_lists_members`).

---

### Task 1: Test harness and session

**Files:** Modify `crates/open-mess-host` (test-support: manifest host override, fake `wam-token`), create `modules/teams/src/session.rs`, `modules/teams/tests/` (host-level tests live in `crates/open-mess-host/tests/teams.rs` or a new `crates/open-mess/tests/teams.rs`, whichever builds the wasm first).

- [ ] **Step 1: Failing tests:** `sign_in_returns_the_account_from_the_token` (fake WAM token with `oid`/`name` claims -> `account-info { id: oid, display-name: name }`), `restore_gets_a_new_token_silently`, `restore_without_wam_session_is_auth_required`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). **Step 5:** Commit `Sign in and restore Teams sessions through WAM tokens`.

### Task 2: Calls, directory and retries

**Files:** Create `modules/teams/src/api.rs`.

- [ ] **Step 1: Failing tests:** `directory_comes_from_authsvc_once`, `retry_after_is_honoured`, `long_retry_after_gives_up`, `cae_challenge_refreshes_once`, `regional_404_redirect_is_followed_on_the_chat_host_only`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). **Step 5:** Commit `Call Teams services with retries and token refresh`.

### Task 3: Read tools

**Files:** Create `modules/teams/src/tools.rs`.

- [ ] **Step 1: Failing tests:** `list_conversations_maps_kinds`, `read_messages_pages_backwards`, `foreign_backward_link_is_refused`, `unread_uses_the_client_rule`, `unread_pages_within_the_limit`, `mentions_from_recent_messages`, `search_posts_to_substrate`, `writing_context_filters_like_the_wrapper`, `get_attachment_fetches_ams_image`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). **Step 5:** Commit `Serve the shared read tools from Teams`.

### Task 4: Writes and targets

- [ ] **Step 1: Failing tests:** `send_posts_the_live_accepted_body`, `edit_puts_the_message`, `react_writes_only_emotions`, `describe_target_lists_members`, `describe_target_reads_the_target_text`, `timeout_after_post_is_uncertain`, `one_to_one_id_checks_both_orders`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). **Step 5:** Commit `Send, edit and react in Teams and describe targets`.

### Task 5: Polling events

**Files:** Create `modules/teams/src/poll.rs`.

- [ ] **Step 1: Failing tests:** `new_message_emits_an_event`, `own_messages_are_flagged`, `mentions_and_direct_are_set`, `restart_does_not_replay_old_messages` (last seen ids kept in module storage).
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). **Step 5:** Commit `Report new Teams messages by polling`.
