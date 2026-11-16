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
- [ ] Split `tui/state.rs` into `state/{mod,agent,jobs,login,modals,commands,input}.rs`.
- [ ] Split `tui/view.rs` into `view/{mod,transcript,editor,status,sidebar,queued,ask,menus,modals}.rs`.
- [ ] Split `llm.rs` into `agent/{loop,prompt,events,asks}` and
      `agent/{tools,providers}/`.
- [ ] Enable the hard file-size check with the split files removed from the
      baseline.
- [ ] Context compaction module.
- [ ] Permission module.

## Constraints

- Mechanical moves only in this phase; no behavior changes.
- Each step compiles and passes `cargo test --workspace`.
- The daemon remains authoritative for turn state; the console stays a client.
- Preserve the user's uncommitted work before starting a split.

## Next

Split `tui/state.rs` by concern, starting with the agent/event methods.

## Verification

```bash
cargo test --workspace
cargo check --workspace --all-targets
python3 scripts/check-file-sizes.py
```

PTY smoke: two turns, steer, queue, recall, interrupt, exit with daemon alive.
