# Milestone B1: module host runtime - implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** a library crate that loads a `wit/module.wit` component, runs it under time, memory and size limits, and implements the host imports (HTTP, WebSocket, storage, secrets, timers, host events, sign-in stubs) with the network rules of spec 8.3 - usable by the daemon in milestone B2.

**Architecture:** New crate `crates/open-mess-host` (no dependency on `crates/open-mess`). `wasmtime` with the component model and async support, `wasmtime-wasi` for the WASI p2 interfaces std pulls in (clocks, random, io, cli) with everything closed (no preopens, no env, no args, null stdio, no sockets). Each loaded module lives in one tokio task that owns its `Store` (an actor); callers talk to it through `ModuleHandle`, so calls into one module are serialized and a crash or timeout drops that actor only. Networking goes through one `Net` component that applies the host list, the address policy, redirects, size limits and timeouts.

**Tech Stack:** Rust 2024, wasmtime + wasmtime-wasi 50.x (component model, async), tokio, reqwest 0.13 (rustls, system proxy), tokio-tungstenite, psl (public suffix list), rusqlite (bundled), keyring 4, thiserror. Read docs.rs for the exact versions you add - the wasmtime component bindgen, `ResourceLimiter`, epoch interruption and `WasiCtxBuilder` APIs change between majors.

**Spec:** `.specs/2026-10-26-open-mess-design.md` 8.3 (whole section), 15 (resource rule). **Contract:** `wit/module.wit` (note the `%own` field and the `module-manifest` alias). **Guest to test against:** `modules/teams` (milestone F1) builds `target/wasm32-wasip2/debug/open_mess_teams.wasm`.

## Global Constraints

- No `unwrap` / `expect` / `panic!` / unchecked indexing in non-test code; generated bindings may need `#[allow(unsafe_code)]` only on the generated module, listed in AGENTS.md like `modules/teams/src/component.rs`.
- Modules may reach only globally routable unicast addresses: deny loopback, unspecified, private RFC 1918, CGNAT `100.64.0.0/10`, link-local, `fc00::/7`, multicast, broadcast, documentation, benchmarking and the other IANA special-purpose ranges, also as IPv4-mapped/compatible IPv6 (spec 8.3, R9). A test-only `AddressPolicy::AllowLoopbackForTests` exists behind `#[cfg(any(test, feature = "test-support"))]`; the daemon cannot construct it.
- Host list: exact host names or `*.` suffix patterns; a pattern must sit below a registrable domain per the public suffix list (`*.com`, `*.co.uk` rejected at load) (spec 8.3, R7). Matching is case-insensitive on the punycode host.
- Direct connections are pinned: resolve once, check every address, connect to a checked address (reqwest `ClientBuilder::resolve` per request). Through the Windows system proxy: check the host and its resolution, then let the proxy connect (spec 8.3, R5/R6).
- No automatic redirects: `redirect::Policy::none()`; 3xx goes back to the module (WIT comment on `fetch`).
- Limits (values are this plan's decisions, constants in `limits.rs`): call wall-clock `CALL_TIMEOUT = 30 s`; memory `MAX_MEMORY = 256 MiB` per module; response body `MAX_BODY = 16 MiB`; WebSocket message `MAX_WS_MESSAGE = 4 MiB`; open WebSockets per module `MAX_SOCKETS = 4`; storage `STORAGE_QUOTA = 16 MiB` per module; `Retry-After` above `MAX_RETRY_AFTER = 300 s` returns `throttled(secs)`; connect timeout 10 s, read timeout 30 s; timers per module `MAX_TIMERS = 32`, shortest delay 100 ms; consecutive crashes before auto-reload stops `MAX_CRASHES = 3`.
- Sign-in imports and the time the user spends in them are excluded from `CALL_TIMEOUT` (spec 8.3, R4); in B1 they are stubs returning `refused("sign-in UI not available")`, so this only matters for the API shape: the actor pauses the call deadline while a sign-in import runs.
- No logging of headers, bodies, tokens or message text; `Net` logs method, host, status and size only.

## Review Focus

- A module that loops forever in `call`: the call returns `HostError::Timeout` within `CALL_TIMEOUT + 1 s`, the module is unloaded, the next call reloads it (Task 3 test `infinite_loop_is_interrupted`).
- A module allocating without bound: the call fails with `HostError::Crashed`, the daemon process keeps running (Task 3 test `memory_hog_is_stopped`).
- A server that answers 302 to `http://127.0.0.1:<port>`: the module gets the 302 response, and fetching the `Location` it hands back is `blocked` (Task 4 test `redirect_target_is_checked_on_its_own_fetch`).
- A host name that resolves to `100.64.1.1`, `10.0.0.1` or `::ffff:192.168.1.1`: `blocked` (Task 4 unit tests in `ip_policy`).
- A response body larger than `MAX_BODY` that never ends: `too-large` after `MAX_BODY` bytes, without buffering more (Task 4 test `oversized_body_is_cut`).

---

### Task 1: Crate, bindings and loading the Teams module

**Files:** Create `crates/open-mess-host/Cargo.toml`, `src/lib.rs`, `src/bindings.rs` (wasmtime `bindgen!` for world `platform-module` from `../../wit`, async), `src/runtime.rs`, `src/error.rs`, `src/limits.rs`. Modify `xtask/src/main.rs`: build all `modules/*` for `wasm32-wasip2` **before** `cargo test` (tests load the built `.wasm` files).

**Interfaces:**
- Produces: `runtime::Engine::new() -> Result<Engine, HostError>` (one per process; starts the epoch ticker); `Engine::load(&self, wasm: &Path, ctx: ModuleContext) -> Result<ModuleHandle, HostError>`; `ModuleHandle::manifest(&self) -> Result<Manifest, HostError>`; `Manifest` = plain Rust mirror of WIT `manifest` (`name`, `version`, `platform`, `hosts`, `sign_in_hosts`, `wam: Option<WamIdentity>`, `extra_tools: Vec<ToolDecl>`); `ModuleContext { data_dir: PathBuf, events: tokio::sync::mpsc::Sender<HostEvent>, net: Net }`; `HostError::{Load(String), Timeout, Crashed(String), Module(ModuleError), Closed}`; `HostEvent::{MessageArrived(NewMessage), Session { module: String, account: String, state: SessionState }}`.

- [ ] **Step 1: Failing test** `crates/open-mess-host/tests/load.rs`: `loads_teams_manifest` - load `target/wasm32-wasip2/debug/open_mess_teams.wasm`, `manifest().name == "teams"`, `wam.client_id == "1fec8e78-bce4-4aaf-ab1b-5451cc387264"`; `rejects_non_component` - loading a file with random bytes is `HostError::Load`.
- [ ] **Step 2:** Run, expect FAIL. **Step 3:** Implement: WASI context with nothing inherited (stdin empty, stdout/stderr discarded, no env, no args, no preopens, socket address check denying everything). **Step 4:** Run, expect PASS; `cargo xtask ci` green.
- [ ] **Step 5:** Commit `Load platform modules as wasm components`.

### Task 2: Test guest modules

**Files:** Create `modules/test-echo/` (package `open-mess-test-echo`): `call` with tool `echo` returns its args; tool `fetch` takes `{"url": ...}`, calls `http.fetch` GET and returns `{"status": n}` or the error variant name; tool `ws` opens a WebSocket to `{"url"}` and sends `"ping"`; tool `store` sets and gets a key of `{"size": n}` bytes; tool `timer` sets timer `7` after 100 ms; `on-timer` and `on-ws-message` forward to `host.log`. `modules/test-spin` (package `open-mess-test-spin`): `call` loops forever. `modules/test-hog` (package `open-mess-test-hog`): `call` allocates 1 MiB vectors in a loop and keeps them. All with `publish = false` and manifests whose `hosts` contain `localhost` - accepted only under the test address policy.

- [ ] **Step 1:** Write the three crates (same `wit-bindgen` setup as `modules/teams/src/component.rs`).
- [ ] **Step 2:** `cargo xtask ci` builds them; commit `Add test guest modules for the host`.

### Task 3: Actor, limits and crash isolation

**Files:** Create `crates/open-mess-host/src/actor.rs`; modify `runtime.rs`, `limits.rs`.

**Interfaces:**
- Produces: `ModuleHandle::call(&self, account: &str, tool: &str, args_json: &str) -> Result<String, HostError>`; `ModuleHandle::sign_in`, `restore`, `sign_out` mirroring the WIT exports; `ModuleHandle::state(&self) -> ModuleState` (`Running`, `Crashed { count: u32, last: String }`, `Disabled`). The actor reloads the component after a crash or timeout on the next call, until `MAX_CRASHES` consecutive crashes, then `Disabled` (spec 8.3).

- [ ] **Step 1: Failing tests** (`tests/limits.rs`): `infinite_loop_is_interrupted` (test-spin: `call` returns `Timeout` within `CALL_TIMEOUT + 1 s`; use a test-only `limits::for_tests()` with a 2 s timeout to keep the test fast), `memory_hog_is_stopped` (test-hog: `Crashed`), `three_crashes_disable_the_module` (`state() == Disabled` after three hog calls), `echo_round_trips` (test-echo `echo` returns the same JSON), `one_module_crash_does_not_affect_another` (hog crashes, echo still answers).
- [ ] **Step 2-4:** Run (FAIL), implement (epoch deadline + `tokio::time::timeout` around the call; `StoreLimits` for memory; a trapped or timed-out store is dropped), run (PASS).
- [ ] **Step 5:** Commit `Run each module in its own actor with time and memory limits`.

### Task 4: Network policy and `http.fetch`

**Files:** Create `crates/open-mess-host/src/net.rs`, `src/ip_policy.rs`, `src/hosts.rs`.

**Interfaces:**
- Produces: `ip_policy::is_allowed(ip: IpAddr, policy: AddressPolicy) -> bool`; `hosts::HostList::parse(patterns: &[String]) -> Result<HostList, HostError>` (rejects patterns at or above a public suffix); `HostList::allows(&self, host: &str) -> bool`; `Net::new(policy: AddressPolicy) -> Net`; `Net::fetch(&self, hosts: &HostList, req: Request) -> Result<Response, HttpError>` implementing the Global Constraints; `HttpError` mirrors WIT `http-error`.

- [ ] **Step 1: Failing tests:** in `ip_policy`: `blocks_special_ranges` (table: `127.0.0.1`, `0.0.0.0`, `10.1.2.3`, `172.16.0.1`, `192.168.1.1`, `100.64.1.1`, `169.254.1.1`, `224.0.0.1`, `255.255.255.255`, `192.0.2.1`, `198.18.0.1`, `::1`, `::`, `fc00::1`, `fe80::1`, `ff02::1`, `::ffff:192.168.1.1`, `64:ff9b::10.0.0.1` all blocked), `allows_public_unicast` (`8.8.8.8`, `1.1.1.1`, `2606:4700:4700::1111`); in `hosts`: `suffix_pattern_matches_subdomains_only`, `bare_public_suffix_is_rejected` (`*.com`, `*.co.uk`), `match_is_case_insensitive`; `tests/net.rs` (local axum test server, `AllowLoopbackForTests`): `host_not_on_list_is_blocked`, `redirect_is_returned_not_followed`, `redirect_target_is_checked_on_its_own_fetch` (with strict policy the Location to 127.0.0.1 is `blocked`), `oversized_body_is_cut`, `slow_server_times_out`, `long_retry_after_is_throttled` (429 + `Retry-After: 301` -> `throttled(301)`).
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). Wire `http.fetch` in the bindings to `Net::fetch` with the module's `HostList`.
- [ ] **Step 5:** Commit `Enforce the module network policy in http.fetch`.

### Task 5: WebSocket, timers and host events

**Files:** Create `crates/open-mess-host/src/ws.rs`, `src/timers.rs`; modify `actor.rs`, `bindings.rs`.

**Interfaces:**
- Produces: WIT `websocket.open/send/close` with the same host/address checks as Task 4, at most `MAX_SOCKETS` per module, frames above `MAX_WS_MESSAGE` close the socket; incoming text frames and closes are delivered as `events.on-ws-message` / `on-ws-closed` calls through the actor queue (each with the normal call limits). `host.set-timer` (max `MAX_TIMERS`, min 100 ms) delivers `events.on-timer`. `host.message-arrived` and `host.session` become `HostEvent`s on `ModuleContext.events`; `host.log` goes to `tracing` with the module name, level clamped.

- [ ] **Step 1: Failing tests** (`tests/events.rs`, local tungstenite echo server, test policy): `ws_frames_reach_the_module` (test-echo `ws` -> its `on-ws-message` logs `ping`; assert via a captured log/event), `fifth_socket_is_refused`, `timer_fires_once`, `message_arrived_becomes_host_event`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Deliver WebSocket frames, timers and host events to modules`.

### Task 6: Storage, secrets and sign-in stubs

**Files:** Create `crates/open-mess-host/src/storage.rs`, `src/secrets.rs`, `src/sign_in.rs`.

**Interfaces:**
- Produces: storage in `<data_dir>/modules/<name>.sqlite3`, table `kv(key TEXT PRIMARY KEY, value BLOB NOT NULL)`, quota `STORAGE_QUOTA` over the sum of key and value lengths (`set` above it returns `quota-exceeded` and keeps the old value); secrets through keyring, service `open_mess/module/<name>`, user `<account>/<key>` (a module only ever sees its own service name); sign-in imports return `refused("sign-in UI not available")` until milestone E/F2.
- [ ] **Step 1: Failing tests:** `storage_round_trip`, `quota_is_enforced` (test-echo `store` with `size` above the quota -> `quota-exceeded`, previous value intact), `secrets_are_namespaced_by_module` (keyring mock store: module `a` cannot read a secret written by module `b` with the same account and key), `sign_in_is_refused_for_now`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Give modules storage with a quota, namespaced secrets and sign-in stubs`.
