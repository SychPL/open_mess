# Self-update from signed GitHub releases - implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** open_mess finds, downloads and verifies a newer signed release of `SychPL/open_mess`, and after the user clicks "Zainstaluj" a separate helper swaps `om.exe` and modules, checks the new daemon and rolls everything back if it fails.

**Architecture:** a new `crates/open-mess/src/update/` module. The daemon side checks and downloads (`fetch.rs`), verifies (`release.rs`), records consent (`consent.rs`), quiesces itself in two phases (`quiesce.rs`) and launches the helper task; the helper is the same binary copied to `bin\om.update.exe`, run as `om.update.exe apply-update --data-dir <dir>` by the scheduled task `open_mess-update`, and drives a stage machine in `update-pending.json` (`state.rs`, `apply.rs`). Windows specifics (Task Scheduler, stopping processes, polling `status`) sit behind two small traits so the logic is tested with fakes; the real implementations shell out to `schtasks.exe` and `powershell.exe` (no new `unsafe`).

**Tech Stack:** what the core uses today, plus `minisign-verify` (core, verify only), `minisign` (xtask and core dev-dependency, keygen and signing), `sha2` and `serde_json` in xtask; core `reqwest` gains the TLS and `system-proxy` features the host crate already enables on the same version.

**Spec:** `.specs/2026-10-27-self-update.md` (sections 4-11; section and point numbers below refer to it).

## Global Constraints

- Repository `SychPL/open_mess`; check URL `https://api.github.com/repos/SychPL/open_mess/releases/latest`, no token, `User-Agent: open_mess/<version>`, timeout 30 s; skip `draft` and `prerelease`.
- Version: tag `v<MAJOR>.<MINOR>.<PATCH>` (digits only); a candidate must be strictly higher than `env!("CARGO_PKG_VERSION")`; manifest `version` must equal the tag.
- Only HTTPS to `api.github.com`, `github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com` (redirects included), system proxy and certificate store as in `open-mess-host/src/net.rs`.
- `manifest.json` and `manifest.json.minisig` at most 64 KiB each; file names only `om.exe` or `<name>.wasm` with `<name>` matching `[a-z0-9-]{1,32}`; total of all files at most 256 MiB; size and SHA-256 must match the manifest.
- Downloads go to `%LOCALAPPDATA%\open_mess\updates\<version>\`; partial files are deleted; nothing there runs before full verification.
- Checks: 5 minutes after daemon start, then every 24 h, plus "Sprawdź teraz".
- Consent: one row in SQLite (version, manifest SHA-256, click time, launch time, use time), written only by the window or the toast; valid 10 minutes from helper launch; single use. No MCP tool or CLI command writes consent or the auto-install option.
- Phase A: modules paused, event chain drained, no open or delivering batch, no running or queued spawn, no `approved` action, 60 s since the last delivery and the last tool call; gives up after 30 min and goes to phase B. Phase B: every new tool call fails with `update in progress, try again shortly`; ends at zero in-flight tool calls and no `waiting` or `approved` action.
- Helper launch: the daemon exits (code 0) only once `update.lock` is held; after 30 s without it the daemon takes the lock itself, deletes `update-pending.json` and resumes ("pomocnik nie wystartował").
- Helper: takes `<data dir>\update.lock` first (retries for 2 s, then exits untouched), never runs migrations (raw `rusqlite::Connection`, no `Store::open`), waits 60 s for `daemon.lock`, then stops the daemon.
- Health check after install: within 60 s the API answers, `status.ui_ready` is true, every module listed in `active_modules` is `Active` with `crashes_since_load == 0`; account restore (`status.accounts_restored`) gets up to 10 minutes from daemon start (a fixed cap: timer events run between account restores, so a cap from the account count is too short). Account online state is not a criterion.
- Tasks: `open_mess` (logon + every 5 min, exists) and `open_mess-update` = `conhost.exe --headless %LOCALAPPDATA%\open_mess\bin\om.update.exe apply-update --data-dir <data dir>`, logon + every 5 min, `IgnoreNew`, re-registered by the daemon before every launch.
- "Stop the daemon" = `schtasks /End /TN open_mess`, then kill every `om.exe` process of this user whose command line matches `\bdaemon\b` (the same filter as `scripts/install.ps1`). "Start the daemon" = `schtasks /Run /TN open_mess`.
- Window texts in Polish (as the spec quotes them), tool errors and code in English, plain hyphens, no panics, no new `unsafe`.

## Review Focus

- A release whose asset URL redirects to a host outside the list, or to plain HTTP: refused before any byte is written (Task 3 test `redirect_to_other_host_is_refused`).
- Power loss between the two renames of `om.exe` (no `bin\om.exe` at all): the next helper run restores it and starts the daemon (Task 8 test `missing_om_exe_after_crash_is_restored`).
- The daemon killed in the middle of a write leaves a hot `state.sqlite3-journal`: the backup includes it, and restore puts back exactly the backed-up set and deletes `-journal`/`-wal`/`-shm` files that are not in it (Task 8 test `restore_puts_back_journal_and_removes_new_ones`).
- An agent keeps calling tools every 30 s during phase A: phase A never sees 60 s of quiet, gives up at 30 min and phase B starts (Task 7 test `phase_a_gives_up_after_30_min`).
- Disk full or connection dropped mid-download: the partial file and the version folder are removed and the window shows the error (Task 3 test `partial_download_is_removed`).

---

### Task 1: Release format and signature check

**Files:** Create `crates/open-mess/src/update/mod.rs`, `update/release.rs`, `keys/release.pub` (placeholder: `untrusted comment: no release key yet - run cargo xtask release-keygen`); modify `crates/open-mess/src/lib.rs`, `crates/open-mess/Cargo.toml` (`minisign-verify`; dev-dep `minisign`).

**Interfaces:**
- Produces: `update::UpdateError { Rejected(String), Network(String), Io(#[from] std::io::Error), Store(#[from] rusqlite::Error) }`; `release::Version(u64, u64, u64)` with `parse(&str)`, `from_tag(&str)` (`v` prefix required), `current()`, `Ord`, `Display`; `release::Manifest { version: String, files: Vec<FileEntry> }`, `FileEntry { name: String, kind: Kind, size: u64, sha256: String }`, `Kind { Exe, Module }`; `pub const RELEASE_KEY: &str = include_str!("../../../../keys/release.pub");` `pub fn verify(manifest: &[u8], signature: &str, public_key: &str) -> Result<Manifest, UpdateError>` (signature first, then JSON, then: names on the whitelist and unique, `om.exe` iff kind `Exe`, 64-hex `sha256`, total size cap); `pub fn manifest_sha256(bytes: &[u8]) -> String`.
- [ ] **Step 1: Failing tests** (`update/release.rs` unit tests, key pair generated with `minisign` in the test): `good_signature_parses`, `bad_signature_is_rejected`, `missing_key_is_rejected` (the placeholder file), `name_whitelist` (`../om.exe`, `Teams.wasm`, `a\\b.wasm`, 33-char name, `om.dll` rejected; `teams.wasm`, `test-x9.wasm` accepted), `duplicate_names_are_rejected`, `total_over_256_mib_is_rejected`, `version_order` (`0.10.0 > 0.9.9`, `v1.2` and `1.2.3-rc1` rejected).
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** `cargo xtask ci`, commit `Verify signed release manifests`.

### Task 2: `cargo xtask release-keygen` and `cargo xtask release`

**Files:** Modify `xtask/src/main.rs`, `xtask/Cargo.toml` (`minisign`, `sha2`, `serde_json`), `CONTRIBUTING.md` (release section).

**Interfaces:**
- Consumes: the manifest JSON shape and whitelist of Task 1 (duplicated in xtask as plain functions: xtask does not depend on `open-mess`).
- Produces: `release-keygen` writes `keys/release.pub` and the secret key to `OPEN_MESS_RELEASE_KEY` or `%USERPROFILE%\.open_mess\release.key` (password prompted by `minisign`, refuses to overwrite either file). `release <x.y.z>` does spec 4 steps 1-6 in order and stops at the first failure; pure helpers `fn unprocessed_commits(shas: &[String], reviews_dir: &Path) -> Vec<String>` (7-char `.reviews/<sha>.processed`), `fn only_version_changed(diff: &str) -> bool` (the Release commit touches only `Cargo.toml` `version =` lines and `Cargo.lock`), `fn build_manifest(version: &str, files: &[(String, Vec<u8>)]) -> String`.
- [ ] **Step 1: Failing tests** (xtask unit tests): `unprocessed_commits_are_listed`, `release_diff_with_other_changes_is_refused`, `manifest_lists_sizes_and_sums`, `test_guests_are_not_released` (`open_mess_test_echo.wasm` skipped, `open_mess_teams.wasm` -> `teams.wasm`).
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). Do not run `release` or `release-keygen` for real: the owner does, with a password (see Task 12).
- [ ] **Step 5:** `cargo xtask ci`, commit `Add release and release-keygen to xtask`.

### Task 3: Check and download

**Files:** Create `crates/open-mess/src/update/fetch.rs`; modify `crates/open-mess/Cargo.toml` (`reqwest` features `rustls-no-provider` and `system-proxy`, matching `open-mess-host`; install the ring provider as `Net::build` does); test `crates/open-mess/tests/update_fetch.rs` (axum fake on `127.0.0.1:0`).

**Interfaces:**
- Consumes: Task 1.
- Produces: `fetch::Source { latest: Url, allowed_hosts: Vec<String>, https_only: bool }` with `Source::github()`; `fn client(source: &Source) -> Result<reqwest::Client, UpdateError>` (custom redirect policy: at most 5 hops, every hop on `allowed_hosts`, HTTPS when `https_only`); `async fn latest(client, source, current: Version) -> Result<Option<Candidate>, UpdateError>`, `Candidate { version: Version, assets: Vec<(String, Url)> }` (`None` when draft, prerelease or not newer; each asset's `browser_download_url` must be on `allowed_hosts`); `async fn download(client, candidate, public_key: &str, root: &Path) -> Result<Downloaded, UpdateError>`, `Downloaded { version: Version, manifest_sha256: String, dir: PathBuf }` - manifest and signature first (64 KiB cap each), then each file streamed with `Response::chunk()` into `<root>\<version>\<name>.part` with a running SHA-256 and size cap, renamed only when both match; any error removes `<root>\<version>`.
- [ ] **Step 1: Failing tests:** `newer_release_downloads_and_verifies`, `equal_or_lower_version_is_ignored`, `draft_and_prerelease_are_ignored`, `tag_and_manifest_version_must_match`, `missing_signature_is_rejected`, `wrong_sum_is_rejected`, `file_larger_than_manifest_is_cut_off`, `redirect_to_other_host_is_refused`, `partial_download_is_removed` (server drops the connection mid-file), `asset_not_in_manifest_is_not_downloaded`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** `cargo xtask ci`, commit `Check GitHub releases and download verified updates`.

### Task 4: Consent, settings and a port read without migration

**Files:** Create `crates/open-mess/src/update/consent.rs`; modify `store.rs` (migration 8: `CREATE TABLE update_consent(version TEXT NOT NULL, manifest_sha256 TEXT NOT NULL, clicked_ms INTEGER NOT NULL, launched_ms INTEGER, used_ms INTEGER)`; `pub fn read_port(db: &Path) -> Result<u16, Error>` opening `SQLITE_OPEN_READ_ONLY`, `SELECT value FROM config WHERE key = 'port'`, `DEFAULT_PORT` when the file or key is missing), `cli.rs` (`port()` uses `read_port` instead of `Store::open`, spec 7.7).

**Interfaces:**
- Produces (all on `&rusqlite::Connection`, so the helper uses them without `Store::open`): `record_click(conn, version, manifest_sha256, now_ms)` (replaces any earlier row), `mark_launched(conn, version, sha, now_ms)`, `valid(conn, version, sha, now_ms) -> Result<bool>` (launched, unused, `now - launched <= 600_000`), `mark_used(conn, version, sha, now_ms)`; `auto_install(conn) -> Result<bool>` / `set_auto_install(conn, bool)` (config key `update_auto_install`); `record_rolled_back(conn, version, sha)` / `rolled_back(conn, version, sha) -> Result<bool>` (config key `update_rolled_back:<version>`).
- [ ] **Step 1: Failing tests:** `consent_expires_ten_minutes_after_launch`, `consent_without_launch_is_invalid`, `consent_is_single_use`, `consent_for_other_manifest_is_invalid`, `rolled_back_release_is_remembered`, `port_is_read_from_a_newer_schema` (set `user_version` to 99: `Store::open` fails with `SchemaTooNew`, `read_port` returns the stored port), and in `tests/cli.rs` `cli_reaches_daemon_with_newer_schema`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** `cargo xtask ci`, commit `Store update consent and read the port without migrating`.

### Task 5: Pause module events and count crashes since load

**Files:** Modify `crates/open-mess-host/src/actor.rs`, `runtime.rs`; test `crates/open-mess-host/tests/` (the file that already covers timers and events with the test guests).

**Interfaces:**
- Produces: `ModuleHandle::pause_events(&self) -> Result<(), HostError>` returns once the actor has stopped pulling guest events (the actor runs one call or event at a time, so no event handler is running when it answers); `ModuleHandle::resume_events(&self)`; `ModuleHandle::crashes_since_load(&self) -> u32` (a counter never reset by a successful call, unlike `crashes`; reset only when a new instance is loaded).
- Plan decision: pausing means the actor stops calling `next_event` while timers and sockets stay as they are; their sends already `await` on the bounded `EVENT_QUEUE`, so nothing is lost and resume delivers what queued. On the update path the daemon exits and the process end closes the connections, which is what spec 7.1 asks; on cancel nothing has to reconnect.
- [ ] **Step 1: Failing tests:** `paused_module_gets_no_timer_events`, `resume_delivers_queued_events`, `pause_waits_for_a_running_event_handler`, `tool_calls_work_while_paused`, `crashes_since_load_survive_a_successful_call`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** `cargo xtask ci`, commit `Pause module events and count crashes since load`.

### Task 6: Activity counters, tool gate and new `status` fields

**Files:** Modify `tools/mod.rs`, `triggers/engine.rs`, `triggers/mod.rs`, `events.rs`, `pending.rs`, `daemon.rs`, `ui/window.rs`; tests in `tests/shared_tools.rs`, `tests/triggers.rs`, `tests/pending.rs`.

**Interfaces:**
- Produces: `tools::Gate` (held by `Registry`) with `in_flight() -> usize` (guard around the whole `Registry::call`, like `api.rs` `Busy`), `last_end_ms() -> u64`, `set(GateMode)`, `GateMode { Open, WritesBlocked, Closed }`; `Closed` fails every call and `WritesBlocked` every write tool with `ToolError::Failed("update in progress, try again shortly")`, before `pending.create`. `triggers::Activity { received: AtomicU64, consumed: AtomicU64, open_batches: AtomicUsize, delivering: AtomicUsize, last_delivery_ms: AtomicU64, hold: watch::Sender<bool> }`: `Bus::forward` counts `received` right after `recv`, the engine counts `consumed` (plus `n` on `Lagged(n)`), stores `open.len()` after each loop turn, guards each spawned delivery, and while `hold` is true flushes nothing and waits on `hold.changed()` or the next event instead of a past deadline. `Pending::count(state: State) -> Result<usize, Error>`. `daemon.rs` keeps the `Runs`, `triggers::Activity` and the host event sender reachable (today the `Runs` Arc and the engine handle are dropped after spawning). `status` gains `ui_ready` (set when the Tauri app has finished setup with the tray), `accounts_restored` (set when the spawned `accounts.restore()` returns, success or not), `modules[].crashes_since_load`.
- [ ] **Step 1: Failing tests:** `closed_gate_rejects_every_call`, `blocked_writes_reject_write_tools_only`, `in_flight_counts_a_running_call`, `held_engine_keeps_due_batches_and_flushes_on_release`, `consumed_matches_received_after_drain`, `count_by_state`, `status_reports_restore_and_crashes`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** `cargo xtask ci`, commit `Track tool and trigger activity for updates`.

### Task 7: Two phases and helper launch in the daemon

**Files:** Create `update/state.rs`, `update/quiesce.rs`, `update/system.rs` (Windows implementations, `#[cfg(windows)]`); modify `daemon.rs` (an exit `Notify` joined into `until_stopped`); test `tests/update_quiesce.rs` (tokio paused clock, fakes).

**Interfaces:**
- Consumes: Tasks 4-6.
- Produces: `state::Paths { data: PathBuf, local: PathBuf }` with `bin()`, `updates()`, `pending_file()` (`<data>\update-pending.json`), `lock_file()` (`<data>\update.lock`), `last_result()` (`<data>\last-update.txt`), `Paths::current()`; `state::PendingFile { version, manifest_sha256, update_dir, stage: Stage, active_modules: Vec<String>, added: Vec<PathBuf>, replaced: Vec<PathBuf> }`, `Stage { Started, BackedUp, Installed, RollingBack }` (serde kebab-case); `state::read(path) -> Read { Missing, Unreadable, Found(PendingFile) }`; `state::write(path, &PendingFile) -> io::Result<()>` (temp file + `fs::rename`); `state::try_lock(path, wait: Duration) -> io::Result<Option<File>>` (`File::try_lock`, retried every 100 ms); `state::helper_alive(paths) -> bool`; `state::record_result(paths, text)`. `quiesce::Activity { gate, triggers, pending, runs, modules: Vec<ModuleHandle>, host_events: mpsc::Sender<HostEvent> }`; `async fn finish_work(&Activity, cancel: &watch::Receiver<bool>) -> Result<(), Cancelled>` (phase A per Global Constraints; queue drained = host sender at full capacity and `received == consumed` on two checks 200 ms apart); `async fn switch(&Activity, cancel) -> Result<(), Cancelled>` (phase B); `fn resume(&Activity)` (gate `Open`, `resume_events` on every module). `trait Launcher { fn launch_helper(&self, helper: &Path, data: &Path) -> Result<(), UpdateError>; }` - the real one writes the task XML (UTF-16 LE with BOM, triggers logon and every 5 min, `IgnoreNew`) and runs `schtasks /Create /XML <file> /TN open_mess-update /F` then `schtasks /Run /TN open_mess-update`, `CREATE_NO_WINDOW` as in `triggers/spawn.rs`. `async fn launch(paths, downloaded, active_modules, &dyn Launcher, exit: &Notify) -> Result<(), UpdateError>` (copy `om.exe` to `bin\om.update.exe`, check its SHA-256, write `PendingFile` stage `Started`, launch, wait 30 s for `helper_alive`; on timeout take the lock, delete the file, return `Err` "pomocnik nie wystartował").
- [ ] **Step 1: Failing tests:** `phase_a_waits_for_an_open_batch`, `phase_a_waits_for_60_s_of_quiet`, `phase_a_waits_for_a_running_spawn`, `phase_a_gives_up_after_30_min`, `phase_b_rejects_new_calls_and_waits_for_in_flight`, `phase_b_waits_for_waiting_actions`, `cancel_resumes_events_and_calls`, `exits_once_update_lock_is_held`, `helper_not_started_resumes_after_30_s`, `pending_file_write_is_atomic` (a reader never sees a half file).
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** `cargo xtask ci`, commit `Quiesce the daemon and launch the update helper`.

### Task 8: `apply-update` helper

**Files:** Create `update/apply.rs`; modify `cli.rs` (`#[command(hide = true)] ApplyUpdate { #[arg(long)] data_dir: PathBuf }`, declared before `Tool`), `modules.rs` (`approve` and `remember_approved` take `&rusqlite::Connection`; extract from module-level `load` a `pub(crate) fn inspect(engine, path) -> Result<Inspected { file_sha256, manifest, manifest_sha256 }, Error>`; `pub(crate) fn version_only_change(a: &Manifest, b: &Manifest) -> bool` = `canonical` without `version` equal), `modules/test-echo/src/lib.rs` (padded `VERSION` const like `HOSTS`), `tests/common/mod.rs` (`echo_with_version(&str)`); test `tests/update_apply.rs`.

**Interfaces:**
- Consumes: Tasks 1, 4, 7.
- Produces: `trait System { fn wait_daemon_lock_free(&self, wait: Duration) -> bool; fn stop_daemon(&self) -> Result<(), UpdateError>; fn start_daemon(&self) -> Result<(), UpdateError>; fn health(&self, active_modules: &[String]) -> Health; }`, `Health { Healthy, Failed(String) }`; `fn run(paths: &Paths, system: &dyn System, public_key: &str, now: fn() -> i64) -> Outcome`, `Outcome { Nothing, Installed, RolledBack(String), Aborted(String), RollbackFailed(String) }`, each written to `last-update.txt`; `pub fn judge(status: &Value, active_modules: &[String]) -> Judgement { Healthy, Pending { accounts_only: bool }, Failed(String) }` (shared with Task 9; a module with `crashes_since_load > 0` fails at once). The real `System` (in `system.rs`) stops and starts as in Global Constraints and polls `status` every second over the local API (port from `read_port`, token from `secrets::stored_local_api_token`).
- Stage machine (spec 7.2, 7.3, 7.6): no file -> `Nothing`; unreadable -> rollback if `<update_dir>\backup` exists, else delete and start; `Started` -> needs `consent::valid`, else delete, record "przerwano: brak ważnej zgody", start the daemon if `daemon.lock` is free; otherwise wait 60 s for the lock, stop, `mark_used`, re-verify signature and sums, back up (`state.sqlite3`, `modules\*.sqlite3`, `*.wasm`, their `-journal`/`-wal` when present, `bin\om.exe`) -> `BackedUp`; swap `om.exe` (`om.new.exe`, current -> `om.old-<ms>.exe`, new -> `om.exe`); place modules (predecessor by manifest name and approved hashes; version-only change replaces under the predecessor's file name via `<file>.new` and approves; else `<name>-<version>[-<n>].wasm` alongside; every added path written to `added` before the copy) -> `Installed` -> start -> `health` -> delete the file on success. `BackedUp`, `Installed`, `RollingBack` found at start, a failed step or `Health::Failed` -> write `RollingBack`, stop, restore every backed-up file through `<file>.tmp` + rename (a busy `om.exe` first renamed to `om.failed-<ms>.exe`), delete files in `added` and journal files not in the backup, then `record_rolled_back`, delete the file, start.
- [ ] **Step 1: Failing tests** (fake `System`, temp `data` and `local` dirs, `echo` variants): `no_pending_file_touches_nothing`, `started_without_consent_aborts_and_starts_daemon`, `second_helper_exits_while_lock_is_held`, `install_swaps_exe_and_approves_version_only_module`, `changed_manifest_goes_alongside_and_waits`, `occupied_name_gets_a_numbered_file`, `failed_health_rolls_back_everything`, `crash_after_backup_rolls_back_on_next_run`, `interrupted_rollback_finishes_on_next_run`, `missing_om_exe_after_crash_is_restored`, `restore_puts_back_journal_and_removes_new_ones`, `tampered_file_after_download_is_caught_by_reverify`, `judge_waits_for_accounts_but_fails_on_a_crash`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** `cargo xtask ci`, commit `Install and roll back updates in a helper`.

### Task 9: Daemon start, probation and cleanup

**Files:** Create `update/probation.rs`; modify `daemon.rs`, `pending.rs` (`decide` refuses while probation is on), `ui/commands.rs` (`approve_module` deletes the replaced predecessor file after `rescan`, spec 7.4); test `tests/update_probation.rs`.

**Interfaces:**
- Consumes: Tasks 6-8.
- Produces: `probation::on_start(paths: &Paths) -> Start { Normal, Exit, Probation, LaunchHelperAndExit }` called in `daemon::run` before `acquire_lock` (helper alive and stage `Started`/`BackedUp`/`RollingBack` or unreadable -> `Exit`; alive and `Installed` -> `Probation`; helper dead and any file -> `LaunchHelperAndExit`); `probation::watch(paths, activity, launcher, status: impl Fn() -> Value, started: Instant)`: while the file exists the gate is `WritesBlocked`, `triggers.hold` is true and `UpdateService::probation()` is true; every second: file gone -> release everything and `cleanup`; helper lock free and file still there -> `judge` own status: `Healthy` -> delete file, record "zainstalowano", release; `Failed` or deadline passed -> `launch_helper` (it rolls back). `probation::cleanup(paths, keep: &Version)` removes other `updates\<v>` folders and `bin\om.old-*.exe`, `om.failed-*.exe`, `om.new.exe`, `om.old.exe`, ignoring files in use.
- [ ] **Step 1: Failing tests:** `daemon_exits_while_helper_changes_files`, `daemon_launches_helper_when_it_died`, `probation_blocks_writes_and_window_saves`, `probation_holds_trigger_deliveries`, `probation_ends_when_helper_deletes_the_file`, `daemon_finishes_check_when_helper_dies`, `cleanup_keeps_current_version_and_files_in_use`, `approving_a_side_by_side_module_deletes_the_predecessor`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** `cargo xtask ci`, commit `Recover and probe updates at daemon start`.

### Task 10: Update service, window and notification

**Files:** Create `update/service.rs`; modify `ui/mod.rs` (`UiState.update`), `ui/window.rs`, `ui/commands.rs`, `ui/app.js`, `ui/index.html`, `build.rs`, `capabilities/main.json`, `approval.rs`, `tools/mod.rs` (`status.update`); tests `tests/ui.rs`, `tests/update_service.rs`.

**Interfaces:**
- Consumes: Tasks 3, 4, 7, 9.
- Produces: `service::UpdateService` with `spawn(..)` (first check after 5 min, then every 24 h), `check_now()`, `install()` (`record_click` then phases, `mark_launched`, `launch`; refuses with the reason from `installable`), `cancel()`, `set_auto_install(bool)`, `view() -> UpdateView { current, found: Option<{version, rolled_back}>, state: checking | downloading | ready | error(String) | phase(A|B|launching), installable: Result<(), String>, probation, auto_install, last_result }`; `fn installable(paths, current_exe, task_data_dir) -> Result<(), String>` (spec 7: `current_exe == bin\om.exe` and data dir equal to `OPEN_MESS_DATA_DIR` from `HKCU\Environment`, else `%APPDATA%\open_mess`). Auto-install records consent only when the release is not rolled back and `Pending::count(Waiting) == 0`, `count(Approved) == 0`, `gate.in_flight() == 0`, no running or queued spawn, no open or delivering batch. Window commands `update_status`, `check_update`, `install_update`, `cancel_update`, `set_auto_install` (added to `COMMANDS` and `allow-*`); every other mutating command returns "aktualizacja w toku" while `probation()`. "Demon i logi" gets an Aktualizacje block (current and found version, "wycofane" mark, Sprawdź teraz, Zainstaluj, Anuluj during phases, the auto-install checkbox, last result, the install.ps1 hint when not installable). Toast "open_mess <wersja> jest gotowy" with Zainstaluj / Później, activation calls `install()`.
- [ ] **Step 1: Failing tests:** `no_tool_or_cli_command_writes_consent` (registry tool names; `status.update` is read-only), `auto_install_waits_for_quiet`, `rolled_back_release_is_not_auto_installed`, `not_installable_from_a_dev_build`, `window_commands_are_registered` (every `COMMANDS` entry has an `allow-*` and a handler).
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** `cargo xtask ci`, commit `Offer updates in the window and a notification`.

### Task 11: Installer registers the helper task

**Files:** Modify `scripts/install.ps1`.

- [ ] **Step 1:** Register `open_mess-update` next to `open_mess`: action `conhost.exe --headless "<bin>\om.update.exe" apply-update --data-dir "<data dir>"` (the data dir the script already resolves), triggers logon and every 5 min, the same settings with `-MultipleInstances IgnoreNew` and no `RestartCount`; copy `om.exe` to `om.update.exe` too, so the task has a program before the first update.
- [ ] **Step 2:** Run the script on the owner's machine with `-NoStart`, check both tasks in `Get-ScheduledTask`, then start `open_mess` and confirm `om status`.
- [ ] **Step 3:** Commit `Register the update helper task in install.ps1`.

### Task 12: First signed releases (with the owner)

Not code: the owner runs these at the computer.

- [ ] Owner: `cargo xtask release-keygen` (sets the password), commits `keys/release.pub`.
- [ ] Owner: `cargo xtask release 0.1.0`; check the GitHub release has `om.exe`, `teams.wasm`, `manifest.json`, `manifest.json.minisig`; install 0.1.0 with `scripts/install.ps1`.
- [ ] Owner: `cargo xtask release 0.1.1` (any small change). The daemon finds it ("Sprawdź teraz"), the owner clicks Zainstaluj, the daemon comes back as 0.1.1 with Teams online and no module approval (spec 11, first criterion).
- [ ] During that install a Claude Code session keeps `om mcp-stdio` open: `om.exe` is swapped, the bridge reconnects to 0.1.1 (spec 11).
- [ ] Rollback drill on 0.1.1 -> 0.1.2 built with a module that traps at load: "przywrócono", 0.1.1 running, `last-update.txt` explains, the release is marked "wycofane".
