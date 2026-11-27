# open_mess - Codex desktop trigger delivery

Date: 2026-11-26
Status: implemented locally; both independent Codex code reviews approved; native selected tests and lint passed
Source baseline: main, 2215362958ef440dbb45a1be953fdad16171d317

## 1. Goal and scope

Contributor review record, 2026-11-26: the configured GLM runner was unavailable, so the
requesting contributor chose two independent Codex reviewers. GLM did not run. This records
the contribution's local review process; it does not claim upstream-maintainer approval or
waive the project's specification, code-quality or merge requirements.

Let a user route an existing messenger trigger to a specific existing Codex desktop
conversation without a Node helper, PowerShell polling, or a second permanent daemon.
Keep the Teams module, messenger tools, account permissions, Claude channels and existing
spawn rules compatible. This contribution contains the reusable integration and its tests;
installed binaries, private messenger data and personal workflow configuration are excluded.

The existing Integrations UI already produces a Codex config.toml fragment. Reuse it;
do not rebuild registration or change how the local API token is stored. The CLI's
Claude-only mcp-command convenience is not the obstacle to desktop event delivery.

Review-specific instructions, sender allowlists and publication rules belong to the
user's agent policy and trigger conditions, never to a generic transport implementation.

## 2. Verified context

### Repository

- `crates/open-mess/src/ui/commands.rs::mcp_command` returns both Claude and Codex setup.
- `crates/open-mess/ui/app.js::renderIntegrations` displays both; keep this behavior.
- `triggers/rules.rs::Action` has Channel and Spawn. Rules are configured only by the UI.
- `triggers/channel.rs` implements claude/channel and notifications/claude/channel.
- `bridge.rs` advertises that capability to the daemon for every mcp-stdio connection.
  A counted channel session alone therefore does not prove that Codex received a notification.
- `triggers/mod.rs::Actions` loads content and dispatches a collected Batch.
- `triggers/spawn.rs` counts handoff to a process queue, not agent acknowledgement.
- `store.rs` has ordered SQLite migrations and refuses a schema newer than the executable.
- `permissions.rs`, `accounts.rs` and the existing trigger engine already own read checks,
  account-removal state, batching, quiet hours, hourly limits and remembered message IDs.
- `update/quiesce.rs` and trigger Activity coordinate work with updates. Reuse that boundary.
- The existing design does not permit a local message archive. Persistent delivery records
  below contain only identifiers, times and states; content is fetched for delivery.

No open upstream PR or branch identified on 2026-11-26 implements this scope. PR 28 concerns
conversation-name matching and must remain independent.

### Runtime experiments, 2026-11-26

Experiments ran outside this source checkout, in a separate ignored prototype directory.
Only synthetic markers were sent to an existing user-owned Codex conversation. No Teams
or GitHub action was performed by either synthetic turn. No real message body was retained.

1. Installed Codex: OpenAI.Codex_26.1117.4957.0. A temporary `codex app-server --stdio`
   accepted initialize and thread/read for the same existing desktop conversation.
2. thread/resume returned JSON-RPC -32600: `thread <id> already has an active writer`.
   Reading shared history is not ownership of the live desktop conversation. Do not start
   another app-server, change rollout files, or take ownership to work around this error.
3. The running desktop exposed one local codex-app-tools named pipe. Its tools/list
   contained codex_app.send_message_to_thread and codex_app.read_thread.
4. Two marker batches were accepted by the desktop and each was acknowledged by the agent
   in the same conversation. A repeat of the same outbox event sent nothing.
5. On a second run, read_thread reported status active before the second marker was sent.
   Both markers were then acknowledged in that turn; the turn contained no side-effect calls.
6. Fourteen prototype tests passed, including framed replies, uncertain delivery, outbox
   recovery and a controlled change of the local pipe endpoint. A real desktop restart was
   not performed: restarting the app would interrupt other work. It remains a release check.

The app-tools endpoint is an installed desktop integration interface, not the documented
Codex app-server protocol. Support is explicitly experimental and capability-checked on
each connection. Documentation: https://developers.openai.com/codex/app-server .

## 3. Contract

R2 clarification approved by both independent Codex reviewers: Codex configuration snapshots apply from the
start of an in-memory collecting batch, not only from outbox admission. If a rule changes while
a batch is collecting and either the original or new action is Codex, discard the old batch
without rerouting it. A new matching message starts a new batch under the new configuration.
Pure Channel/Spawn collecting-batch behavior remains unchanged. Use a persistent monotonic rule
generation, so changing A to B and back to A still invalidates the original collected batch.
Track this generation on every collecting batch, including legacy actions, so a transition
Channel -> Codex -> Channel between engine observations cannot preserve an older batch.
Capture the matching rule and generation under one store guard, revalidate before collection,
and validate generation/current configuration atomically with durable admission. Pure legacy
edits retain their prior generation and behavior.
Keep discarded identifiers remembered across restart. Reviewers approved this clarification
on 2026-11-26 before implementation; other native fixes stay within the R1 contract.

### 3.1 New destination, preserving old rules

Add `Action::Codex { thread_id: String }`, serialized as type codex and thread_id. Existing
Channel and Spawn JSON, their defaults and delivery behavior remain unchanged. On non-Windows,
reject creation of a Codex destination with a clear unsupported-platform error; existing
non-Codex rules continue to work. An unsupported saved Codex rule never crashes the daemon.

Only trusted configuration UI operations may create/change a destination. Add a third action
to the existing trigger editor and a required conversation ID field with plain-language help
explaining where to copy an existing Codex conversation ID. Validate the bounded identifier
without executing anything. A read-only check button verifies the destination through
read_thread; saving while Codex is closed is allowed with an explicit offline state. Do not
create a conversation, choose a recent conversation, or change Codex settings automatically.
Display and accessibility conventions follow the existing bilingual UI and tr() helper.

Do not expose a new MCP/CLI tool that changes routing, enables rules, approves messages or
chooses executables. Existing status gains additive delivery counts only. No messenger tool
schema, default, name, error or permission changes.

### 3.2 Desktop protocol adapter

Implement the adapter inside the Rust daemon, using installed tokio, serde_json and getrandom.
On Windows discover local named pipes whose names begin exactly with codex-app-tools-. Probe
tools/list using the existing desktop envelope `{threadStartKind:"all"}`. A candidate must
advertise the two named codex_app capabilities and read back the exact configured thread ID.
Zero candidates means offline; more than one matching candidate means ambiguous and no send.
Never use a remote pipe, arbitrary path, shell, window automation, token extraction or fallback
to the most recent conversation. Discover again after reconnect; do not persist a volatile pipe.

Wire format: little-endian u32 UTF-8 JSON byte length followed by that many bytes. Requests
use JSON-RPC 2.0 and a random ID; reply ID must match. Reject malformed JSON, missing result,
oversized frames and mismatched IDs; a partial header/body is not a complete reply. Bound the
fully serialized request and response to 8 MiB before allocating/writing. Probe at most 32 pipes,
with 2-second probe budgets and a 10-second aggregate discovery budget. Use a 60-second send
budget. Discovery failure cannot widen the target or select an arbitrary instance.

Send only codex_app.send_message_to_thread with arguments threadId, hostId local, prompt;
omit model and thinking. The desktop call envelope also contains namespace codex_app, tool,
callerSource codex, hostId local, threadId and fresh callId/turnId correlation identifiers.
The observed read_thread response is `{success:true,contentItems:[{type:"inputText",text:...}]}`;
it is not the external MCP wrapper `{content:[...]}`. Parse the embedded metadata with limits.

The prompt has a fixed trusted introduction saying a configured messenger rule fired, plus
the existing JSON event envelope whose other people's text remains under untrusted. No
message text becomes a path, argument, instruction, policy override or program. A desktop
`success:true` records accepted_by_codex, not agent completion or a Teams reply. No automatic
agent-result parsing, response forwarding or change of write permissions is included.

### 3.3 Durable delivery records

Add a dedicated SQLite migration/table owned by the daemon; do not reuse user approval rows.
Store the rule ID, destination snapshot and full-rule hash, account/conversation/message identifiers, times,
count, attempt state and retry time. Never store message bodies, sender names, attachments,
agent prompts, Codex responses or credentials. Cap batches at the engine's existing MAX_BATCH.

Admission and the trigger engine's remembered-ID update must form one SQLite transaction,
so a crash cannot mark an event handled without its pending delivery record. Remembering
handled events still applies to the old destinations without changing their behavior.
Enforce uniqueness per rule/account/conversation/message ID to suppress replay and overlapping
re-batches, rather than hashing only a batch's set of IDs. Preserve message order using time
and stable message-ID tie-breaking. A repeated already-accepted message never sends again.

States: pending, sending, accepted_by_codex, uncertain, failed, voided, expired. Persist sending
before the first possible send. After daemon restart sending becomes uncertain; it must not
be automatically retried. No-response/invalid-response/timeout after a possible write is
uncertain, never pending. A definitive rejection is failed. Before any possible send, a known
offline endpoint may return to pending with backoff 5, 10, 20, 40, 60 seconds, then every minute.
Bound retries by TTL. A broken or ambiguous adapter is visible as failed instead of silently
falling back to a different transport. Never promise exactly-once delivery across an ambiguous
external send. Manual resolution of uncertain/failed is UI-only: the user can mark
accepted_by_codex or dismiss as voided, preserving the uniqueness receipt. No retry button for
uncertain delivery. New window commands are read-only check_codex_destination(thread_id),
codex_deliveries(rule_id), and resolve_codex_delivery(id, resolution), where resolution is accept
or dismiss. Refuse resolution of pending/sending or unknown rows. Invalid arguments never send.
These are trusted window commands, never tools. Off-Windows administrative clients get a fixed
unsupported-platform error for destination checks; they never gain a Codex send endpoint.

At every attempt recheck the current rule is enabled, its full configuration hash matches, the account
exists and is not removing, read permission is allowed, quiet hours are over and hourly caps
allow a send. Hash the full saved rule configuration, not just its action, so a changed sender
condition also invalidates pending work. Disabled/changed/deleted rules and revoked read/account removal void their
pending records; they never reroute to a newly configured conversation. Past quiet hours/rate
caps do not create a catch-up burst. For Codex rules only, the engine admits to the outbox without
charging its process-local Sent budget. The outbox reserves an attempt just before the first
possible write, atomically with sending state, in SQLite using UTC milliseconds. Count reservations
per rule over the trailing hour, including uncertain sends, and retain them across daemon restart.
Discovery while offline consumes no send reservation. Keep old Channel/Spawn rate behavior unchanged.
If the wall clock moves backward, do not remove reservations from the future; conservatively
defer new writes until those reservations age out. Prune reservations only after the hour and
never reset them merely because an event became terminal. Limit retained reservations to the
configured rule count times its bounded hourly limit and 50,000 globally, refusing overflow.
Keep reservations when rules are deleted; they age out naturally and cannot be reset by
deleting/recreating a rule with the same ID.

Re-fetch message content through the existing read_messages module path and bounded event
formatter. The existing fill_in_content reads only the newest batch-size items, so it cannot
provide delayed lookup. Use pages of 100, then before equal to the oldest returned ID, at most
10 calls/1,000 returned items/4 MiB of returned JSON/30 seconds for the whole lookup. Stop once
every requested ID is found, a page is empty, before does not advance, or a bound is reached.
Never infer deletion from a short page or the engine's arrival timestamp. The platform's before
semantics may omit tied timestamps; unmatched items get explicit content_status lookup_unavailable
and their original IDs, not a claim of deletion. A platform failure keeps pending within TTL.
Do not change the public read_messages contract or fetch an unbounded conversation history.

After all asynchronous lookup/discovery/preparation, recheck the full current rule hash, account,
read permission, quiet hours, rate reservation and update holds immediately before the first
possible desktop request write. This final check covers the initial attempt as well as retries.
It must run after connecting the pipe and serializing the bounded request, without another await
between the synchronous authorization check/reservation and starting the write. If it fails,
send nothing and persist the corresponding voided/pending state. Do not rely on pre-fetch checks.
Do not mark messages read. Permission revocation after a possible send cannot revoke an already
delivered prompt; record the actual result without retrying or leaking more content.

Resource limits: 1,000 pending batches globally, 100 per rule and 8 MiB of serialized metadata
globally; 24-hour pending TTL. Reject the newest admission atomically when any limit is exceeded
and expose an overflow counter instead of deleting older pending work silently. Terminal
metadata/uniqueness receipts expire after seven days and are capped at 50,000 entries, aligned
with existing remembered-ID bounds. Explain this bounded replay protection in documentation.
No content archive is introduced. Removing an account clears its associated delivery metadata.

One daemon-owned worker, at most one delivery in flight per destination and four globally.
Use the existing Activity/quiesce boundary: updates stop new sends, wait for started sends,
and leave unsent records durable. Cancellation before a possible write leaves pending;
cancellation after it becomes uncertain. A failed persistence write after external acceptance
leaves sending, which becomes uncertain on recovery. No second helper process is started.

The worker listens to the existing probation hold and a new Codex-only pause in trigger Activity.
Set the latter at the start of update finish_work, before waiting for in-flight deliveries;
pending outbox rows are not in-flight work and do not prevent an update from finishing.
Clear this pause in quiesce::resume when cancellation restores normal work. The final pre-write
check must see both holds. Count started sends through Activity.delivering with a guard raised
before the final hold check and dropped only after persistence of their result.

### Delivery reconciliation after configuration changes

The trusted Triggers window provides an account-independent entry to unresolved Codex
deliveries, including records whose rule was deleted or changed to Channel/Spawn. Account
removal still clears its metadata. Every row identifies its original stored Codex conversation,
not the current rule's target. Both rule history and unresolved history use bounded pages of
100 records, ordered by creation time descending and ID ascending. An explicit next-page
cursor retains that order and query scope; resolving a record never sends it again.

## 4. Implementation sequence and touched files

1. Complete both independent Codex spec reviews following the contributor review record. Record every
   finding and disposition below.
2. Document the verified redacted desktop protocol in docs/protocols/codex.md. State the
   tested app version and active-writer limitation. Reconcile design sections 8.2 and 8.7.
3. Extend triggers/rules.rs and its existing validation/round-trip tests with the Codex action.
4. Add a focused triggers/codex.rs adapter and deterministic framed-protocol tests. Keep its
   OS transport behind cfg(windows); controlled duplex peers can test parsing on Linux.
5. Add triggers/outbox.rs, its SQLite migration in store.rs and tests. Connect engine admission
   transactionally, and connect the worker through triggers/mod.rs, daemon.rs and quiesce.
6. Extend ui/commands.rs, ui/window.rs and ui/app.js for destination checks and queue state.
   Register the new commands in crates/open-mess/build.rs::COMMANDS and
   crates/open-mess/capabilities/main.json; test invocation permissions on Windows.
   Use existing tr() translation and trusted UI command patterns. No general adapter framework.
7. Add counts/state to status and the trigger UI, with identifiers visible only in the intended
   configuration UI, not logs or status counters. Update README and existing trigger tests.
8. Run cargo xtask ci on the exact snapshot, plus opt-in local desktop checks below. Do not
   replace the user's live executable or migrate the user's database during development.

Follow the existing self-update rollback/database-snapshot rules for the new migration. Older
binaries must not read a newer live schema; an updater rollback restores its matching database
snapshot. Do not weaken SchemaTooNew to support this feature.

## 5. Verification and acceptance

Add executable Gherkin scenarios under specs/ for this API/MCP integration scope, linked to
the native Rust tests rather than introducing a BDD runtime. Specs are in English. Validate
their syntax with an existing parser when available; lack of a parser is an explicit gap.

- Contract tests: old Channel/Spawn serialization and MCP tools/list/tools/call remain compatible.
- Desktop decoder: fragmented header/body, wrong ID, invalid JSON, oversized serialized Unicode,
  missing success, explicit refusal, missing/ambiguous capability, cancellation and disconnection.
- Outbox: acknowledged duplicate, overlapping replay, crash before/after sending, persistence
  failure after acknowledgement, worker restart, bounded capacity/TTL and all terminal states.
- Permissions: change rule target, disable/delete rule, remove account or revoke read while
  pending. Assert no send and durable voided state; quiet-hour/rate-limit retries stay bounded.
  Also block asynchronous content/discovery, revoke access or change sender conditions, then
  release preparation and prove the initial attempt writes zero desktop request bytes.
- Delayed content: enqueue an older message, add newer messages, recover offline delivery and
  assert a later read_messages page supplies the original content. Check bounded lookup, tied
  timestamps/nonprogress and unavailable markers without falsely declaring deletion.
- Durable rate: with max_per_hour=1 accept one batch, restart with another pending batch in
  the same hour and assert zero second writes; advance a controlled clock beyond the hour and
  assert one send. Test backward wall-clock movement and uncertain reservations too.
- UI: edit/save/cancel each action, required field, offline check, ambiguous instance, queue-state
  labels, copy existing MCP config, English/Polish text, keyboard operation and narrow layout.
- Runtime: tools/list and safe tools/call(status) against an isolated running open_mess host.
  A mocked adapter does not establish the public host binding or permissions behavior.
- Opt-in local synthetic desktop run: idle then active target, readback of both acknowledgements,
  no messenger/GitHub calls, no duplicate after replay. CI never uses the user's accounts.
- Opt-in real restart: while the user can safely restart Codex, create an offline pending event,
  reopen the app and verify one delivery after discovery. Until executed it is unverified,
  regardless of the controlled endpoint-replacement test passing.

## 6. Threat model and hardening boundary

Trusted: the user's configuration actions, installed daemon and its state database. Partially
trusted: connected agents and the installed desktop IPC surface; verify capabilities and IDs.
Untrusted: message content, sender names, platform payloads, pipe responses, stored/corrupted
delivery records and external process errors. Validate persisted state on restore, cap decoded
sizes/counts and never log raw payloads, IDs, names, prompts or external error text. Logs contain
fixed state names, codes and counts only, following AGENTS.md.

Do not defend against malicious local code already running as the same Windows user, consistent
with the root design's boundary. This does not authorize changing pipe ownership, Codex auth,
account permissions, review policies or the user's existing installation. The experimental
transport may break on an incompatible desktop update; fail visibly and preserve pending work.

### Local verification record, 2026-11-26

Baseline is the SHA above; changes remain in the working tree with an empty index and no
commits. Two independent Codex reviewers approved the final Rust/JavaScript implementation.

- Final native selected suite: 341 tests passed on Linux in the isolated Rust 1.100.0 container,
  with test executables run as UID 1000. This includes library 221, accounts 15, HTTP/MCP 11,
  channel 15, permissions 2, spawn 9, triggers 22, UI 28, English UI 5 and update quiesce 13.
  Running as root first produced one unrelated read-only-file fixture failure in recording;
  the unchanged test passed as an ordinary user. New Codex tests passed in both runs.
  Test WASM modules, including Teams, were built before the final regression run.
- Prototype framed transport/outbox: 14 passed. These synthetic-only prototype tests are
  separate from native production tests and are not evidence of deploying the Rust worker.
- The exact native codex.rs adapter compiled for Windows GNU and ran on Windows. A synthetic
  send was accepted and its requested acknowledgement observed in the existing Codex chat.
- Browser fixtures exercised the actual UI JavaScript with synthetic data and mocked native
  commands: English at 1100px and Polish at 800px, required-ID validation, read-only checking,
  offline draft preservation, save arguments and manual resolution without a send. Screenshots
  were visually checked. This does not establish native Windows WebView invocation permissions.
- Gherkin syntax: 13 scenarios parsed with @cucumber/gherkin. This is syntax validation, not
  execution of the manual scenarios.
- cargo clippy -p open-mess --all-targets --locked -- -D warnings passed for the final code.
  Formatting and git diff --check passed.
  This is not a completed full workspace cargo xtask ci gate. The test command was cargo test -p open-mess --lib
  --test api --test accounts --test channel --test spawn --test triggers --test permissions
  --test ui --test ui_en --test update_quiesce --locked.
- Still unverified: real Codex desktop restart and a native Windows configuration-window run.
  The running installation and user database were not replaced or migrated. No Teams or GitHub
  messages were sent during verification.
- Whole-library Windows cross-check from Linux did not pass: cargo check -p open-mess --lib
  --target x86_64-pc-windows-gnu --locked stopped at tauri::generate_context! with
  "capability with identifier main not found". The baseline build.rs gates tauri-build on
  cfg(windows), which describes the build-script host. Directly compiling and running that
  unchanged baseline script on Linux, even with Windows target environment variables, produced
  no capability metadata. This confirms the build-host prerequisite; it does not establish a
  passing Windows library check. A full baseline Cargo comparison was stopped after this narrow
  reproduction identified the prerequisite. No cross-build workaround was added to the product.
  A native Windows build and window invocation remain release checks, alongside full workspace CI.

### Windows runtime follow-up, 2026-11-27

The following supersedes the earlier unverified Windows-window and no-deployment status.
The full Windows MSVC release executable built with cargo-xwin 0.23.1 from an isolated
source copy. That copy changed only the build-dependency target gate in Cargo.toml and the
build-script host cfg guards, allowing Tauri's normal Windows-target capability generation
to run on Linux. These two build-host shims are not product changes in this patch; the
remaining 220 reviewed source files matched byte for byte. This is not evidence of a
native Windows-host build or of the full workspace CI gate.

The executable ran on Windows. An isolated daemon passed MCP initialize/tools-list/status,
invalid-argument rejection and unauthenticated HTTP 401 checks. The actual Tauri settings
window saved a synthetic Codex rule and checked an existing desktop destination without
sending a test message. The same executable subsequently migrated an authorized existing
installation from schema 8 to 9, saved the destination through trusted UI, and restored its
existing account online. Account permissions and non-action rule fields were compared before
and after migration and matched. The rule survived a daemon restart. The desktop application
also restarted; a subsequent read-only destination check succeeded. These checks supersede
the earlier missing-window and missing-desktop-restart observations, but do not establish
queued delivery while the desktop is closed followed by exactly one accepted delivery after
reopening, nor a new genuine incoming message's complete end-to-end path.

Deployment credentials, messenger/session identifiers, local routing/policy helpers and
message history are private operational state and are excluded from this contribution.
Full `cargo xtask ci` subsequently passed for the final code on 2026-11-27. This is a local
Linux test result, separate from the earlier Windows executable checks.

### R4 review corrections, 2026-11-27

The pre-correction snapshot completed `cargo xtask ci` on an ordinary second run. Its first
run failed one unchanged simulated Teams state case; the second passed without product or
test changes. The failed result is retained and its cause is not established.

The subsequent R4 corrections add independent unresolved-history access, explicit original
destinations and bounded keyset pages. The 47 native trigger tests passed, including malformed
and oversized protocol replies, Unicode request limits, conservative acknowledgements,
capability/ambiguity discovery, global and byte admission edges, expiry and receipt edges,
restored-state rejection, lookup bounds, tied missing-message identities, production rule
mutation/account cleanup and real update finish/resume wiring. Two headless browser fixtures
passed against the actual UI JavaScript in English and Polish, using mocked native commands;
they cover the edited/deleted-rule histories, original destination, older-page failure/retry
and terminal resolution without sending. All 15 Gherkin definitions parsed successfully.

These corrections are not yet deployed to the installed executable. The Windows runtime checks
above belong to the earlier snapshot. An actual Claude Desktop 7.37095.0 status invocation
against that version's isolated daemon also passed: version 0.2.1, no accounts or modules.
The temporary test connection and daemon were removed afterward. Final R4.2 code passed
full workspace CI on 2026-11-27, and independent review validated all ten P2 corrections.
Only public documentation cleanup followed that code run; the executable remains unchanged.

## 7. Rebuttal register

None. No finding has been rejected. The GLM runner failed before model execution because
opencode was unavailable; the contributor then requested the reviewer substitution recorded above.

## 8. Round changelog

- R0: draft grounded in baseline source and synthetic runtime transport experiments.
- R1: reviewer A rejected two P2 gaps; fixed final pre-write authorization and bounded delayed
  lookup with explicit unavailable semantics. Reviewer B approved with two P3 gaps; added
  Tauri command/permission files and durable hourly-reservation recovery. Clarified full-rule
  invalidation, update-worker pause and user-only terminal resolution. At that round native implementation,
  compiler checks and real desktop restart were pending. Both independent Codex reviewers
  approved R1 without additional findings on 2026-11-26. This approves the design only.
- R2: both reviewers approved the collecting-batch clarification before implementation.
  Fixed terminal-receipt rotation at message/byte limits, persisted destination validation,
  conservative discovery failures and production-path verification. Follow-up reviews found
  legacy -> Codex -> legacy invalidation and split configuration snapshots; fixed both with
  a persistent generation and shared store-guard boundaries. Added deterministic transition
  and interleaving tests. Final authorization and reservation also share one store guard.
  Both independent reviewers then approved the current code without further findings.
  Their approval is static review; final native/API/regression/lint execution is separate.
- R3: native verification found the existing translation-convention test requires i18n::pick
  beside each translated literal. Changed the new error mapping to direct pick calls, with
  identical strings and behavior. Both reviewers approved that delta. Built the missing Teams
  WASM fixture instead of weakening its UI test. The complete selected native suite then passed.
  Lint found a collapsible conditional, needless reference and unchecked JSON assignment.
  Corrected these and the equivalent payload assignment using checked object insertion.
  Both reviewers approved the final narrow lint delta. Final lint and all 341 selected native
  tests passed on that code after the corrections, with no further source changes.
  Reconciled the root design's 8.2/8.7 with links to this contract; both reviewers approved
  that documentation-only update. Recorded the Windows cross-build limitation without claiming
  native Windows application verification or changing the baseline build-host requirement.
- R4: corrected the three accepted reconciliation-UI findings and added the seven requested
  P2 native-protection groups. Retained the bounded metadata-only design and unchanged public
  messenger/MCP tool contracts. Independent review closed the accepted P2 findings, and the
  final code passed full workspace CI. These corrections are not yet deployed locally.
