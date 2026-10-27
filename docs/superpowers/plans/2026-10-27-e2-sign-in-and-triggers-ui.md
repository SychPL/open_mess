# Milestone E2: sign-in and the triggers section - implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the user adds and re-signs accounts from the window (Windows account through WAM, an embedded sign-in page, a device code or a QR code, as the module asks), and manages trigger rules in a Wyzwalacze section.

**Architecture:** The host's sign-in imports (`crates/open-mess-host/src/sign_in.rs`, stubs since B1) become real: each import sends a `SignInRequest` to the daemon through a channel and waits (the call deadline is already paused) for the window's answer. WAM runs in the host with the `windows` crate. The window (milestone E1) shows the requests: an account picker for WAM, a code panel, a QR panel, or a separate sign-in window (a Tauri `WebviewWindow` with no IPC capabilities that loads only approved HTTPS sign-in hosts). The Wyzwalacze section uses `triggers::rules` from milestone D.

**Tech Stack:** what A-E1 use; `windows` 0.62 (`Security_Authentication_Web_Core`, `Security_Credentials`, `Win32_System_WinRT`, `Win32_Foundation`), `windows-future`, `qrcode` (QR to SVG).

**Spec:** `.specs/2026-10-26-open-mess-design.md` 8.3 (sign-in primitives, sign-in identity consent, unknown host during sign-in, R10), 8.7, 8.8, 9.1. **Protocol:** `docs/protocols/teams.md` Live results item 1 (the exact WAM calls that work). **Depends on:** E1 and D merged.

## Global Constraints

- Sign-in starts only from the window (`Dodaj konto`, `Zaloguj ponownie`); no tool or CLI command starts it (spec 8.8).
- WAM: only the client id and scopes in the module's approved manifest; provider `FindAccountProviderWithAuthorityAsync("https://login.microsoft.com", "organizations")`; properties `wam_compat=2.0`, `authority=https://login.microsoftonline.com/organizations`; list `FindAllAccountsWithClientIdAsync` and let the user pick (show the account's user name; duplicates by tenant are shown with their domain); later tokens come from `GetTokenSilentlyWithWebAccountAsync` with the stored `WebAccount.Id` (kept as a module secret `wam-account`); `interactive = true` uses `IWebAuthenticationCoreManagerInterop::RequestTokenForWindowAsync` owned by the configuration window - the one `unsafe` call, listed in AGENTS.md (spec 9.1).
- Tokens never reach logs, the window, the store or the event bus; they go only to the module that asked.
- The sign-in page window: separate Tauri window, no IPC/commands, loads only HTTPS URLs on the module's approved sign-in hosts; the window title shows the current host; navigation to another host pauses and asks "Strona logowania przechodzi na <host>. Zezwolić?"; yes adds the host to that module's approved sign-in hosts, no ends sign-in as `refused`; the first navigation starting with the redirect prefix closes the window and returns that URL (spec 8.3, R3/R10).
- Device code and QR: shown in the main window with the verification URL as a plain link opened in the default browser.
- Sign-in waits up to `SIGN_IN_BUDGET = 10 min`; the user can cancel (`cancelled`).
- Triggers section: list rules with their counters, add/edit/delete; the program for a spawn action is chosen with a file dialog (absolute path); arguments are a list of fixed strings; shows `put`'s warning about autonomous accounts; shows the channel setup: the JSON for `om mcp-stdio` and the `claude --mcp-config <file> --dangerously-load-development-channels server:open_mess_channel` command, with the note that the bridge belongs only in channel sessions.

## Review Focus

- A module whose manifest declares client id X asks `wam-token` for client id Y or for a scope outside its manifest: refused (Task 2 test `wam_is_limited_to_the_manifest`).
- The sign-in page redirects to `login.contoso-adfs.example`: the window asks; "no" ends sign-in, "yes" continues and remembers the host for that module only (Task 3 test `unknown_sign_in_host_asks_and_remembers`).
- The user closes the sign-in window or the account picker: the module gets `cancelled`, no account is added (Task 1 test `cancel_reaches_the_module`).
- Re-signing an existing account with a different Windows account: refused with "to jest inne konto" unless the user removes the old one first (Task 4 test `re_sign_in_must_match_the_account`).
- A spawn rule pointing at a program that is later deleted: the rule shows `failed_to_start` growing, the window marks it (Task 5 manual check).

---

### Task 1: Sign-in request channel

**Files:** Modify `crates/open-mess-host/src/sign_in.rs`, `src/lib.rs` (export `SignInRequest`, `SignInReply`), `crates/open-mess/src/daemon.rs`, `ui/commands.rs`, `ui/app.js`.

**Interfaces:**
- Produces: `SignInRequest::{DeviceCode { module, code, url }, Qr { module, data }, OpenPage { module, url, redirect_prefix }, WamPick { module, accounts: Vec<WamAccount> }}` sent on a `mpsc::Sender<(SignInRequest, oneshot::Sender<SignInReply>)>` in `ModuleContext`; `SignInReply::{Done(Option<String>), Cancelled, Refused(String)}`; window commands `sign_in_start(module)`, `sign_in_answer(request_id, reply)`, event `sign-in-request`.
- [ ] **Step 1: Failing tests:** `device_code_reaches_the_window`, `cancel_reaches_the_module`, `budget_expires_as_timed_out`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). **Step 5:** Commit `Route module sign-in prompts to the window`.

### Task 2: WAM in the host

**Files:** Create `crates/open-mess-host/src/wam.rs` (`#[cfg(windows)]`, stub elsewhere); modify `sign_in.rs`, `Cargo.toml`, `AGENTS.md` (the interop call).

**Interfaces:**
- Produces: `wam::accounts(client_id) -> Vec<WamAccount { id, user_name }>`; `wam::token_silent(client_id, scopes, account_id) -> Result<String, SignInError>`; `wam::token_interactive(hwnd, client_id, scopes) -> Result<(String, WamAccount), SignInError>`; WIT `wam-token(scopes, interactive)` checks the manifest identity, uses the stored `wam-account`, or on first sign-in asks the window to pick (`WamPick`) and stores the pick.
- [ ] **Step 1: Failing tests** (pure parts; WAM itself is checked by hand): `wam_is_limited_to_the_manifest`, `stored_account_is_reused`, `pick_is_stored_as_module_secret`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). Manual: with test-echo extended to call `wam-token` for the Teams client id and `https://ic3.teams.office.com/.default`, sign in from the window, check the picker lists the work accounts and that the module gets a token (log only its length). **Step 5:** Commit `Sign in with the Windows account through WAM`.

### Task 3: Sign-in page window

**Files:** Create `crates/open-mess/src/ui/sign_in_page.rs`; modify `modules.rs` (approved sign-in hosts per module, persisted).

- [ ] **Step 1: Failing tests:** `only_approved_hosts_load` (navigation policy function), `redirect_prefix_returns_url`, `unknown_sign_in_host_asks_and_remembers`, `non_https_is_blocked`.
- [ ] **Step 2-4:** Run (FAIL), implement (Tauri `on_navigation` uses the tested policy function), run (PASS). Manual: open a sign-in page for a test module pointing at `https://login.microsoftonline.com/` and cancel. **Step 5:** Commit `Show module sign-in pages in a locked-down window`.

### Task 4: Adding and re-signing accounts

**Files:** Modify `crates/open-mess/src/accounts.rs`, `ui/commands.rs`, `ui/app.js`.

- [ ] **Step 1: Failing tests:** `add_account_from_sign_in`, `re_sign_in_must_match_the_account`, `failed_sign_in_adds_nothing`.
- [ ] **Step 2-4:** Run (FAIL), implement (`Dodaj konto` lists active modules, runs `sign_in`, adds the returned account; `Zaloguj ponownie` runs it for an existing one and compares the account id), run (PASS). **Step 5:** Commit `Add and re-sign accounts from the window`.

### Task 5: Wyzwalacze section

**Files:** Modify `crates/open-mess/src/ui/commands.rs`, `ui/app.js`.

**Interfaces:** commands `rules() -> Vec<RuleDto + counters>`, `put_rule(RuleDto) -> Option<String /* warning */>`, `delete_rule(id)`, `pick_program() -> Option<String>` (native file dialog), `channel_setup() -> ChannelSetupDto` (JSON text and command line).
- [ ] **Step 1: Failing tests:** `put_rule_returns_autonomous_warning`, `relative_program_is_rejected_from_the_window`, `counters_are_listed`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS). Manual: create a DM rule with channel action, check the setup text; create a spawn rule with a program, delete the program, check the window marks `failed_to_start`. **Step 5:** Commit `Manage trigger rules in the window`.
