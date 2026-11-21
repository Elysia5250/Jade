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

_None at present._

Completed plans:

- `extract-agent-crate-and-harden-ci.md` — harden CI (clippy/fmt/macOS),
  extract `envoy-agent` out of `envoy-cli`, trim redline files, and record
  core ADRs.
- `modularize-agent-and-console.md` — split the console state/view and the
  agent loop, clear the 700-line baseline, and land compaction/permissions/MCP.

Archived plans (frozen, read-only; see [`docs/achieved/`](../achieved/README.md)):

- `hardening-and-modernization.md` — closed OS sandbox, permission, MCP, and
  SSRF gaps; fixed data loss, protocol versioning, and concurrency bugs;
  aligned prompt caching and reasoning; slimmed the TUI. Phases 1–8 complete.
- `handoff-phase3-to-phase8.md` — the Phase 3–8 takeover manual (invariants,
  pitfalls, quantitative gates). Its phases shipped in `35b24bb`.

