# Architecture

Jade is a daemon-owned agent with a thin terminal client.

```text
┌─────────────┐  postcard over agent.sock  ┌──────────────────────────────┐
│ jade-cli   │  Ensure/Resume/Send/...     │ jade daemon                 │
│ TUI client  │ ──────────────────────────► │ runtime::Runtime             │
│             │ ◄────────────────────────── │  session + event replay      │
└─────────────┘  Snapshot/Event             │  agent loop + tools          │
                                            │  permission checks           │
                                            └──────────────┬───────────────┘
                                                           │ service 16 RPC
                                                           ▼
                                            ┌──────────────────────────────┐
                                            │ jade-service / topaz      │
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
- `tui/view/` only reads in-memory snapshots. Catalog, preview, diff, and the
  workspace index refresh outside `draw`. The console redraws on input, daemon
  events, and animation ticks.
- Wire types are postcard-compatible. Keep every field; `skip_serializing_if`
  shifts non-self-describing fields.
- `agent.sock` and control sockets are local, mode `0600`.
- The daemon is the source of truth for `running`; the console may be stale and
  must recover from a snapshot.
- The console keeps its own width-aware markdown renderer (linked spans, tables,
  and the decrypt view). It does not take a `pulldown-cmark` dependency.

## Module map

```text
crates/jade-agent/src/
  daemon.rs               daemon lifecycle
  session.rs              session/transcript persistence
  memory.rs               project/user memory store
  worktree/               local git worktree management
  auth/                   provider credentials, OAuth/PKCE, routing
  hooks/                  deterministic lifecycle hooks
  plugins/                declarative plugin bundles (plugin.json) across Managed/User/Project
  commands/               custom /command markdown prompt templates
  runtime/                daemon-side session runtime, MCP pool, client, attach protocol
  agent/
    loop.rs               single agent loop, turn boundaries, overflow retry
    prompt.rs             system prompt, project instructions, skills, todos & active mode injection
    events.rs             AgentEvent + round-trip tests
    context/              catalog, tiered harness budgets, pruning, summary, tokens
    tools/                narrow Tool trait/registry + fs, shell, search, web, todo, skill, say/ask/mode
    providers/            narrow ModelProvider trait/registry + openai-chat, openai-codex, anthropic, SSE
  permissions/            modes, rules, paths, shell, approval, audit
  skills/                 SKILL.md discovery, frontmatter parser, trust gate
  mcp/                    bidirectional MCP server (stdio) and pooled client (stdio + HTTP/SSE, resources/prompts, deferred search)

crates/jade-cli/src/
  main.rs                 CLI entrypoint and command dispatch
  cli_args.rs             clap argument and subcommand definitions
  cli_ops.rs              CLI status, device, and file subcommands
  headless/               non-interactive `jade -p` / `jade ask` mode
  tui/
    mod.rs                terminal setup; paint syncs snapshots, then draws
    drive.rs              event-driven console loop
    inputs.rs             catalog (including custom /commands), file preview, and diff snapshots
    commands.rs           slash command registry (/compact, /handoff, /budget, /permissions, /mcp, /plugin, /skill, ...)
    state/                App state, split by concern
    view/                 pure rendering over those snapshots
    links.rs              workspace index, depth-capped, inode-deduped
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
2. Explicit rule precedence (`Managed Deny > Local Deny > Hard Protected Guard > Managed Ask > Local Ask > Managed Allow > Local Allow > Mode Baseline`) produces `allow`/`ask`/`deny`. `auto` and `turbo` short-circuit to `allow` right after the deny rules (turbo always, auto only when the sandbox enforces the workspace boundary), except for protected paths.
3. A `deny` short-circuits; an `ask` creates an approval request (`AskBroker`) or fails cleanly on non-interactive MCP stdio.
4. `jade-core::sandbox` enforces filesystem and network limits after approval. Backends are `bwrap`, then Linux Landlock, then macOS Seatbelt (`sandbox-exec`). With no backend the command is refused. Local and remote agent `exec` default to an offline workspace write. Landlock cannot hide `~/.ssh`, `~/.aws`, or `.env`. Human shell and `jade run` stay unsandboxed.
5. The decision and sandbox mode are appended to `<data_dir>/agent/permission_audit.jsonl` (the data directory defaults to `~/.jade`). Hook decisions go to `<data_dir>/agent/hook_audit.jsonl`, and agent invocations to `<data_dir>/agent/audit.log`. Each of those logs rotates at 10 MiB, keeping one previous generation named with a `.1` suffix.
6. A rejection is returned to the model with actionable context.

## Where to look

- Protocol: `crates/jade-proto/src/lib.rs`, `crates/jade-agent/src/runtime/`
- Sandbox & search: `crates/jade-core/src/{sandbox/,seatbelt.rs,search.rs}`
- Agent loop & tools: `crates/jade-agent/src/agent/{loop.rs,tools/}`
- Compaction: `crates/jade-agent/src/agent/context/`
- Permissions: `crates/jade-agent/src/permissions/`
- Skills & MCP: `crates/jade-agent/src/{skills,mcp}/`
- Console: `crates/jade-cli/src/tui/`
- Research and rationale: `docs/achieved/HARNESS-RESEARCH.md` (archived snapshot, 2026-11-16)
- Cross-repo `topaz` / `grain` / `jade` split archive: root `ARCHITECTURE.md` (kept in sync across the three repos; this file is the live internal map for `jade`)
