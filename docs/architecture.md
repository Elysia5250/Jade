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

## Module map (target)

```text
crates/envoy-cli/src/
  main.rs                 CLI parsing and command dispatch only
  daemon.rs               daemon lifecycle
  session.rs              session/transcript persistence
  runtime.rs              daemon-side session runtime + attach protocol
  agent/
    loop.rs               single agent loop, turn boundaries
    prompt.rs             system prompt, project instructions
    events.rs             AgentEvent + round-trip tests
    context/              token budget, pruning, summary compaction
    tools/                fs, shell, job, say/ask
    providers/            openai-chat, openai-codex, anthropic, SSE
  permissions/            modes, rules, paths, shell, approval, audit
  tui/
    mod.rs                terminal setup + run loop
    state/                App state, split by concern
    view/                 pure rendering, split by screen region
    text.rs               wrapping/markdown helpers
    theme.rs              color tokens
```

`llm.rs` is a thin compatibility facade that re-exports `agent::*`; new code
should import from `agent` directly.

## Context lifecycle

1. `build_system_prompt` produces the stable prefix.
2. `ContextBudget` decides whether the model history fits.
3. Cheap pruning drops stale tool output and binary payloads.
4. `Compactor` summarizes the old head while keeping a recent tail.
5. The summary is stored as a handoff and re-injected before the tail.
6. The daemon emits compaction events; the TUI shows progress.
7. Original messages remain persisted; only the model-visible history is
   replaced.

## Permission lifecycle

1. A tool call is normalized (tool, action, resource).
2. Mode + rules produce allow/ask/deny.
3. A deny short-circuits; an ask creates an approval request.
4. The OS sandbox enforces filesystem/network limits after approval.
5. The decision and rule are written to the audit log.
6. A rejection is returned to the model with actionable context.

## Where to look

- Protocol: `crates/envoy-proto/src/lib.rs`, `crates/envoy-cli/src/runtime.rs`
- Agent loop: `crates/envoy-cli/src/agent/loop.rs`
- Compaction: `crates/envoy-cli/src/agent/context/`
- Permissions: `crates/envoy-cli/src/permissions/`
- Console: `crates/envoy-cli/src/tui/`
- Research and rationale: `docs/HARNESS-RESEARCH.md`
