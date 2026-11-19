# 003: TUI as a thin client behind a Cargo crate boundary

- Status: accepted
- Date: 2026-11-19

## Context

In early iterations, the agent loop, permissions, MCP, skills, and `ratatui` console all lived inside `envoy-cli`. Keeping TUI rendering pure (no provider calls, no daemon mutations, no `ratatui` types leaking into agent code) relied on human discipline rather than compiler enforcement, and every TUI tweak triggered a broad rebuild.

## Decision

The terminal console (`crates/envoy-cli/src/tui/`) is strictly a client of the daemon:
- Rendering (`tui/view/`) is pure over `App` state and never performs I/O, calls providers, or mutates daemon state.
- All agent/runtime/permissions/skills/MCP/auth/session/worktree logic lives in a separate library crate (`crates/envoy-agent`) with zero dependency on `ratatui`.
- Workspace dependency direction is strictly one-way and Cargo-enforced: `envoy-proto -> envoy-core -> envoy-service -> envoy-agent -> envoy-cli`.

## Consequences

- **Easier:** The compiler prevents TUI types from leaking into the agent or daemon; TUI-only changes compile faster; headless (`envoy -p`) and future frontends can consume `envoy-agent` directly.
- **Harder:** Cross-boundary types shared with the TUI or headless mode must have explicit `pub` APIs on `envoy-agent`.
- **Explicitly not done:** No splitting `envoy-agent` into micro-crates per subsystem until a concrete external consumer requires it.
