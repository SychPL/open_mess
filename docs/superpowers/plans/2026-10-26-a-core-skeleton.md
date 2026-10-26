# Milestone A: core skeleton - implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `om daemon` runs as a single instance on a fixed localhost port, guards its API with a bearer token, keeps state in SQLite, and serves one tool registry over MCP (Streamable HTTP) and the `om` CLI, starting with the `status` tool.

**Architecture:** One binary crate `crates/open-mess` (binary `om`). The daemon is a tokio + axum server on `127.0.0.1:<port>` with two routes: `/mcp` (rmcp Streamable HTTP server) and `/api/tools` + `/api/call/<tool>` (plain JSON for the CLI). Both read one `Registry` of tools. The CLI is a thin reqwest client of `/api`.

**Tech Stack:** Rust 2024, tokio, axum 0.8, rmcp 3.x (server + Streamable HTTP; client in tests), clap 4, rusqlite (bundled), keyring 4 (Windows Credential Manager), reqwest 0.13, serde/serde_json, getrandom. Use the latest stable versions at implementation time and check rmcp's and keyring's docs.rs for the exact feature names and APIs of that version - they changed across major versions.

**Spec:** `.specs/2026-10-26-open-mess-design.md` (sections 8.1, 8.2, 8.4, 8.5 `status`, 15).

## Global Constraints

- No `unwrap` / `expect` / `panic!` / `todo!` / `unreachable!` / unchecked indexing in non-test code; no `unsafe` (workspace lints, AGENTS.md "Never crash").
- The local API listens on `127.0.0.1` only, never `0.0.0.0` (spec 8.1).
- Every request to `/mcp` and `/api/*` needs `Authorization: Bearer <token>`; missing or wrong token is HTTP 401 (spec 8.1).
- The token is random (32 bytes from the OS RNG, hex-encoded), generated at the daemon's first start and stored in Windows Credential Manager under service `open_mess`, user `local-api` (spec 8.1, 8.4).
- The port is fixed: default `7341`, stored in the SQLite `config` table under key `port`; if it is taken, the daemon exits with an error that names the port - it never picks another (spec 8.1).
- At most one daemon per Windows user: a second `om daemon` prints `open_mess daemon is already running` and exits with code 1 (spec 8.1).
- Data folder `%APPDATA%\open_mess\` (state DB `state.sqlite3`, lock file `daemon.lock`); `OPEN_MESS_DATA_DIR` overrides it (tests, portable use).
- `OPEN_MESS_TOKEN` and `OPEN_MESS_PORT` override the stored token / port for the CLI (scripts, tests). The daemon never reads `OPEN_MESS_TOKEN`.
- Tool names, parameters and results are identical over MCP and the CLI (AGENTS.md "One tool registry").
- No logging of the token or of request bodies.
- Raise `rust-version` in the workspace `Cargo.toml` to `1.89` (needed for `std::fs::File::try_lock`).

## Review Focus

- Two `om daemon` started at the same moment: exactly one keeps running; the other exits 1 with the "already running" message (Task 2 test `second_lock_fails`).
- Port already taken by another program: daemon exits non-zero, message contains the port number, no fallback port (Task 4 test `port_in_use_is_an_error`).
- Request from a browser page (CSRF / DNS rebinding) without the token: 401, tool not run (Task 4 tests `missing_token_is_401`, `wrong_token_is_401`).
- CLI with the daemon not running: clear message `open_mess daemon is not running - start it with: om daemon`, exit code 2, no hang (Task 6 test `cli_without_daemon`).
- Corrupt or newer-version SQLite file (user_version above the known one): daemon refuses to start with a clear error instead of rewriting it (Task 3 test `newer_schema_is_refused`).

---

### Task 1: Crate layout and dependencies

**Files:**
- Modify: `Cargo.toml` (workspace: `rust-version = "1.89"`)
- Modify: `crates/open-mess/Cargo.toml` (dependencies)
- Modify: `crates/open-mess/src/main.rs`
- Create: `crates/open-mess/src/lib.rs` (modules `paths`, `instance`, `store`, `secrets`, `tools`, `api`, `cli`)

**Interfaces:**
- Produces: library crate `open_mess` (so integration tests in `crates/open-mess/tests/` can use it) and binary `om` whose `main` calls `open_mess::cli::run() -> std::process::ExitCode`.

- [ ] **Step 1:** Add dependencies (tokio with `rt-multi-thread, macros, net, signal`, axum, rmcp with server + Streamable HTTP server features, clap with `derive`, rusqlite with `bundled`, keyring with its Windows native store feature, reqwest with `json`, serde with `derive`, serde_json, getrandom, thiserror). Dev-dependencies: rmcp client + Streamable HTTP client features, tempfile, tokio `test-util`.
- [ ] **Step 2:** `lib.rs` declares the modules (empty files compile); `main.rs` becomes `fn main() -> ExitCode { open_mess::cli::run() }` with `cli::run` printing `om <version>` for now.
- [ ] **Step 3:** Run `cargo xtask ci`. Expected: green.
- [ ] **Step 4:** Commit `Set up the om crate layout and dependencies`.

### Task 2: Data folder and single instance

**Files:**
- Create: `crates/open-mess/src/paths.rs`, `crates/open-mess/src/instance.rs`
- Test: in-module `#[cfg(test)]` tests

**Interfaces:**
- Produces: `paths::data_dir() -> Result<PathBuf, Error>` (`OPEN_MESS_DATA_DIR` or `%APPDATA%\open_mess`, created if missing); `instance::InstanceLock::acquire(dir: &Path) -> Result<InstanceLock, Error>` holding an exclusive `std::fs::File::try_lock` on `<dir>/daemon.lock` for its lifetime; `Error::AlreadyRunning`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn second_lock_fails() {
    let dir = tempfile::tempdir().unwrap();
    let _first = InstanceLock::acquire(dir.path()).unwrap();
    assert!(matches!(InstanceLock::acquire(dir.path()), Err(Error::AlreadyRunning)));
}

#[test]
fn lock_is_released_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    drop(InstanceLock::acquire(dir.path()).unwrap());
    assert!(InstanceLock::acquire(dir.path()).is_ok());
}
```

- [ ] **Step 2:** Run `cargo test -p open-mess instance`. Expected: FAIL (not defined).
- [ ] **Step 3:** Implement `paths::data_dir` and `InstanceLock::acquire` (`TryLockError::WouldBlock` maps to `AlreadyRunning`).
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5:** Commit `Add the data folder and the single-instance lock`.

### Task 3: State store

**Files:**
- Create: `crates/open-mess/src/store.rs`

**Interfaces:**
- Produces: `store::Store::open(path: &Path) -> Result<Store, Error>`, `Store::get_config(&self, key: &str) -> Result<Option<String>, Error>`, `Store::set_config(&self, key: &str, value: &str) -> Result<(), Error>`, `Store::port(&self) -> Result<u16, Error>` (stored value or `DEFAULT_PORT = 7341`), `Error::SchemaTooNew { found: i64, known: i64 }`. Schema version kept in `PRAGMA user_version`; version 1 = table `config(key TEXT PRIMARY KEY, value TEXT NOT NULL)`. Later milestones append migrations to one ordered list in this file.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn port_defaults_to_7341_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let store = Store::open(&path).unwrap();
    assert_eq!(store.port().unwrap(), 7341);
    store.set_config("port", "7400").unwrap();
    drop(store);
    assert_eq!(Store::open(&path).unwrap().port().unwrap(), 7400);
}

#[test]
fn newer_schema_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    rusqlite::Connection::open(&path).unwrap().pragma_update(None, "user_version", 999).unwrap();
    assert!(matches!(Store::open(&path), Err(Error::SchemaTooNew { found: 999, .. })));
}

#[test]
fn bad_port_value_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
    store.set_config("port", "not-a-port").unwrap();
    assert!(store.port().is_err());
}
```

- [ ] **Step 2:** Run `cargo test -p open-mess store`. Expected: FAIL.
- [ ] **Step 3:** Implement `Store` (migrations run in one transaction; a stored port outside `1..=65535` or non-numeric is `Error::BadConfig`).
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5:** Commit `Add the SQLite state store with schema versioning`.

### Task 4: Token, tool registry and the HTTP API

**Files:**
- Create: `crates/open-mess/src/secrets.rs`, `crates/open-mess/src/tools.rs`, `crates/open-mess/src/api.rs`
- Test: `crates/open-mess/tests/api.rs`

**Interfaces:**
- Produces:
  - `secrets::local_api_token() -> Result<String, Error>`: reads Credential Manager (`open_mess` / `local-api`), generates and stores 64 hex chars (32 random bytes) when absent.
  - `tools::Registry::with_builtin(started: std::time::Instant) -> Registry`; `Registry::list(&self) -> Vec<ToolInfo>`; `Registry::call(&self, name: &str, args: serde_json::Value) -> Result<serde_json::Value, ToolError>`; `ToolInfo { name: String, description: String, input_schema: serde_json::Value }`; `ToolError::{UnknownTool(String), InvalidArgs(String)}`.
  - Built-in tool `status`, no parameters, result `{"version": "<CARGO_PKG_VERSION>", "uptime_seconds": <u64>, "modules": [], "accounts": []}`.
  - `api::serve(listener: tokio::net::TcpListener, token: String, registry: Arc<Registry>, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<(), Error>`; routes `GET /api/tools` (`[ToolInfo]`), `POST /api/call/{tool}` (body = args JSON, 200 with result, 404 unknown tool, 400 invalid args), `/mcp` (rmcp Streamable HTTP; `tools/list` and `tools/call` delegate to the same `Registry`). A bearer-token middleware wraps all three; token comparison is constant-time.
  - `api::bind(port: u16) -> Result<TcpListener, Error>` binding `127.0.0.1:<port>`; `Error::PortInUse(u16)` displayed as `port <n> is already in use - change it in the configuration window or set another port`.

- [ ] **Step 1: Write the failing tests** (in `tests/api.rs`; a helper starts `serve` on `bind(0)`-style ephemeral port `127.0.0.1:0` with token `"t0ken"`)

```rust
#[tokio::test] async fn missing_token_is_401() { /* GET /api/tools without header -> 401 */ }
#[tokio::test] async fn wrong_token_is_401() { /* Bearer nope -> 401, also on POST /mcp */ }
#[tokio::test] async fn status_over_api() {
    // POST /api/call/status with Bearer t0ken -> 200,
    // body["version"] == env!("CARGO_PKG_VERSION"), body["modules"] == json!([]), body["accounts"] == json!([])
}
#[tokio::test] async fn unknown_tool_is_404() { /* POST /api/call/nope -> 404 */ }
#[tokio::test] async fn status_over_mcp() {
    // rmcp client over Streamable HTTP with the bearer header:
    // list_tools contains "status"; call_tool("status") result JSON equals the /api result's keys
}
#[tokio::test] async fn port_in_use_is_an_error() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let err = open_mess::api::bind(port).await.unwrap_err();
    assert!(err.to_string().contains(&port.to_string()));
}
#[tokio::test] async fn listens_on_loopback_only() {
    let l = open_mess::api::bind(0).await.unwrap();
    assert!(l.local_addr().unwrap().ip().is_loopback());
}
```

Write the bodies as described in the comments, with real assertions.

- [ ] **Step 2:** Run `cargo test -p open-mess --test api`. Expected: FAIL.
- [ ] **Step 3:** Implement `secrets`, `tools` and `api` as specified above.
- [ ] **Step 4:** Run the tests. Expected: PASS. Also `cargo test -p open-mess secrets` for a unit test `token_is_generated_once` using keyring's mock credential store (two calls return the same 64-hex-char string).
- [ ] **Step 5:** Commit `Serve the tool registry over MCP and the local API`.

### Task 5: `om daemon`

**Files:**
- Create: `crates/open-mess/src/daemon.rs`
- Modify: `crates/open-mess/src/cli.rs`, `crates/open-mess/src/lib.rs`

**Interfaces:**
- Consumes: Tasks 2-4.
- Produces: `daemon::run() -> Result<(), Error>`: `data_dir` -> `InstanceLock::acquire` -> `Store::open(<dir>/state.sqlite3)` -> `local_api_token` -> `api::bind(store.port()?)` -> `api::serve` until Ctrl+C. Prints `open_mess daemon listening on http://127.0.0.1:<port>/mcp` once bound.

- [ ] **Step 1: Write the failing test** `crates/open-mess/tests/daemon.rs`, `#[cfg(windows)]` (the daemon reads its token from the real Credential Manager in a child process; Linux CI has no keyring backend): pre-create the store in a temp `OPEN_MESS_DATA_DIR` with a free port, start `env!("CARGO_BIN_EXE_om") daemon`, wait for the `listening` line, start a second `om daemon` with the same data dir and assert exit code 1 and stderr containing `already running`, then kill the first.

- [ ] **Step 2:** Run it. Expected: FAIL.
- [ ] **Step 3:** Implement `daemon::run` and the `daemon` subcommand.
- [ ] **Step 4:** Run it. Expected: PASS.
- [ ] **Step 5:** Commit `Run the daemon as a single instance on the configured port`.

### Task 6: CLI generated from the registry

**Files:**
- Modify: `crates/open-mess/src/cli.rs`
- Test: `crates/open-mess/tests/cli.rs`

**Interfaces:**
- Consumes: `/api/tools`, `/api/call/{tool}` from Task 4.
- Produces: `om <tool> [--<param> <value>]... [--json-args '<json>'] [--json]`. The CLI fetches `/api/tools`, validates the tool name, builds the args object from `--<param> <value>` pairs (values parsed as JSON when they parse, else strings) or takes `--json-args` verbatim, calls `/api/call/<tool>`, and prints the result: pretty JSON with `--json`, otherwise one `key: value` line per top-level field. Token from `OPEN_MESS_TOKEN` else Credential Manager; port from `OPEN_MESS_PORT` else the store in the data dir. `om daemon`, `om --version` and `om mcp-command` are the only non-tool commands; `om mcp-command` prints the ready `claude mcp add ...` command with the URL and the bearer token (read from Credential Manager), for the user to paste - the token is shown only there, never logged.
- Exit codes: 0 success, 1 tool error (message from the API), 2 daemon not running (`open_mess daemon is not running - start it with: om daemon`), 3 bad usage (unknown tool, malformed args).

- [ ] **Step 1: Write the failing tests** (`tests/cli.rs`; each starts the API in-process with `serve` on an ephemeral port and token `t0ken`, then runs the `om` binary with `OPEN_MESS_PORT` / `OPEN_MESS_TOKEN`)

```rust
#[tokio::test] async fn cli_status_json() {
    // om status --json -> exit 0, stdout parses as JSON, ["version"] == CARGO_PKG_VERSION
}
#[tokio::test] async fn cli_unknown_tool() { /* om nope -> exit 3, stderr mentions "nope" */ }
#[test] fn cli_without_daemon() {
    // OPEN_MESS_PORT=<a free port nobody listens on> om status -> exit 2 within 5 s,
    // stderr contains "open_mess daemon is not running"
}
#[tokio::test] async fn cli_bad_token() { /* OPEN_MESS_TOKEN=wrong om status -> exit 1, stderr mentions 401 or "token" */ }
```

- [ ] **Step 2:** Run `cargo test -p open-mess --test cli`. Expected: FAIL.
- [ ] **Step 3:** Implement the CLI as specified (reqwest with a 5 s connect timeout).
- [ ] **Step 4:** Run the tests. Expected: PASS. Then `cargo xtask ci`. Expected: green.
- [ ] **Step 5:** Commit `Generate the om CLI from the tool registry`.

### Task 7: Docs

**Files:**
- Modify: `README.md` (a short "Try it" section: `cargo run -p open-mess -- daemon`, `om status`, `om mcp-command` to get the ready `claude mcp add` command), `CONTRIBUTING.md` (layout table: the `crates/open-mess` modules).

- [ ] **Step 1:** Update both files (English, plain hyphens). Check the exact `claude mcp add` flags with `claude mcp add --help` and use them in `om mcp-command` (Task 6) and the README.
- [ ] **Step 2:** `cargo xtask ci`. Expected: green.
- [ ] **Step 3:** Commit `Document running the daemon and connecting MCP clients`.
