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

Current decisions worth recording as they land:

- daemon-owned agent sessions with sequenced event replay
- postcard on the local agent socket (and why wire structs keep every field)
- TUI as a client, not an agent host
- summary compaction with a retained recent tail
- explicit permission precedence and OS sandbox as the real boundary
