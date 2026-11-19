# 001: Daemon-owned agent sessions with sequenced event replay

- Status: accepted
- Date: 2026-11-19

## Context

Terminal coding agents typically run the LLM turn loop inside the foreground TUI process. If the terminal closes, SSH drops, or the user restarts the console, the running turn, queued follow-ups, and detached shell jobs are lost or left orphaned. Furthermore, local consoles can easily develop stale `running` booleans when racing with rapid user input or steering.

## Decision

The `envoy` daemon (`envoy-agent::runtime::Runtime`) is the sole owner of:
- active turn execution, cancellation, and completion,
- prompt steering and follow-up queues,
- interactive `AskBroker` approval/question state,
- detached job execution (`envoy-core::Jobs`), and
- a monotonically increasing per-session event sequence (`SeqEvent`).

Clients attach over `agent.sock` with their last applied sequence number and replay missed events. If the daemon restarts and sequence continuity is lost, it emits `SessionReset` and clients recover from a fresh snapshot. `enter` always sends `Prompt` to the daemon, which authoritatively decides whether to start a new turn or steer the active one.

## Consequences

- **Easier:** Closing or crashing the console never kills a running turn or detached job; reattaching restores exact state; steering and queuing cannot race with stale client flags.
- **Harder:** All interactive state (`Ask`, permissions, compaction progress, checkpoints) must cross an IPC boundary and be serializable.
- **Explicitly not done:** No in-process TUI-hosted agent loop; no client-side turn-completion heuristics.
