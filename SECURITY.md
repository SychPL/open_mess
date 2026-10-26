# Security

open_mess holds sign-in sessions for your messaging accounts and can send messages as you, so
security bugs here are serious.

## Reporting

Please report vulnerabilities privately through the repository's private vulnerability reporting
(Security tab, "Report a vulnerability"), not in a public issue. Include the version or commit,
steps to reproduce and the impact you see. Never include real tokens, cookies or message
contents; redact them.

## In scope

- Leaks of tokens, refresh tokens, the local API token or message contents (logs, files,
  crash output, network).
- Ways to reach the local API or MCP endpoint from another machine or without its token.
- Bypassing agent permissions: sending without the required confirmation, reading accounts or
  conversations an agent isn't allowed to read.
- Message content from other people being treated as instructions (prompt injection through
  triggers or tools).
- A WebAssembly module escaping its sandbox: reaching hosts outside its manifest, or another
  module's secrets.
- Anything that changes or damages the official clients' data.

## Out of scope

- The platforms' own services and their terms of use.
- Attacks that need control of the user's Windows account already.
