# Execution plans

Long-running work gets a checked-in plan so a later session can resume without
guessing. Keep one file per initiative, named by outcome.

```markdown
# Plan: short outcome

- Status: active | complete | abandoned
- Owner:
- Last updated: YYYY-MM-DD

## Goal
What must be true when this is done?

## Progress
- [x] ...
- [ ] ...

## Decisions
What changed since the plan was written?

## Next
The smallest next action, and how to verify it.

## Verification
Exact commands / PTY checks.
```

Active plans:

- `modularize-agent-and-console.md` — split the console state/view and the
  agent loop before adding compaction and permissions.
