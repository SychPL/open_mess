# Milestone E1: configuration window - implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** a tray icon and a configuration window (layout A: sections on the left, details on the right) in which the user sees the daemon, accounts and modules, sets agent permissions, approves modules, and approves or rejects waiting actions - including long content no notification can show.

**Architecture:** On Windows, `om daemon` runs a Tauri 2 event loop on the main thread (tray icon, window created on demand, destroyed on close so WebView2 only runs while it is open) and the existing tokio daemon on its own runtime thread. The window's frontend is static HTML/CSS/JS in `crates/open-mess/ui/` (no build step, no framework, no remote content). It talks to the daemon only through Tauri commands defined in `crates/open-mess/src/ui/commands.rs`, which are plain functions over the daemon state so they are unit-tested without a webview. Linux builds compile `ui` to a stub (no tray, no window).

**Tech Stack:** Tauri 2 (latest 2.x stable - not the 3.0 alphas), `tauri-build`; Windows-only target dependencies. Read the Tauri 2 docs for the exact APIs: tray (`tauri::tray::TrayIconBuilder`), `WebviewWindowBuilder`, capabilities/permissions files, CSP, `RunEvent::ExitRequested` (prevent exit when the last window closes).

**Spec:** `.specs/2026-10-26-open-mess-design.md` 8.1, 8.2, 8.3 (module approval and replacement), 8.6 (permissions, "Do zatwierdzenia"), 8.8 (whole section), 15. Layout A as described in 8.8. **Depends on:** milestones A-C (merged). Sign-in and the triggers section come in E2 (after milestone D).

## Global Constraints

- Closing the window never stops the daemon; the tray menu has `Otwórz ustawienia` and `Zakończ` (stops the daemon) (spec 8.8). `om config` opens the window through a new local API route `POST /api/ui/open` (token-protected like the rest; opening a window is harmless).
- The window is the only place that can: set permissions, approve or disable modules, approve or reject waiting actions, remove accounts, reveal the local API token. None of these are tools, and the Tauri commands are reachable only from the window's own page: Tauri capabilities allow exactly the commands below for the main window, no plugin APIs, `withGlobalTauri: false`, CSP `default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src ipc: http://ipc.localhost`, no remote URLs ever loaded in this window.
- UI text is Polish (the user's language), code and identifiers English. Plain hyphens, no em dashes. Follows the system light/dark theme.
- Content written by other people (conversation titles, participant names, message text) is inserted with `textContent`, never `innerHTML` (a message containing `<img onerror>` must render as text).
- Approving in the window shows the full content with scrolling; the "Do zatwierdzenia" list is the place where long content can be approved (spec 8.6, R7). Approve and Reject are per action; there is no "approve all".
- The local API token is shown only after clicking `Pokaż polecenie` in the Integracje section, and only for that view.

## Review Focus

- A waiting action whose text contains `<img src=x onerror=alert(1)>` and a 3 000-line body: rendered as text, scrollable, Approve works (Task 3 manual check).
- Closing the window while a toast is on screen: daemon keeps running, the toast still works (manual check in Task 1).
- Approving a module whose file changed after the window loaded: refused with "plik modułu się zmienił - odśwież" because the hashes passed with the click no longer match (Task 2 test `stale_module_approval_is_refused`).
- Two windows requested quickly (`om config` twice): one window, focused (Task 1 test `second_open_focuses`).
- Removing an account with actions waiting: the actions disappear from the list and their toasts close (Task 3 test `removing_account_clears_its_waiting_actions`).

---

### Task 1: Tray, window shell and the event loop

**Files:** Modify `crates/open-mess/Cargo.toml` (Windows-only `tauri`, `tauri-build` build-dependency), create `crates/open-mess/build.rs`, `crates/open-mess/tauri.conf.json`, `crates/open-mess/capabilities/main.json`, `crates/open-mess/src/ui/mod.rs` (`#[cfg(windows)]` real, else stub), `crates/open-mess/ui/index.html`, `ui/app.css`, `ui/app.js`, `crates/open-mess/icons/` (an original simple icon, recorded in `ATTRIBUTION.md` if derived from anything); modify `daemon.rs` (daemon on a runtime thread, UI on the main thread), `api.rs` + `cli.rs` (`POST /api/ui/open`, `om config`).

**Interfaces:**
- Produces: `ui::run(state: UiState) -> Result<(), Error>` (blocks on the Tauri loop; returns when `Zakończ` is chosen, after the daemon shut down cleanly); `UiState` = handles the commands need (`Arc<Accounts>`, `Arc<Pending>`, store access, modules, port, data dir); `ui::open_window()` (idempotent: focuses an existing window).
- [ ] **Step 1: Failing tests:** `second_open_focuses` (the window registry logic: two open requests produce one window id), `ui_open_route_requires_token`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). Manual: start `om daemon`, tray icon appears, `om config` opens the window with the left menu (Konta, Uprawnienia agentów, Do zatwierdzenia, Moduły, Integracje, Demon i logi), closing it keeps `om status` working, `Zakończ` stops the daemon. Write what you saw in the commit message.
- [ ] **Step 5:** Commit `Add the tray icon and the configuration window shell`.

### Task 2: Commands for status, accounts, modules and integrations

**Files:** Create `crates/open-mess/src/ui/commands.rs`; modify `modules.rs` (rescan in place), `ui/app.js`.

**Interfaces:**
- Produces Tauri commands (each a plain `fn`/`async fn` over `UiState`, wrapped by `#[tauri::command]` on Windows): `status() -> StatusDto`; `accounts() -> Vec<AccountDto>`; `remove_account(id: String)`; `modules() -> Vec<ModuleDto>` (name, version, platform, state, hosts, sign-in hosts, WAM identity, file and manifest hashes, what changed versus the approved version); `approve_module(name, file_sha256, manifest_sha256)` (refused with a clear message when the hashes no longer match the file on disk); `set_module_enabled(name, enabled)`; `rescan_modules()` (keeps a running old version until a replacement is approved); `mcp_command() -> String` (the `claude mcp add ...` line with the token, as `om mcp-command` prints it).
- [ ] **Step 1: Failing tests:** `stale_module_approval_is_refused`, `rescan_keeps_running_version_until_approved`, `remove_account_from_window`, `module_dto_lists_hosts_and_identity`.
- [ ] **Step 2-4:** Run (FAIL), implement (commands + the Konta, Moduły, Integracje, Demon sections in the frontend), run (PASS).
- [ ] **Step 5:** Commit `Show accounts, modules and integrations in the window`.

### Task 3: Permissions and the approval list

**Files:** Modify `crates/open-mess/src/ui/commands.rs`, `permissions.rs` (`clear_override(store, account, conversation)`), `pending.rs` (toasts of removed actions close), `ui/app.js`.

**Interfaces:**
- Produces: `permissions(account) -> PermissionsDto` (account row plus conversation overrides); `set_permissions(account, conversation: Option<String>, read, write)`; `clear_permission_override(account, conversation)`; `waiting() -> Vec<PreviewDto>` (full content) plus a Tauri event `pending-changed` driven by `Pending::subscribe()`; `decide(action, approve) -> String /* new state */`.
- Frontend: Uprawnienia agentów (per account: read toggle, write `nigdy / z potwierdzeniem / samodzielnie`, overrides per conversation with add/remove); Do zatwierdzenia (one card per action: account, conversation, all participants, full scrollable content, before/after for edits, Approve / Reject; updates live).
- [ ] **Step 1: Failing tests:** `set_and_clear_override`, `decide_from_window_updates_state`, `removing_account_clears_its_waiting_actions`. The frontend inserts other people's content only through one helper that sets `textContent`; that is checked by hand (Step 4), not by a JS test framework.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). Manual: set a test account to `confirm`, `om send` a 50-line text containing `<img src=x onerror=alert(1)>`, check it shows as text and approve it from the list; set `nigdy`, check `om send` is refused.
- [ ] **Step 5:** Commit `Set permissions and decide waiting actions in the window`.
