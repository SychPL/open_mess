# Milestone F1: Teams protocol core - implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the pure, network-free logic of the Teams module - service directory, chat service decoders, unread rule, HTML/Markdown conversion, request builders with the read-state write ban, retry policy - as a Rust crate that builds both natively (tests) and as a `wasm32-wasip2` component implementing `wit/module.wit`.

**Architecture:** One crate `modules/teams` (package `open-mess-teams`, `crate-type = ["cdylib", "rlib"]`). Plain modules under `src/` hold all logic and are tested natively on recorded-shape fixtures. `src/component.rs`, compiled only for `target_arch = "wasm32"`, uses `wit-bindgen` to implement the `platform-module` world; in F1 it returns the manifest and answers every other export with `module-error::not-supported` - the network wiring is milestone F2.

**Tech Stack:** Rust 2024, serde/serde_json, url, wit-bindgen (guest, latest stable; check its docs for the `generate!` syntax of that version), target `wasm32-wasip2`. HTML parsing: a small hand-written tokenizer is enough for Teams' limited HTML; add a crate only if the tokenizer would exceed ~150 lines.

**Spec:** `.specs/2026-10-26-open-mess-design.md` sections 8.3, 8.5, 9.2, 9.3. **Protocol:** `docs/protocols/teams.md` sections 1-8 - every shape, field name and rule in this plan comes from there; where the doc marks a fact UNCONFIRMED, implement it as the doc describes and name the test `..._unconfirmed_shape` so F2's live capture knows what to re-check.

## Global Constraints

- No network, no clock reads in logic functions: time comes in as a parameter (`now_ms: i64`) so tests are deterministic.
- No `unwrap` / `expect` / `panic!` / unchecked indexing in non-test code; every decoder returns `Result<_, ProtoError>` and treats any missing or mistyped field as `ProtoError::Incompatible(<path>)` - never guesses (spec 9.3).
- Fixtures are synthetic: shapes from `docs/protocols/teams.md`, invented ids (`19:aaaa...@thread.v2`, `8:orgid:00000000-0000-0000-0000-000000000001`), invented names and text. No real tenant ids, user ids, tokens or message text.
- Property writes are allowed only for the name `emotions`; everything in docs section 3.4's write list is refused by construction (spec 9.2).
- The manifest's client id is the desktop id `1fec8e78-bce4-4aaf-ab1b-5451cc387264` with redirect `https://login.microsoftonline.com/common/oauth2/nativeclient` (spec 9.1).
- `cargo xtask ci` also builds the module for `wasm32-wasip2` (Task 1 adds that step); CI installs the target.

## Review Focus

- A conversation list or message page with an unknown `messagetype` (e.g. a new system event type): the item is skipped with a recorded `unknown_type` note, the page still decodes (Task 3 test `unknown_message_type_is_skipped`).
- A message body with nested or malformed HTML (unclosed tags, entities, `<script>`): Markdown output contains the text, never raw tags or script content, and never panics (Task 4 test `malformed_html_degrades_to_text`).
- A user sending text that contains `<`, `&` or HTML-looking strings: it arrives as literal text, not markup (Task 4 test `send_escapes_html`).
- `Retry-After` of 301 s or a missing/garbage `Retry-After` on 429: give up with `throttled(301)` / fall back to exponential backoff (Task 6 tests).
- A conversation whose horizon fields are missing entirely: `unread` reports `Unknown` for it instead of guessing read or unread (Task 5 test `missing_horizon_is_unknown`).

---

### Task 1: Crate and component skeleton

**Files:**
- Modify: `Cargo.toml` (workspace members add `"modules/*"`)
- Create: `modules/teams/Cargo.toml`, `modules/teams/src/lib.rs`, `modules/teams/src/manifest.rs`, `modules/teams/src/component.rs`
- Modify: `xtask/src/main.rs` (CI step `cargo build -p open-mess-teams --target wasm32-wasip2 --locked`), `.github/workflows/ci.yml` (`targets: wasm32-wasip2` on the toolchain step), `CONTRIBUTING.md` (prerequisite `rustup target add wasm32-wasip2`)

**Interfaces:**
- Produces: `manifest::manifest() -> Manifest` (plain Rust struct mirroring WIT `manifest`: `name = "teams"`, `platform = "teams"`, `version = CARGO_PKG_VERSION`, `hosts` and `sign_in_hosts` from docs sections 1-7 and 8.3's pattern rule (`*.sharepoint.com`, never a bare public suffix), `wam = Some(WamIdentity { client_id: "1fec8e78-bce4-4aaf-ab1b-5451cc387264", scopes })`, `extra_tools = []`); `component.rs` (wasm32 only) exports it through `wit-bindgen` and returns `ModuleError::NotSupported("milestone F2")` from `sign-in`, `restore`, `call`, and ignores `events`.

- [ ] **Step 1: Write the failing tests** in `manifest.rs`:

```rust
#[test]
fn manifest_names_the_desktop_identity() {
    let m = manifest();
    assert_eq!(m.name, "teams");
    assert_eq!(m.wam.as_ref().map(|w| w.client_id.as_str()), Some("1fec8e78-bce4-4aaf-ab1b-5451cc387264"));
    assert!(m.sign_in_hosts.iter().any(|h| h == "login.microsoftonline.com"));
}

#[test]
fn host_patterns_sit_below_a_registrable_domain() {
    for h in manifest().hosts {
        if let Some(rest) = h.strip_prefix("*.") {
            assert!(rest.split('.').count() >= 2, "{h}");
            assert!(!["com", "net", "co.uk"].contains(&rest), "{h}");
        }
    }
}
```

- [ ] **Step 2:** `cargo test -p open-mess-teams`. Expected: FAIL.
- [ ] **Step 3:** Implement the crate, the manifest and the component glue; add the xtask/CI/CONTRIBUTING changes.
- [ ] **Step 4:** `cargo test -p open-mess-teams` PASS; `cargo build -p open-mess-teams --target wasm32-wasip2` produces `target/wasm32-wasip2/debug/open_mess_teams.wasm`; `cargo xtask ci` green.
- [ ] **Step 5:** Commit `Add the Teams module crate and its component skeleton`.

### Task 2: Service directory (authsvc)

**Files:** Create `modules/teams/src/authsvc.rs`, `modules/teams/tests/fixtures/authsvc_ok.json`, `modules/teams/tests/fixtures/authsvc_missing_chat.json`

**Interfaces:**
- Produces: `authsvc::request(entra_token: &str) -> HttpRequest` (POST `https://teams.microsoft.com/api/authsvc/v1.0/authz`, headers per docs section 2 and 8); `authsvc::decode(body: &[u8]) -> Result<Session, ProtoError>`; `Session { skype_token: Option<SecretString>, expires_at_ms: Option<i64>, directory: ServiceDirectory }`; `ServiceDirectory { chat_service: Url, trouter: Option<Url>, registrar: Option<Url>, ams: Option<Url>, middle_tier: Option<Url>, search: Option<Url> }` (from `regionGtms`, `chat_service` from `chatServiceAfd` - required). `HttpRequest { method, url, headers: Vec<(String, String)>, body: Vec<u8> }` lives in `src/http.rs` (shared by later tasks). `SecretString` (in `src/http.rs`) has a `Debug` impl that prints `***`.

- [ ] **Step 1: Failing tests:** `decodes_region_directory` (fixture -> `chat_service` equals the fixture's `chatServiceAfd`, `ams` present); `missing_chat_service_is_incompatible` (`Err(ProtoError::Incompatible(p))` with `p` containing `chatServiceAfd`); `debug_never_prints_token` (`format!("{:?}", session)` does not contain the fixture's token string).
- [ ] **Step 2:** Run, expect FAIL. **Step 3:** Implement. **Step 4:** Run, expect PASS.
- [ ] **Step 5:** Commit `Decode the Teams service directory from authsvc`.

### Task 3: Chat service decoders and request builders

**Files:** Create `modules/teams/src/chat.rs`, fixtures `conversations_page.json`, `messages_page.json`, `messages_page_unknown_type.json`

**Interfaces:**
- Produces:
  - `chat::Conversation { id: String, kind: ConversationKind /* Direct, Group, Channel, Meeting, Other */, title: Option<String>, last_message_id: Option<String>, horizons: Horizons }` and `chat::decode_conversations(body: &[u8]) -> Result<Page<Conversation>, ProtoError>`.
  - `chat::Message { id: String, conversation: String, sender_id: String, sender_name: Option<String>, sent_ms: i64, edited: bool, deleted: bool, html: String, mentions: Vec<Mention>, reply_to: Option<String> }`, `chat::decode_messages(body: &[u8]) -> Result<Page<Message>, ProtoError>`; `Page<T> { items: Vec<T>, backward_link: Option<String>, sync_state: Option<String>, skipped: Vec<String> }` (`skipped` lists unknown `messagetype`s).
  - Builders returning `HttpRequest`: `list_conversations(dir: &ServiceDirectory, page: Option<&str>)`, `read_messages(dir, conv: &str, page: Option<&str>, limit: u16)`, `send(dir, conv: &str, html: &str, client_message_id: &str)`, `edit(dir, conv: &str, message_id: &str, html: &str)`, `react(dir, conv: &str, message_id: &str, emotion: &str)`. Auth headers are added by F2, not here. Paths and query parameters exactly as docs section 3.2.
  - `chat::property_write(dir, conv: &str, message_id: &str, name: &str, value: &str) -> Result<HttpRequest, ProtoError>`: `Ok` only for `name == "emotions"`; any other name, in any letter case, is `Err(ProtoError::Forbidden(name))`. `react` uses it.

- [ ] **Step 1: Failing tests:** `decodes_conversation_page`, `decodes_message_page_with_paging_links`, `unknown_message_type_is_skipped` (page decodes, `skipped == ["<that type>"]`), `send_carries_client_message_id`, `edit_targets_the_message`, `consumption_horizon_write_is_forbidden` (`property_write(.., "consumptionhorizon", ..)` and `"ConsumptionHorizon"` are `Forbidden`), `only_emotions_is_writable` (iterate the whole write list from docs section 3.4, each is `Forbidden`; `"emotions"` is `Ok`), `missing_message_id_is_incompatible`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Decode Teams conversations and messages and build chat requests`.

### Task 4: HTML and Markdown

**Files:** Create `modules/teams/src/text.rs`

**Interfaces:**
- Produces: `text::html_to_markdown(html: &str) -> String` (paragraphs and `<br>` to newlines, `<b>/<strong>` to `**`, `<i>/<em>` to `_`, `<a href>` to `[text](href)`, `<code>`/`<pre>` to backticks/fences, mentions (`<span itemtype=".../Mention">`) to `@Name`, entities decoded, every other tag dropped with its text kept, `<script>`/`<style>` dropped with their content); `text::markdown_to_html(md: &str) -> String` (escape `& < > " '` first, then the same small subset back, newlines to `<br>`).

- [ ] **Step 1: Failing tests:** `paragraphs_and_breaks`, `bold_italic_links`, `mention_becomes_at_name`, `entities_are_decoded`, `malformed_html_degrades_to_text` (input `"<p>a <b>b <i>c</p><script>x()</script>&amp"` -> contains `a`, `b`, `c`, not `<`, not `x()`), `send_escapes_html` (`markdown_to_html("1 < 2 & <b>")` contains `1 &lt; 2 &amp; &lt;b&gt;`), `round_trip_of_simple_markdown`.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Convert Teams HTML to Markdown and back`.

### Task 5: Unread rule

**Files:** Create `modules/teams/src/unread.rs`

**Interfaces:**
- Consumes: `Horizons` from Task 3 (the fields docs section 3.4 names: conversation consumption horizon, the "marked as unread" bookmark, the account-wide `appConsumptionHorizon`, last message id/time).
- Produces: `unread::state(conv: &Conversation, app_horizon_ms: Option<i64>, me: &str) -> UnreadState` with `UnreadState::{Read, Unread, Unknown}` - exactly the rule of docs section 3.4, including the exceptions the doc names. Pure read: there is no function that produces a write.

- [ ] **Step 1: Failing tests** - one per clause of the docs rule, plus: `own_last_message_is_read`, `bookmark_marks_unread_even_if_horizon_passed`, `mark_all_as_read_horizon_wins_over_old_messages`, `stale_bookmark_older_than_app_horizon_is_read`, `missing_horizon_is_unknown`. Name tests for clauses the doc marks UNCONFIRMED with the `_unconfirmed_shape` suffix.
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Compute Teams unread state with the client's rule`.

### Task 6: Retry policy

**Files:** Create `modules/teams/src/retry.rs`

**Interfaces:**
- Produces: `retry::decide(status: u16, retry_after: Option<&str>, attempt: u8, now_ms: i64) -> Retry` with `Retry::{Done, After(u32 /* ms */), GiveUp(GiveUpReason)}`; policy from docs section 8: retry 429 and the listed 5xx, at most 4 retries, honour `Retry-After` (seconds or HTTP date, relative to `now_ms`), give up when it exceeds 300 s (`GiveUp(Throttled(secs))`), exponential backoff with the doc's base when the header is missing or unparsable.

- [ ] **Step 1: Failing tests:** `ok_is_done`, `retry_after_seconds_is_honoured`, `retry_after_over_300_gives_up` (301 -> `GiveUp(Throttled(301))`), `garbage_retry_after_uses_backoff`, `fifth_attempt_gives_up`, `client_errors_are_not_retried` (400, 401, 403, 404 -> `Done`).
- [ ] **Step 2-4:** Run (FAIL), implement, run (PASS).
- [ ] **Step 5:** Commit `Add the Teams client's retry policy`.
