# open_mess

One lightweight program that connects your messengers to AI agents.

open_mess is a single Rust daemon that holds your Microsoft Teams account (Discord, Slack and
others to follow), signs in the way the official apps do, and gives agents one set of tools over
MCP. The same tools work from the command line. There is no chat window for people: agents read
and write, you configure.

**Status: early development.** The daemon, MCP and the CLI run, with one tool (`status`); no
messenger is connected yet. The design is
[`.specs/2026-10-26-open-mess-design.md`](.specs/2026-10-26-open-mess-design.md) (in Polish).

## Try it

On Windows, with Rust 1.97 or newer:

```bash
cargo run -p open-mess -- daemon
```

The daemon listens on `http://127.0.0.1:7341/mcp` and keeps running until Ctrl+C. Only one runs
per Windows user. In another terminal (`cargo install --path crates/open-mess` puts `om` on your
`PATH`):

```bash
om status           # the same tools agents get over MCP
om status --json    # JSON for scripts
om mcp-command      # prints the ready `claude mcp add ...` command
```

`om mcp-command` prints the command that adds open_mess to Claude Code for every project:

```bash
claude mcp add --transport http --scope user open_mess http://127.0.0.1:7341/mcp --header "Authorization: Bearer <token>"
```

The token is generated at the daemon's first start and kept in Windows Credential Manager. It is
shown only by `om mcp-command`; keep it to yourself, since anything holding it can use the tools.

Trigger notifications reach a Claude Code session through Claude Code's channels, which take them
only from MCP servers it starts over stdio: `om mcp-stdio` is such a server and forwards everything
to the daemon. Add it only to the sessions you start with channels, not to every project: Claude
Code never tells a server whether channels are on, so open_mess counts every `om mcp-stdio` as a
session to notify, and a notification sent to a session without channels is lost. For example, with
`open-mess-channel.json` holding
`{"mcpServers": {"open_mess_channel": {"command": "om", "args": ["mcp-stdio"]}}}`:

```bash
claude --mcp-config open-mess-channel.json --dangerously-load-development-channels server:open_mess_channel
```

(`--dangerously-load-development-channels` is how Claude Code loads channels outside its approved
list during its research preview of channels.)

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
- **Triggers**: wake an agent when someone writes to you (into an open Claude Code session, or as
  a new agent run), with batching, quiet hours and approval before sending.
- **Coexistence**: open_mess acts like a separate device. Your official Teams keeps working as
  before, and reading through open_mess never marks anything as read.

## Local call recording (Windows)

open_mess can record your Teams calls on your own computer and turn them into text there, with
Whisper (large-v3-turbo) on the CPU. It is off until you turn it on in the configuration window
(Nagrywanie rozmów); the model, about 1.6 GB, is downloaded only from there. The design is
[`.specs/2026-10-28-local-transcription.md`](.specs/2026-10-28-local-transcription.md) (in Polish).

- **What is recorded**: from the moment a Teams call starts until Teams lets go of its audio,
  about three minutes after you hang up, everything the device Teams plays the call on plays (the
  other participants, but also notifications and other apps), and your microphone only while it is
  on in Teams. A toast and the tray show each recording and can stop it; a stopped call is not
  recorded again.
- **Where it stays**: `%LOCALAPPDATA%\open_mess\recordings`, never in a roaming profile, never in
  logs, and open_mess sends it nowhere; an agent you let read the transcripts passes them on to
  its model provider, as with messages. The audio is deleted once it is transcribed (or after 7 days if it
  cannot be); the text stays until you delete it.
- **Who reads it**: agents with the read permission of "Nagrania lokalne" (on by default), through
  `list_recordings` and `get_recording_transcript`, with the text marked as other people's words;
  agents can stop a recording (`stop_recording`) but never start one. You read your transcripts in
  the window, whatever agents may.
- **Telling the participants** is up to you: open_mess reminds you with every recording and sends
  nothing to them. Recording calls may be against your organisation's policies or the law where you
  are; that decision is yours.

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
