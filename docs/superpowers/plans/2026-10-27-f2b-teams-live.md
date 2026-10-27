# Milestone F2b: Teams live - checklist with the user

Not a coding plan: a session the owner runs at the computer with the coordinating agent, on the
corporate account (spec 9.5: user present, least suspicious sign-in first, live writes only in the
self-chat `48:notes`, minimal traffic). The agent never clicks approvals or sign-in UI and never
prints other people's message content; the owner runs the commands that show content.

Record every result (redacted: no tokens, tenant or user ids, names or message text) in
`docs/protocols/teams.md` under "Live results", and turn every surprise into a fix with a test.

## 0. Before

- [ ] Main has the post-merge review fixes; `cargo xtask ci` green.
- [ ] `cargo build --release -p open-mess -p open-mess-teams --target ...` (the module for
      `wasm32-wasip2`), copy `open_mess_teams.wasm` into `%APPDATA%\open_mess\modules\`.
- [ ] Owner decides whether the old `teams-wrapper` (scheduled task `TeamsWrapperMCP`, a hidden
      Chromium with Teams Web) is stopped for the test week, so it cannot be confused with
      open_mess when looking at the official client.
- [ ] Owner checks how the official Teams looks now (a few chats with known unread state), to
      compare later.

## 1. Daemon, window, module

- [ ] Owner starts `om daemon` in a terminal; the tray icon appears.
- [ ] `om config`; Moduły shows `teams` awaiting approval with its hosts and WAM identity
      (`1fec8e78-...`, 3 scopes); owner approves.

## 2. Sign-in (WAM)

- [ ] Konta -> Dodaj konto -> Teams; the picker lists the work accounts; owner picks the right one.
- [ ] The account goes online; `om status` shows it; `om accounts`.
- [ ] Restart the daemon; the account comes back online with no prompt (silent WAM).

## 3. Reading (owner runs, agent sees only counts and status codes)

- [ ] `om list_conversations --account <id> --json` (count, kinds).
- [ ] `om read_messages --account <id> --conversation 48:notes` and one real chat.
- [ ] `om unread`, `om mentions`, `om search --query <word>` (search is the least known part:
      docs/protocols/teams.md section 6).
- [ ] `om get_attachment` on an image in the self-chat.
- [ ] Official Teams: the chats read through om are still unread if they were unread before; no
      phone notification behaved differently.

## 4. Writing, only in `48:notes`

- [ ] Permissions stay "z potwierdzeniem".
- [ ] `om send --account <id> --conversation 48:notes --text "open_mess test"`; a toast appears;
      owner clicks Zatwierdź; the message is in the self-chat in the official client.
- [ ] `om edit_message` on it (toast with before/after, owner approves); `om react` (like).
- [ ] Owner rejects one send; nothing is posted.
- [ ] `om pending` shows the history without text.

## 5. Events and triggers

- [ ] Wyzwalacze: a DM rule with the channel action; start Claude Code with the channel command from
      the window; a colleague (or the owner from the phone, as a different account if possible)
      writes a DM; the session gets one notification after the quiet period.
- [ ] Own messages wake nothing.

## 6. Open protocol questions (read-only captures with REA, owner present)

- [ ] Where `appConsumptionHorizon` comes from (CSA `teams/users/me` is the candidate); then
      `unread` can say "unread" instead of "unknown".
- [ ] `v1/threads/<id>/members` field names (approval shows display names).
- [ ] Substrate search request shape; AMS download auth; channel replies (`;messageid=`).

## 7. Coexistence week

- [ ] The daemon runs every day; owner notes any oddity in the official client (logouts, empty
      chats, read state, phone notifications).
- [ ] Every day `om status` shows the Teams account online (spec 4.2: open_mess itself stays
      signed in); a day it is not counts as a failure of the week.
- [ ] After 7 days with none: spec 4 success criteria 2 and 3 are met.
