# AGENTS.md

This repository is the Envoy coding agent: a daemon-owned single agent loop,
a terminal console, a remote executor, and a small protocol crate. Keep the
map short; deeper design notes live in `docs/`.

## Commands

```bash
cargo test --workspace
cargo check --workspace --all-targets
cargo build --release -p envoy-cli
python3 scripts/check-file-sizes.py
```

The local release flow is:

```bash
install -m 0755 target/release/envoy ~/.envoy/bin/envoy
systemctl --user restart envoy
```

Use `docs/MANUAL-TEST.md` for the PTY checklist (two turns, steer, queue,
recall, interrupt, daemon survival).

## Source layout

- `crates/envoy-proto`: wire types and framing. No domain logic, no UI.
- `crates/envoy-core`: job store and execution primitives. No UI, no provider
  SDKs.
- `crates/envoy-service`: daemon/server, conduit adapter, remote RPC. No TUI.
- `crates/envoy-agent`: daemon-owned `runtime/`, `agent/` loop (providers,
  tools, prompt, context compaction), `permissions/`, `hooks/`, `skills/`,
  `mcp/`, `auth/`, `session`, `memory`, `worktree`, and `daemon`. No `ratatui`
  or TUI types.
- `crates/envoy-cli`: `envoy` binary entrypoint, CLI dispatch, `headless/`,
  and the `tui/` console client. `tui/view/` reads snapshots from `inputs`
  and must not perform I/O, call providers, or mutate daemon state. `drive`
  redraws on events and animation ticks.

Dependency direction is one-way and Cargo-enforced:
`proto -> core -> service -> agent -> cli`. Do not make a lower layer depend
on the TUI or on a concrete provider.

## Size and shape rules

- **Source files:** soft 400 lines, hard 700 lines (tests and generated files
  excluded by the checker). The hard limit is enforced by
  `scripts/check-file-sizes.py` in CI.
- **Functions:** soft 60 lines, hard 100 lines. Split by a named concept, not
  by arbitrary line count.
- **Nesting:** prefer depth <= 3; use early returns.
- **One public concept per module.** No god `App` with every concern in one
  `impl`. Compose state objects (`AgentState`, `SessionState`, `JobState`,
  `LoginState`, `UiState`) instead.
- **No speculative helpers.** Inline single-use logic; extract when it names a
  real boundary or is reused.
- **No new dependency** unless it lives behind a narrow trait or adapter and
  the reason is in the PR/commit.

## Wire and events

- `AgentEvent` and session messages cross `agent.sock` via postcard. Postcard is
  not self-describing: do not use `skip_serializing_if` on wire structs.
- Every new event needs a round-trip test and a sequence-number/replay story.
- Keep the daemon authoritative for `running`, queue, and turn completion. The
  console must never guess a turn is over.

## Permissions

- The rule engine decides `allow` / `ask` / `deny`; the OS sandbox is the real
  boundary. Never treat shell string matching as a security boundary.
- Precedence is explicit: deny > ask > allow, with managed policy above local
  settings. Do not add implicit "more specific wins" behavior. In relaxed modes
  (`turbo` always, `auto` when the active OS sandbox enforces the workspace
  boundary, mutating/`exec` targets stay inside the workspace or temp dir, and
  the call does not request non-allowlisted outbound network), non-protected
  calls short-circuit to `allow` right after deny rules.
- Canonicalize paths, resolve symlinks, protect `.git`, `.ssh`, `.env`,
  `.envoy`, `.codex`, `.agents`, and `.aws` (protected paths always `ask` even
  in `auto`/`turbo`).
- Shell checks split compound commands and wrappers; parse failure means `ask`
  (outside the relaxed-mode short-circuit above).
- Plan mode is read-only except for its own plan files.
- Every approval decision is audit-logged. Saved approvals are scoped; they
  never override a deny.

## Docs

- `AGENTS.md` is a map, not an encyclopedia. Keep it under 200 lines.
- `docs/architecture.md`: current module map and invariants.
- `docs/decisions/`: one short ADR per irreversible choice.
- `docs/plans/`: active execution plans with progress and next steps.
- `docs/achieved/`: finished, frozen documents kept for reference only. Never
  update them for semantic changes; treat `architecture.md`, `decisions/`, and
  `MANUAL-TEST.md` as authoritative instead.
- Nested `AGENTS.md` files may add crate-specific rules; the nearest file wins.
- Generated code is never hand-edited; document the generator command.

## Testing

- Unit tests live beside the module. Integration tests live in `tests/`.
- Prefer a real implementation or a small fake over mocks.
- Add a regression test for every bug fix.
- For context compaction: test trigger, tail retention, summary recall, cache
  ordering, and one overflow retry.
- For permissions: test deny/allow precedence, compound shell, symlinks,
  `.env` protection, and parse failure.
- For TUI: a `TestBackend` render plus a PTY smoke test for interactive flows.

## Commits

Use conventional commit titles: `type(scope): summary`, with types `feat`,
`fix`, `refactor`, `test`, `docs`, `chore`. Keep PRs focused. For a mechanical
move, say so and keep behavior unchanged; put feature changes in a separate
commit.
