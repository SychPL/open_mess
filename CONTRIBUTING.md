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
| `crates/open-mess/src/accounts.rs` | accounts, each bound to the module that signed it in; restore and removal |
| `crates/open-mess/src/events.rs` | the event bus: module events with account ids, for the triggers |
| `crates/open-mess/src/tools/` | the tool registry: every tool, for MCP and the CLI alike; `shared.rs` has the shared tools' schemas and results |
| `crates/open-mess/src/api.rs` | the local API on `127.0.0.1`: bearer token, `/mcp` (MCP) and `/api` (CLI) |
| `crates/open-mess/src/cli.rs` | the `om` command line, generated from the registry |
| `crates/open-mess/src/triggers/` | trigger rules and their engine: matching, batching, limits, and the actions (channel notification, program run) |
| `crates/open-mess/src/bridge.rs` | `om mcp-stdio`: the stdio MCP server a Claude Code session with channels runs, forwarding to the daemon |
| `crates/open-mess/tests/` | integration tests: the API, the CLI and the daemon process |
| `crates/open-mess-host/` | the module host: loads platform modules and runs them under limits |
| `modules/teams/` | the Teams platform module, a WebAssembly component implementing `wit/module.wit` |
| `modules/test-*/` | small guest modules the module host's tests load (`publish = false`) |
| `wit/` | the contract between the core and platform modules |
| `xtask/` | repository tasks (`cargo xtask ci`, `hooks`, `release`, `release-keygen`) |
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

### Running the daemon from Claude Desktop

Claude Desktop is an MSIX package, so processes started by its built-in tools see a virtualized
`%APPDATA%`: files they write there land in `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\`
and a daemon started elsewhere does not see them. Copy modules into
`%APPDATA%\open_mess\modules\` from the same terminal that starts `om daemon` (or from Explorer).

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

## Releases

Only the owner releases. Once, `cargo xtask release-keygen` asks for a password and creates the
signing key: the secret key goes to `OPEN_MESS_RELEASE_KEY` or `%USERPROFILE%\.open_mess\release.key`
(never inside the repository), the public key to `keys/release.pub`, which you commit; `om`
compiles it in and refuses any update signed with another key. The command never overwrites a key.

`cargo xtask release 1.2.3` needs `git`, the GitHub CLI (`gh`, signed in) and the key password,
which it asks for on the console unless `OPEN_MESS_RELEASE_KEY_PASSWORD` holds it. It requires a
clean `main` equal to `origin/main`, a version above the last `v*` tag, a green `cargo xtask ci` and
a `.reviews/<sha>.processed` file for every commit since that tag. The secret key must be the pair
of `keys/release.pub` as it was at that tag: that is the key the installed versions trust. Then it
commits the version bump (`Release 1.2.3`, only the version and `Cargo.lock`), tags `v1.2.3`, builds
`om.exe` and the platform modules, signs `manifest.json`, pushes, creates the GitHub release as a
draft, checks that it holds exactly these files and only then publishes it. Installs never see a
draft, so they never get a half-uploaded release. It stops at the first failure and, once the commit
exists, prints the commands that undo or finish the release. After the push they first check with
`git ls-remote` that the tag reached origin, then run `gh release view`, because what is left to do
depends on whether the release is missing, still a draft or already published.

Run both commands in a normal terminal such as Windows Terminal. They read the password from the
console, and a terminal embedded in another app may never pass it through.

Merge commits, and the squash commits GitHub makes when it merges a pull request, never pass the
local post-commit hook, so `release` lists them as unprocessed. Check that each one adds nothing
unreviewed, then write its `.processed` file saying how you checked. For a merge commit, its tree
must be the one an automatic re-merge gives: `git merge-tree --write-tree <sha>^1 <sha>^2` must print
the same tree id as `git rev-parse <sha>^{tree}`. For a squash commit, `git diff <sha>^ <sha>` must
hold exactly the changes of the reviewed branch.

### Releasing from GitHub

From the second release on, the owner can release without building or typing the password locally:
in the repository's Actions tab pick "Release", then "Run workflow" on `main`, enter the version and
approve the run when GitHub asks for the `release` environment. The workflow runs
`cargo xtask release <version> --pipeline` on a fresh Windows runner with the key from that
environment. The pipeline mode does everything above, except that the `.reviews` check, which only
exists on the owner's machine, becomes a check that every commit since the last tag is the squash
commit of a pull request merged into `main`. It refuses to run outside GitHub Actions, without an
earlier `v*` tag, or when `main` moved after the run was started. The run's summary lists the
commits with their pull requests.

The first release is local: it covers the commits from before pull requests, which only the
`.reviews` check can vouch for.

The setup, once:

- The `release` environment, with the owner as its required reviewer and deployments from `main`
  only. Leave "Prevent self-review" off: the owner both starts the run and approves it, and with it
  on nobody could approve a release.
- Its two secrets, set by the owner in a normal terminal and by nobody else (the second command asks
  for the password):

  ```powershell
  Get-Content -Raw "$env:USERPROFILE\.open_mess\release.key" | gh secret set RELEASE_KEY_FILE --env release --repo SychPL/open_mess
  gh secret set RELEASE_KEY_PASSWORD --env release --repo SychPL/open_mess
  ```

- Squash as the only way to merge pull requests, so every commit on `main` is the merge commit of
  its pull request.

The workflow pushes the `Release <version>` commit and its tag straight to `main` with the
workflow's own token. Branch protection on `main` that requires pull requests must let GitHub
Actions through, or releases go back to being local.

When a run fails after the push, the commit and the tag are on `main` and the release is missing or
still a draft. The run keeps `target/release-dist` for 14 days as the artifact
`release-dist-<version>`. Its summary gives the commands that finish the release, also when the
Release step was cancelled or timed out before `xtask` could print them: in a clone at the tag, check
the tag with `git ls-remote`, then `gh run download <run id> -n release-dist-<version> -D target/release-dist`,
then `gh release view` and the `gh release` commands for what it shows.

### Rotating the release key

When the key leaks or has to change, every install still trusts only the old public key, so the new
one arrives in a release signed with the old one:

1. Create the new pair next to the old one: delete `keys/release.pub`, point
   `OPEN_MESS_RELEASE_KEY` at a new file outside the repository and run `cargo xtask release-keygen`.
2. Commit the new `keys/release.pub` through a pull request.
3. Release it with the old key, which is still in the `release` environment (or locally, with
   `OPEN_MESS_RELEASE_KEY` back on the old file). `release` checks the key against the last tag's
   `keys/release.pub`, so it refuses the new key here. Say in the release notes that the key
   changed.
4. Only then replace both secrets with the new key file and its password, and make the new key the
   local default.

Installs check only the newest release, so keep the transition release the newest one for as long as
you can. An install that was off for its whole time never sees it and refuses every later release:
its user reinstalls by hand with `scripts/install.ps1` from a build of the newest tag.

## Licence

By contributing you agree that your work is dual-licensed under MIT or Apache-2.0, like the rest
of open_mess.
