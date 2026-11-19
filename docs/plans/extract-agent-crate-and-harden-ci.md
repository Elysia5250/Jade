# Plan: extract `envoy-agent` crate and harden CI

- Status: complete
- Owner: Elysia
- Last updated: 2026-11-19

## Goal

Turn the architectural rules in `AGENTS.md` into compile-time and CI-enforced
guardrails, so a solo maintainer can resume work in any session without
re-checking invariants by hand:

1. CI runs `fmt`, `clippy -D warnings`, and tests on both Linux and macOS
   (covering `seatbelt.rs` and Darwin paths).
2. `envoy-cli` no longer owns the entire agent/runtime/permissions/MCP stack:
   those live in a dedicated library crate (`crates/envoy-agent`), enforcing
   the one-way boundary `envoy-proto -> envoy-core -> envoy-service -> envoy-agent -> envoy-cli`
   at compile time and speeding up TUI-only rebuilds.
3. Files currently sitting close to the 700-line hard limit (600–690 lines) are
   split along named boundaries before future features push them over.
4. Core irreversible architectural choices are recorded as 1-page ADRs in
   `docs/decisions/`.

## Research summary (2026-11-19)

### 1. CI & lint baseline
- `.github/workflows/ci.yml` currently only runs on `ubuntu-latest` with
  `check-file-sizes.py`, `cargo test --workspace`, and `cargo check --workspace --all-targets`.
- `cargo fmt --all -- --check` currently reports diffs in **66 files**.
- `cargo clippy --workspace --all-targets` currently stops at `envoy-service`
  with:
  - `envoy-core` (2 warnings): `job.rs:293` (`type_complexity`),
    `seatbelt.rs:23` (`single_element_loop`).
  - `envoy-service` (3 errors, 3 warnings): `client.rs:313`, `remote.rs:34`,
    `remote.rs:66` (`clippy::never_loop` — deny by default); `conduit/ws.rs:46`,
    `conduit/ws.rs:150`, `server.rs:506`.
  - `envoy-cli` clippy warnings remain to be audited once `envoy-service` passes.
- Running `cargo test --workspace` on macOS already surfaces a real
  macOS-only test failure that Linux CI missed:
  `tui::tests_ux::welcome_shows_resumes_tag_and_n_starts_fresh_session` fails
  at `tests_ux.rs:200` because macOS's long `/private/var/folders/.../T/...`
  temp path truncates the `resumes` tag in the rendered welcome frame.
- Also noted during `cargo test`: 3 existing compiler warnings in `envoy-cli`
  (`auth/mod.rs:25,31` unused imports, `agent/effort.rs:162` dead code).

### 2. Crate split feasibility (`envoy-cli` -> `envoy-agent` + `envoy-cli`)
- `crates/envoy-cli/src/` currently holds **156 of 179** `.rs` files in the
  workspace.
- Static module dependency audit of `crates/envoy-cli/src/` confirms a clean
  cut already exists:
  - **Runtime/agent cluster (11 modules)**: `agent`, `auth`, `daemon`, `hooks`,
    `mcp`, `memory`, `permissions`, `runtime`, `session`, `skills`, `worktree`.
  - **Zero** modules in this cluster depend on `tui`, `headless`, `cli_args`,
    `cli_ops`, or `main`.
  - Internal cross-dependencies exist *within* the cluster (e.g., `agent` <->
    `auth` / `hooks` / `permissions` / `memory`), so extracting the cluster as
    one `envoy-agent` library crate avoids fighting internal cycles while
    immediately enforcing the `agent -> tui` boundary via Cargo.
  - `envoy-cli` keeps only: `main.rs`, `cli_args.rs`, `cli_ops.rs`,
    `cli_tests.rs`, `headless/`, and `tui/`.
  - `ratatui` and `unicode-width` stay in `envoy-cli` only; `reqwest`,
    `tree-sitter`, `sha2`, `similar`, etc. move to `envoy-agent`.

### 3. Redline files near the 700-line hard limit (non-test, >= 600 lines)
- `crates/envoy-cli/src/tui/mod.rs` — 681 lines
- `crates/envoy-cli/src/permissions/shell.rs` — 667 lines
- `crates/envoy-cli/src/tui/markdown.rs` — 662 lines
- `crates/envoy-cli/src/agent/loop.rs` — 648 lines
- `crates/envoy-cli/src/auth/routing.rs` — 643 lines
- `crates/envoy-service/src/client.rs` — 640 lines
- `crates/envoy-cli/src/permissions/rules.rs` — 634 lines
- `crates/envoy-core/src/job.rs` — 630 lines
- `crates/envoy-cli/src/auth/mod.rs` — 629 lines
- `crates/envoy-cli/src/tui/view/modals.rs` — 628 lines
- `crates/envoy-cli/src/agent/context/catalog.rs` — 627 lines
- `crates/envoy-cli/src/hooks/exec.rs` — 603 lines

## Progress

### Phase 0: Quick hygiene (done)
- [x] Remove legacy `crates/envoy-cli/src/llm.rs` shim and switch remaining 4
      call sites to `crate::agent`.
- [x] Mark `docs/plans/modularize-agent-and-console.md` as `complete` and
      clarify root `ARCHITECTURE.md` vs `docs/architecture.md`.

### Phase 1: CI guardrails (clippy + fmt + macOS) — done
- [x] Fix `clippy::never_loop` errors and warnings in `envoy-core` and
      `envoy-service`.
- [x] Run `cargo clippy --workspace --all-targets -- -D warnings` and fix
      remaining lints in `envoy-cli`.
- [x] Run `cargo fmt --all` and split `tui/tests_ux.rs` -> `tui/tests_ux_layout.rs`
      so formatting stays under the 700-line hard limit.
- [x] Fix macOS-only test failures:
  - `welcome_shows_resumes_tag_and_n_starts_fresh_session`: truncate long
    workspace paths in `tui/view/welcome.rs` before the `  (resumes)` tag.
  - `envoy-service/src/client.rs` `tmpdir`: add atomic sequence + pid to
    prevent parallel macOS tests colliding on 1µs `SystemTime` timestamps.
  - `envoy-service/tests/{conduit_control,stress_server}.rs` `tmpdir`: use
    short `/tmp` paths so Unix socket paths stay under macOS `SUN_LEN` (104 bytes).
- [x] Update `.github/workflows/ci.yml`:
  - add `cargo fmt --all -- --check`
  - add `cargo clippy --workspace --all-targets -- -D warnings`
  - add `macos-latest` matrix runner to test `seatbelt.rs` and Darwin paths.

### Phase 2: Extract `crates/envoy-agent` — done
- [x] Create `crates/envoy-agent/Cargo.toml` and add it to workspace `Cargo.toml`.
- [x] Move the 11 runtime/agent modules (`agent`, `auth`, `daemon`, `hooks`,
      `mcp`, `memory`, `permissions`, `runtime`, `session`, `skills`, `worktree`)
      from `envoy-cli` into `envoy-agent/src/lib.rs` (mechanical move, no
      behavior change; promoted only the 6 cross-crate items to `pub`).
- [x] Update `envoy-cli` imports (`crate::...` -> `envoy_agent::...`) and prune
      agent-only dependencies (`tree-sitter`, `tree-sitter-rust`, `sha2`, `libc`)
      from `crates/envoy-cli/Cargo.toml`.
- [x] Update `AGENTS.md` and `docs/architecture.md` module map to reflect the
      5-crate layout.

### Phase 3: Trim redline files — done (top 4 redline files split; remaining opportunistic)
- [x] Split `tui/mod.rs` (684 -> 508) — extracted `tui/modals.rs` and `tui/terminal.rs`.
- [x] Split `permissions/shell.rs` (677 -> 521) — extracted `permissions/shell_classify.rs`.
- [x] Split `tui/markdown.rs` (662 -> 417) — extracted `tui/markdown_table.rs`.
- [x] Split `agent/loop.rs` (648 -> 361) — extracted `agent/loop_provider.rs`.
- [ ] Remaining 600–645 line files (`auth/routing.rs`, `service/client.rs`,
      `permissions/rules.rs`, `core/job.rs`, `auth/mod.rs`, `tui/view/modals.rs`,
      `agent/context/catalog.rs`, `hooks/exec.rs`) — split opportunistically when touched.

### Phase 4: Core ADRs in `docs/decisions/` — done
- [x] `001-daemon-owned-sessions-and-replay.md`
- [x] `002-postcard-local-wire-protocol.md`
- [x] `003-tui-as-thin-client.md`
- [x] `004-two-stage-compaction-with-tail-retention.md`
- [x] `005-explicit-permission-precedence-and-os-sandbox.md`

## Decisions

- **Terminal-Bench 2.0 deprioritized**: Local hardware is too slow for regular
  Terminal-Bench runs; keep `benchmarks/terminal-bench/` as-is without gating
  engineering work on benchmark runs.
- **Root `ARCHITECTURE.md` preserved**: It is a cross-repo archive shared with
  `conduit` and `grain`; `docs/architecture.md` remains the live internal map
  for `envoy`.
- **Single `envoy-agent` crate first**: Do not try to split `agent`,
  `permissions`, `runtime`, `mcp`, and `skills` into five tiny crates up front;
  extracting the cohesive 11-module cluster gives 90% of the compile-time and
  boundary benefits with minimal churn.

## Next

Complete. Remaining 600–645 line files can be split opportunistically when
touched; CI now enforces `fmt`, `clippy -D warnings`, `check-file-sizes.py`,
and full tests on both Linux and macOS.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --workspace --all-targets
python3 scripts/check-file-sizes.py
```
