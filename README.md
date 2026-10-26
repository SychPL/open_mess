# open_mess

One lightweight program that connects your messengers to AI agents.

open_mess is a single Rust daemon that holds your Microsoft Teams account (Discord, Slack and
others to follow), signs in the way the official apps do, and gives agents one set of tools over
MCP. The same tools work from the command line. There is no chat window for people: agents read
and write, you configure.

**Status: design phase.** Nothing works yet. The design is
[`.specs/2026-10-26-open-mess-design.md`](.specs/2026-10-26-open-mess-design.md) (in Polish).

## Why

Agent integrations for messengers today are bolted onto other apps: you copy tokens out of a
browser, every messenger has its own tool with its own syntax, each one is another process, and
some of them break the official client they lean on. open_mess replaces them with one process,
one syntax and real sign-in.

## How it will work

- **Daemon** (`om daemon`): one background process that starts with Windows and holds every
  account, live connection and module.
- **MCP** over HTTP on `127.0.0.1`: one daemon serves every Claude Code and Codex session at once.
- **CLI** (`om <tool>`): the same tools, for people and scripts.
- **Configuration window**: accounts and sign-in, agent permissions, triggers, modules,
  integrations, daemon status. Closing it leaves the daemon running.
- **Modules**: each platform is a WebAssembly component loaded at run time, so adding a messenger
  doesn't need a rebuild. The core does all networking, storage and secret handling for them.
- **Triggers**: wake an agent when someone writes to you (into an open Claude Code session, as a
  new agent run, or as a webhook), with batching, quiet hours and approval before sending.
- **Coexistence**: open_mess acts like a separate device. Your official Teams keeps working as
  before, and reading through open_mess never marks anything as read.

## Roadmap

1. **Messages**: core, CLI, MCP, configuration window, triggers, the Teams module. Then Discord
   and Slack modules on the same core.
2. **Meetings you attend**: calendar, transcripts Teams already made, local transcription;
   agents write the summaries.
3. **An agent attends for you**: joins a meeting, listens, and answers through a fast model when
   someone addresses you, introducing itself as your assistant.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Agents: [AGENTS.md](AGENTS.md).

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

open_mess is an independent project. It is not affiliated with or endorsed by Microsoft,
Discord, Slack or any other platform it connects to. Using unofficial clients may be against a
platform's terms or your organisation's policies; that decision is yours.
