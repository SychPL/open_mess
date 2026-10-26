## Change

What problem does this solve and what behaves differently now? Link the issue.

## Spec

`.specs/<slug>.md`, reviewed by Codex and GLM: final round and verdicts (or "not needed: trivial change").

## Tests

Which tests cover this, and do they fail without the change? Paste the `cargo xtask ci` result.

## Commit reviews

Every commit reviewed by Codex and GLM and reconciled (`.reviews/<sha>.processed`). List P1/P2 findings and how each was resolved.

## Coexistence (platform modules only)

For changes that talk to a platform: checked with the official client running side by side? Anything marked as read, logged out or missing there?

## Checklist

- [ ] No secrets, tokens or real message contents in code, fixtures, logs or this PR
- [ ] No `unwrap` / `expect` / `panic!` / unchecked indexing in non-test code
- [ ] New tool available over both MCP and the CLI
- [ ] New dependencies named here, with their licence and why
