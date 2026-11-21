# Decisions

One short ADR per choice that is expensive to reverse. Keep it to a page:

```markdown
# NNN: Title

- Status: proposed | accepted | superseded
- Date: YYYY-MM-DD

## Context
What problem forced a choice?

## Decision
What did we choose?

## Consequences
What gets easier, what gets harder, and what did we explicitly not do?
```

Recorded decisions:

- `001-daemon-owned-sessions-and-replay.md` — daemon-owned agent sessions with sequenced event replay
- `002-postcard-local-wire-protocol.md` — postcard on the local agent socket (and why wire structs keep every field)
- `003-tui-as-thin-client.md` — TUI as a thin client behind the `jade-agent` crate boundary
- `004-two-stage-compaction-with-tail-retention.md` — two-stage context compaction with a retained recent tail
- `005-explicit-permission-precedence-and-os-sandbox.md` — explicit permission precedence and OS sandbox as the real boundary
