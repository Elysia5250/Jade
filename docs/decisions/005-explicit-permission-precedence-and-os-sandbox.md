# 005: Explicit permission precedence and OS sandbox as the real boundary

- Status: accepted
- Date: 2026-11-19

## Context

Coding agents run arbitrary shell commands and file edits on developer machines. Pure string-matching allowlists over shell commands are trivially bypassed via subshells, wrappers, symlinks, or interpreters. Conversely, prompting on every read-only command makes the agent unusable.

## Decision

Authorization and isolation are split into two explicit layers:
1. **Policy gate (`envoy-agent::permissions`):** Normalizes every tool call and evaluates an explicit precedence ladder with no implicit "more specific wins" heuristics:
   `Managed Deny > Local Deny > Hard Protected Guard (.git, .ssh, .env, .envoy, .codex, .agents, .aws) > Managed Ask > Local Ask > Managed Allow > Local Allow > Mode Baseline (default | accept-edits | plan | auto | turbo)`.
   Paths are canonicalized and symlinks resolved; compound shell commands and wrappers are split, and any shell parse failure falls back to `ask`. Every decision is appended to `~/.envoy/audit/permissions.jsonl`.

## Amendment (2026-11-20): `auto` and `turbo`

`auto` and `turbo` short-circuit **after** the deny rules and before the
hard protected-path / parse-failure / sudo guards: they return `allow` instead
of `ask`, because the OS sandbox below is the boundary that makes those asks
redundant. `auto` only relaxes when `SandboxCapabilities::enforces_workspace_boundary()`
is true (bwrap or Seatbelt, i.e. secrets hidden and subpaths protected);
without such a backend it keeps the old conservative asks. `turbo` relaxes
unconditionally and is a deliberate opt-in, so it is rendered in red.

The hard floor is unchanged: explicit managed/local `deny` rules still win in
both modes, and a managed-policy load error still fails closed. `ask` rules and
saved approvals are concessions relative to the mode, so these two modes
override them.
2. **OS sandbox (`envoy-core::sandbox`):** Enforces the real filesystem and network boundary after approval using `bwrap`, then Linux Landlock, then macOS Seatbelt (`sandbox-exec`). If isolation is requested and no backend exists, execution **fails closed**. Local and remote agent `exec` default to offline workspace-write. Human shell commands and `envoy run` remain unsandboxed.

## Consequences

- **Easier:** Security guarantees do not rest on fragile shell regexes; policy precedence is predictable and auditable; saved approvals can never override a deny.
- **Harder:** Sandbox behavior varies slightly across OS backends (notably Linux Landlock cannot hide `~/.ssh`, `~/.aws`, or `.env`, which is documented explicitly); macOS Seatbelt and Linux `bwrap` both require platform testing in CI.
- **Explicitly not done:** No Windows native sandbox yet (Unix-only); no silent fallback to unsandboxed execution when a sandbox policy is requested.
