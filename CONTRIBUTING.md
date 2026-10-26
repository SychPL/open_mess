# Contributing to open_mess

open_mess connects messengers (Teams first) to AI agents through one Rust daemon, with MCP for
agents and a CLI for people. It is in early development; the design is
[`.specs/2026-10-26-open-mess-design.md`](.specs/2026-10-26-open-mess-design.md).

This page covers how to build it, how the code is laid out, the rules that aren't negotiable,
how specs and commits are reviewed, and how to send a change. [AGENTS.md](AGENTS.md) has the
long form of the rules; it is written for AI agents but is the most complete description of the
house style.

## Get it running

You need stable Rust 1.97 or newer, the WebAssembly target the platform modules build for, and git.

```bash
rustup target add wasm32-wasip2
cargo build
cargo run -p open-mess -- daemon
cargo xtask ci
```

`cargo xtask ci` runs exactly what CI runs: `cargo fmt --check`, `cargo clippy -D warnings`, the
`wasm32-wasip2` build of the modules (the host's tests load them) and the tests. Run it before every
commit.

## How the code is laid out

| Path | What |
|---|---|
| `crates/open-mess/` | the `om` binary: daemon and CLI (library `open_mess` plus `src/main.rs`) |
| `crates/open-mess/src/daemon.rs` | `om daemon`: start-up order, the last start error, running until Ctrl+C |
| `crates/open-mess/src/instance.rs` | single-instance lock (`daemon.lock` in the data folder) |
| `crates/open-mess/src/paths.rs` | the data folder: `%APPDATA%\open_mess`, or `OPEN_MESS_DATA_DIR` |
| `crates/open-mess/src/store.rs` | SQLite state (`state.sqlite3`): schema migrations, configuration such as the port |
| `crates/open-mess/src/secrets.rs` | secrets in Windows Credential Manager, starting with the local API token |
| `crates/open-mess/src/modules.rs` | the modules folder: module files, their approval and the running modules |
| `crates/open-mess/src/tools/` | the tool registry: every tool, for MCP and the CLI alike; `shared.rs` has the shared tools' schemas and results |
| `crates/open-mess/src/api.rs` | the local API on `127.0.0.1`: bearer token, `/mcp` (MCP) and `/api` (CLI) |
| `crates/open-mess/src/cli.rs` | the `om` command line, generated from the registry |
| `crates/open-mess/tests/` | integration tests: the API, the CLI and the daemon process |
| `crates/open-mess-host/` | the module host: loads platform modules and runs them under limits |
| `modules/teams/` | the Teams platform module, a WebAssembly component implementing `wit/module.wit` |
| `modules/test-*/` | small guest modules the module host's tests load (`publish = false`) |
| `wit/` | the contract between the core and platform modules |
| `xtask/` | repository tasks (`cargo xtask ci`, `cargo xtask hooks`) |
| `.specs/` | specs, one per non-trivial change; review output in `.specs/review/` is local |
| `scripts/review/` | the Codex and GLM review scripts and their prompts |
| `.githooks/` | the post-commit hook that sends each commit for review |
| `plan/` | local notes and plans, gitignored, never committed |

More crates (core) arrive with the specs that introduce them.

## Rules that aren't negotiable

1. **Never disturb the official clients.** open_mess is a separate device: it never touches the
   official apps' local data, never reuses their device ids, never marks messages as read.
2. **No token extraction.** Accounts sign in through the platform's own sign-in flow.
3. **No secrets or message contents** in logs, fixtures, docs, issues, PRs or commits.
4. **Never crash.** No `unwrap`, `expect`, `panic!` or unchecked indexing in non-test code
   (the workspace lints deny them), no `unsafe` without a listed exception.
5. **One tool registry.** Every capability is the same tool over MCP and the CLI.
6. **Everything is tested.** New behaviour comes with tests that fail without it. CI never uses
   real accounts.
7. **Permissive dependencies only.** open_mess is MIT OR Apache-2.0; `deny.toml` lists the
   accepted licences. Say in the PR which dependency you add and why.

## Specs

A non-trivial change starts as `.specs/<slug>.md`: goal, context, steps, touched files,
acceptance criteria, a threat model and a rebuttal register (AGENTS.md, "Specs"). The spec is
reviewed by Codex and GLM before any code is written:

```bash
scripts/review/review-spec.sh .specs/<slug>.md 1
```

Fix the admissible P1/P2 findings or record a rebuttal, then run round 2, and so on. The spec is
ready when both reviewers approve, or when both report only P3 for two rounds in a row.

## Reviews

Every commit is reviewed by Codex and GLM. Enable the hook once per clone:

```bash
cargo xtask hooks
```

After each commit the hook writes the diff to `.reviews/<sha>.patch` and starts both reviewers in
the background; committing is never blocked. Verdicts arrive in `.reviews/<sha>-codex.txt` and
`.reviews/<sha>-glm.txt`. Before you push, deal with every P1/P2 (a follow-up commit, or a reason
it doesn't apply) and write `.reviews/<sha>.processed`.

The review tooling needs Windows with:

- PowerShell 7 (`pwsh`),
- the Codex CLI (`codex`), signed in,
- `opencode` with the Z.ai coding plan (`zai-coding-plan/glm-5.3`), signed in.

`scripts/review/run-reviewer.ps1` times out a stuck reviewer and retries it. GLM falls back from
`glm-5.3` to `glm-5.3-flash`; Codex has no fallback, so a Codex review is always done by Codex.
Reviewers only get read-only tools: they read untrusted diffs and must not be able to run
commands or change files. Without these tools the hook prints a notice and skips the review; a
maintainer then reviews your commits before merge.

## Sending a change

1. Branch from the latest `main`; one topic per branch.
2. Spec approved (for non-trivial changes), `cargo xtask ci` green, commits reviewed and
   reconciled.
3. Open a pull request and fill in the template. CI must pass: format, Clippy, tests on Windows
   and Linux, and the dependency check. Pull requests are squash-merged.
4. Never force-push `main`. Rebase your own branch when it conflicts; don't merge `main` into it.

Commit messages are plain English with an imperative subject and a body that says why.

## Licence

By contributing you agree that your work is dual-licensed under MIT or Apache-2.0, like the rest
of open_mess.
