# Plan: modularize agent and console

- Status: active
- Last updated: 2026-11-16

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
- [ ] Enable the hard file-size check with the split files removed from the
      baseline. (state, view and llm entries removed; auth/main/mcp/service
      remain.)
- [ ] Context compaction module.
- [ ] Permission module.

## Constraints

- Mechanical moves only in this phase; no behavior changes.
- Each step compiles and passes `cargo test --workspace`.
- The daemon remains authoritative for turn state; the console stays a client.
- Preserve the user's uncommitted work before starting a split.

## Next

Phase 1 is complete through the agent split. Next is phase 2: add
`agent/context/` (budget, pruning, structured summary, overflow retry) before
permissions. `auth.rs`, `main.rs`, `mcp.rs` and two service files remain in
the size baseline; split them opportunistically when they are touched.

## Verification

```bash
cargo test --workspace
cargo check --workspace --all-targets
python3 scripts/check-file-sizes.py
```

PTY smoke: two turns, steer, queue, recall, interrupt, exit with daemon alive.
