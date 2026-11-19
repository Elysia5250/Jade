# Architecture

Envoy is a daemon-owned agent with a thin terminal client.

```text
┌─────────────┐  postcard over agent.sock  ┌──────────────────────────────┐
│ envoy-cli   │  Ensure/Resume/Send/...     │ envoy daemon                 │
│ TUI client  │ ──────────────────────────► │ runtime::Runtime             │
│             │ ◄────────────────────────── │  session + event replay      │
└─────────────┘  Snapshot/Event             │  agent loop + tools          │
                                            │  permission checks           │
                                            └──────────────┬───────────────┘
                                                           │ service 16 RPC
                                                           ▼
                                            ┌──────────────────────────────┐
                                            │ envoy-service / conduit      │
                                            │ local or remote jobs         │
                                            └──────────────────────────────┘
```

## Invariants

- The daemon owns the turn, queue, steering, ask broker, and completion. A UI
  exit must not kill a running turn.
- Agent events are sequenced per session. A client persists the last applied
  sequence and replays from it; a daemon restart emits `SessionReset`.
- The TUI is a client. It renders state and sends requests; it does not run the
  agent loop.
- Wire types are postcard-compatible. Keep every field; `skip_serializing_if`
  shifts non-self-describing fields.
- `agent.sock` and control sockets are local, mode `0600`.
- The daemon is the source of truth for `running`; the console may be stale and
  must recover from a snapshot.

## Module map

```text
crates/envoy-agent/src/
  daemon.rs               daemon lifecycle
  session.rs              session/transcript persistence
  memory.rs               project/user memory store
  worktree/               local git worktree management
  auth/                   provider credentials, OAuth/PKCE, routing
  hooks/                  deterministic lifecycle hooks
  runtime/                daemon-side session runtime, client, attach protocol
  agent/
    loop.rs               single agent loop, turn boundaries, overflow retry
    prompt.rs             system prompt, project instructions, skills & todos injection
    events.rs             AgentEvent + round-trip tests
    context/              catalog, tiered harness budgets, pruning, summary, tokens
    tools/                fs, shell, search (glob/grep), web, todo, skill, say/ask
    providers/            openai-chat, openai-codex, anthropic, SSE
  permissions/            modes, rules, paths, shell, approval, audit
  skills/                 SKILL.md discovery, frontmatter parser, trust gate
  mcp/                    bidirectional MCP server (stdio) and client (stdio + HTTP/SSE)

crates/envoy-cli/src/
  main.rs                 CLI entrypoint and command dispatch
  cli_args.rs             clap argument and subcommand definitions
  cli_ops.rs              CLI status, device, and file subcommands
  headless/               non-interactive `envoy -p` / `envoy ask` mode
  tui/
    mod.rs                terminal setup + run loop
    commands.rs           slash command registry (/compact, /handoff, /budget, /permissions, /mcp, /skill, ...)
    state/                App state, split by concern
    view/                 pure rendering, split by screen region
    text.rs               wrapping/markdown helpers
    theme.rs              color tokens
```

## Context lifecycle

1. `build_system_prompt_for_session` produces the stable prefix with `<available_skills>` and `<active_todos>`.
2. `context::catalog` + `context::budget` resolve physical caps (live `/models`, `models.dev`, 2026 catalog) and vendor-native harness compaction tiers (`/budget`).
3. Stage 1 (`context::prune`) folds older tool outputs while keeping recent tail turns untouched.
4. Stage 2 (`context::summary`) summarizes the old head across atomic `assistant(tool_calls)+tool` boundaries, keeping a recent tail, and supports `/handoff` clean session reset.
5. One automatic overflow compaction retry runs if the provider rejects a prompt as too long, feeding back into `model_caps.json`.
6. The daemon emits `Compacting`/`Compacted`/`HandoffReady` events; the TUI updates model history while keeping visual transcript items intact.

## Permission lifecycle

1. A tool call is normalized (tool, action, resource).
2. Explicit rule precedence (`Managed Deny > Local Deny > Hard Protected Guard > Managed Ask > Local Ask > Managed Allow > Local Allow > Mode Baseline`) produces `allow`/`ask`/`deny`.
3. A `deny` short-circuits; an `ask` creates an approval request (`AskBroker`) or fails cleanly on non-interactive MCP stdio.
4. `envoy-core::sandbox` enforces filesystem and network limits after approval. Backends are `bwrap`, then Linux Landlock, then macOS Seatbelt (`sandbox-exec`). With no backend the command is refused. Local and remote agent `exec` default to an offline workspace write. Landlock cannot hide `~/.ssh`, `~/.aws`, or `.env`. Human shell and `envoy run` stay unsandboxed.
5. The decision and sandbox mode are written to the audit log (`~/.envoy/audit/permissions.jsonl`).
6. A rejection is returned to the model with actionable context.

## Where to look

- Protocol: `crates/envoy-proto/src/lib.rs`, `crates/envoy-agent/src/runtime/`
- Sandbox & search: `crates/envoy-core/src/{sandbox.rs,seatbelt.rs,search.rs}`
- Agent loop & tools: `crates/envoy-agent/src/agent/{loop.rs,tools/}`
- Compaction: `crates/envoy-agent/src/agent/context/`
- Permissions: `crates/envoy-agent/src/permissions/`
- Skills & MCP: `crates/envoy-agent/src/{skills,mcp}/`
- Console: `crates/envoy-cli/src/tui/`
- Research and rationale: `docs/HARNESS-RESEARCH.md`
- Cross-repo `conduit` / `grain` / `envoy` split archive: root `ARCHITECTURE.md` (kept in sync across the three repos; this file is the live internal map for `envoy`)
