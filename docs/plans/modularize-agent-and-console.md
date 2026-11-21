# Plan: modularize agent and console

- Status: complete
- Last updated: 2026-11-19

## Goal

Make the console and agent loop small enough to review and change safely:
no source file over 700 lines, no god `App`, no provider/UI coupling. Then add
context compaction and permissions behind their own modules.

## Progress

- [x] Baseline the existing TUI split and context meter.
- [x] Add `scripts/check-file-sizes.py` and a debt baseline.
- [x] Add `AGENTS.md` map and `docs/architecture.md`.
- [x] Split `tui/state.rs` into `state/{mod,agent,jobs,login,modals,commands,input}.rs`.
- [x] Split `tui/view.rs` into `view/{mod,transcript,editor,status,sidebar,queued,ask,menus,modals}.rs`.
- [x] Split `llm.rs` into `agent/{loop,prompt,events,asks}` and
      `agent/{tools,providers}/`.
- [x] Enable the hard file-size check with the split files removed from the
      baseline. (`scripts/file-size-baseline.txt` is now empty and CI enforces
      the 700-line hard limit across the workspace.)
- [x] Context compaction module (`agent/context/`, `/compact`, `/handoff`, `/budget`, vendor harness tiers, overflow retry, `runtime/` split).
- [x] Permission module (`permissions/`, OS `bwrap` workspace sandbox, `Shift+Tab`, `/permissions`, `/plan`, `/trust`).
- [x] Toolchain, Skills, and Bidirectional MCP (`glob`, `grep`, `webfetch`, `todo`, `skill`, `skills/`, `mcp/` server + stdio/HTTP-SSE client, `/mcp`, `/skill`).

## Constraints

- Mechanical moves only in this phase; no behavior changes.
- Each step compiles and passes `cargo test --workspace`.
- The daemon remains authoritative for turn state; the console stays a client.
- Preserve the user's uncommitted work before starting a split.

## Next

Complete. Follow-on structural work (extracting `jade-agent`, hardening CI,
and trimming files near the 700-line cap) is tracked in
`docs/plans/extract-agent-crate-and-harden-ci.md`.

## Verification

```bash
cargo test --workspace
cargo check --workspace --all-targets
python3 scripts/check-file-sizes.py
```

PTY smoke: two turns, steer, queue, recall, interrupt, exit with daemon alive.
