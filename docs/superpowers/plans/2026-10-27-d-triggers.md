# Milestone D: triggers - implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** when someone writes to the user, the daemon wakes an agent: a notification into an open Claude Code session (action 1) or a new agent run (action 2), with batching, quiet hours and limits, and with other people's content always marked as untrusted.

**Architecture:** `crates/open-mess/src/triggers/` - `rules.rs` (rules in SQLite), `engine.rs` (subscribes to the event bus of B2, matches, batches, rate-limits), `channel.rs` (action 1 through MCP channel notifications), `spawn.rs` (action 2: start a fixed program without a shell). Rules are created and changed only by the configuration window (milestone E) and tests - never through MCP or the CLI, because an agent-made rule could start programs.

**Tech Stack:** what milestones A-C use; rmcp's server-to-client notifications.

**Spec:** `.specs/2026-10-26-open-mess-design.md` 8.6 (triggered agents use the account permissions), 8.7 (whole section), 15 (resource rule). **Depends on:** B2 (event bus, accounts), C (permissions).

## Global Constraints

- Rule = account + condition (`direct`, `mention`, `conversation` with a conversation id, `from` with sender ids) + action (`channel` or `spawn`) + quiet hours (local time window, may wrap midnight) + `max_per_hour` (default 6) + batching (`quiet_ms` default 60 000, `max_delay_ms` default 300 000) + enabled flag.
- Never fire for: the user's own messages (`own = true`), accounts in state `removing` or unknown, conversations the account permissions do not allow to read (spec 8.7, R7).
- Batching: events of one rule and conversation are collected and delivered as one notification when the conversation is quiet for `quiet_ms`, but never later than `max_delay_ms` after the first event (spec 8.7, R4).
- Limits (spec 15): at most `max_per_hour` deliveries per rule (extra batches are dropped and counted in `status`); at most one running spawn per rule and `MAX_RUNS = 2` spawned runs in total (extra batches queue, at most 10 per rule, oldest dropped first); a spawned run is killed after `RUN_TIMEOUT = 30 min`.
- Content: every delivery separates metadata (account, conversation id and kind, message ids, times, counts) from other people's content (sender names, conversation title, message text), which sits under `untrusted` with the same notice text as the shared tools (spec 8.5).
- Action 1: deliver to the most recently active MCP session that declared the channel capability, never to more than one (spec 8.7, R2); none connected -> fall back to the rule's spawn action if it has one, else drop and count.
- Action 2: `std::process::Command` with the rule's program path and fixed argument list - never a shell, nothing from the event in the program path or arguments. The batch goes to the child's stdin as JSON, and to a file in `<data_dir>/runs/` (size-capped 1 MiB) whose path is passed in the environment variable `OPEN_MESS_EVENT_FILE`; the file is deleted when the run ends and leftovers are deleted at daemon start (spec 8.7, R1/R5/R6).
- Agents started by action 2 use MCP/CLI with the same permissions as everyone else (spec 8.6).

## Review Focus

- A sender name containing `"; rm -rf ~; "` or `$(calc)` on a spawn rule: the program receives it only inside the stdin JSON / event file, nothing is executed (Task 4 test `content_never_reaches_the_command_line`).
- A chat that never goes quiet (a message every 10 s for 10 min): first delivery at `max_delay_ms` after the first message, not never (Task 2 test `max_delay_caps_batching`).
- Twenty DMs in a minute on a rule with `max_per_hour = 6`: at most one batch delivered per quiet period, deliveries capped at 6 per hour, the rest counted as dropped (Task 2 test `hourly_cap`).
- The user's own message in a watched conversation: no delivery (Task 2 test `own_messages_never_fire`).
- Two Claude Code sessions with channels open: exactly one, the most recently active, gets the notification (Task 3 test `one_session_gets_it`).

---

### Task 1: Rules store

**Files:** Create `crates/open-mess/src/triggers/mod.rs`, `triggers/rules.rs`; modify `store.rs` (migration: `trigger_rules(id TEXT PRIMARY KEY, account TEXT NOT NULL, rule TEXT NOT NULL /* JSON */, enabled INTEGER NOT NULL, created_ms INTEGER NOT NULL)`).

**Interfaces:**
- Produces: `Rule { id, account, condition: Condition, action: Action, quiet_hours: Option<(NaiveTime, NaiveTime)>, max_per_hour: u32, quiet_ms: u64, max_delay_ms: u64, enabled: bool }` (times as minutes since midnight if no time crate is in the workspace); `Action::{Channel { fallback_spawn: Option<Spawn> }, Spawn(Spawn)}`; `Spawn { program: PathBuf, args: Vec<String>, working_dir: Option<PathBuf> }`; `rules::{list, get, put, delete}` with validation (program must be an absolute path to an existing file; `max_per_hour` 1-60; `quiet_ms` 5 000-600 000; `max_delay_ms` >= `quiet_ms` and <= 3 600 000).
- [ ] **Step 1: Failing tests:** `round_trip`, `relative_program_is_rejected`, `limits_are_validated`, `no_tool_creates_rules` (the registry lists no rule-changing tool).
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Store trigger rules`.

### Task 2: Matching, batching and limits

**Files:** Create `crates/open-mess/src/triggers/engine.rs`; modify `daemon.rs` (subscribe to the bus).

**Interfaces:**
- Produces: `engine::Engine::spawn(bus: &events::Bus, accounts, permissions, rules, deliver: Delivery) -> JoinHandle<()>` where `Delivery` is the boundary to Tasks 3-4 (`async fn deliver(rule: &Rule, batch: Batch) -> DeliveryResult`); `Batch { account, conversation, kind, events: Vec<BatchedMessage>, first_ms, last_ms }`; counters (`delivered`, `dropped_rate`, `dropped_queue`, `dropped_no_session`) per rule, shown by `status`.
- [ ] **Step 1: Failing tests** (tokio paused clock, fake `Delivery` recording calls): `direct_message_fires`, `mention_fires`, `own_messages_never_fire`, `unreadable_conversation_never_fires`, `removing_account_never_fires`, `quiet_period_batches`, `max_delay_caps_batching`, `quiet_hours_suppress` (window wrapping midnight), `hourly_cap`, `disabled_rule_is_ignored`, `lagged_bus_is_counted` (bus overflow does not crash the engine).
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Match, batch and rate-limit trigger events`.

### Task 3: Action 1 - channel notification

**Files:** Create `crates/open-mess/src/triggers/channel.rs`; modify `api.rs` (MCP server: declare `experimental: {"claude/channel": {}}`, track sessions that declare it and their last activity).

**Interfaces:**
- Produces: `channel::Sessions` (registry of live MCP sessions with the channel capability and last-activity time); `channel::deliver(&Sessions, batch) -> DeliveryResult` sending `notifications/claude/channel` with `{content: <text with the untrusted block>, meta: {account, conversation, kind, count}}` to the most recently active session only.
- [ ] **Step 1: Spike (no commit):** start the daemon, connect a Claude Code session started with channels enabled (the user's Telegram plugin is the reference: `~/.claude/plugins/cache/claude-plugins-official/telegram/*/server.ts`, capability `experimental['claude/channel']`, method `notifications/claude/channel`), send a test notification over Streamable HTTP. If Claude Code does not receive channel notifications from an HTTP MCP server, implement `om mcp-stdio` instead: a stdio MCP server that forwards every request to the daemon's HTTP MCP endpoint and forwards the daemon's notifications back, ending with its parent session and reconnecting with backoff when the daemon restarts (spec 8.7, R9). Record which path was taken in the commit message.
- [ ] **Step 2: Failing tests** (rmcp client in-process standing in for Claude Code): `one_session_gets_it`, `most_recent_session_wins`, `no_session_falls_back_to_spawn`, `no_session_and_no_spawn_is_counted`, `notification_marks_untrusted_content`.
- [ ] **Step 3-4:** Implement, run (PASS).
- [ ] **Step 5:** Commit `Wake an open Claude Code session through MCP channel notifications`.

### Task 4: Action 2 - spawn

**Files:** Create `crates/open-mess/src/triggers/spawn.rs`; modify `daemon.rs` (clean `<data_dir>/runs/` at start); create test helper binary `crates/open-mess/tests/bin/record_run.rs` (writes its argv, stdin and the event file to a path given in its first argument).

**Interfaces:**
- Produces: `spawn::deliver(&Spawn, batch, runs: &Runs) -> DeliveryResult` honouring `MAX_RUNS`, one run per rule, the queue of 10, `RUN_TIMEOUT` (kill the process tree on Windows with a job object if the standard library cannot; otherwise `Child::kill`).
- [ ] **Step 1: Failing tests:** `spawn_gets_batch_on_stdin_and_file`, `content_never_reaches_the_command_line`, `event_file_is_deleted_after_the_run`, `leftover_run_files_are_deleted_at_start`, `at_most_two_runs`, `run_timeout_kills`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Start an agent for a trigger without a shell`.
