# Stage 1 roadmap

Spec: `.specs/2026-10-26-open-mess-design.md`. Protocol facts: `docs/protocols/teams.md`.

Stage 1 is several subsystems, so it is built as a series of plans, each producing working, tested
software on its own. Each milestone gets its own plan file in this folder when its turn comes;
only milestone A is written in detail now.

| Milestone | What it delivers | Needs | Can run in parallel with |
|---|---|---|---|
| A. Core skeleton | `om daemon` (single instance, fixed port, bearer token), SQLite state, tool registry served over MCP (Streamable HTTP) and the CLI, `status` tool | - | - |
| B1. Module host runtime | `crates/open-mess-host`: wasmtime host with the imports of spec 8.3 (HTTP/WebSocket with host list, address checks, limits; storage; secrets; timers; sign-in stubs), test guest modules | F1 (WIT) | A, C |
| B2. Modules in the daemon | modules folder, manifest approval state, accounts bound to modules, the shared read tools of 8.5 routed to modules, untrusted-content marking | A, B1 | C |
| C. Permissions and approvals | permission model of 8.6, write tools `send` / `edit_message` / `react` / `pending`, pending-action store, Windows notification approval, untrusted-content marking | A (B for real writes) | B, F1 |
| D. Triggers | trigger rules of 8.7: conditions, batching with max delay, quiet hours, rate and resource limits, action 1 (channel notification, stdio bridge if needed), action 2 (spawn without shell) | B, C | E |
| E. Configuration window | Tauri 2 window, layout A, the sections of 8.8, tray icon, sign-in primitives' UI | A, B (C, D for their sections) | D |
| F1. Teams protocol core | the Teams module's pure logic as a WASM component: decoders for chat service / authsvc / Trouter payloads, the unread rule, HTML to Markdown, the writable-property allow-list - tested on redacted fixtures, no network | B's WIT (can start from a draft) | B, C |
| F2. Teams live | sign-in spike and live captures with the user present (spec 9.5), then wiring F1 to real endpoints, coexistence week | B, C, E, F1 + the user | - |

Order: A and F1 first (done or in progress), B1 in parallel with A, then B2 + C, then D + E, then F2. WAM sign-in, authsvc, chat reads and sends are already confirmed live (docs/protocols/teams.md, Live results).

Every milestone follows AGENTS.md: spec section already approved (this spec), `cargo xtask ci`
green before each commit, every commit reviewed by Codex and GLM and reconciled, merged into
`main` only after that.
