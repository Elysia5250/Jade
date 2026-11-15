# Envoy (`envoy/`)

Detached remote execution, a policy gate, and a stdio MCP server. Zero network
dependencies: peers are reached through whatever transport you plug in, and the
included `conduit` bridge is the only crate that knows about one.

## The one idea

**Interaction and execution are separate layers.**

```
client ──short RPC──▶ envoy-service ──▶ Jobs (daemon)  ──▶ child process group
   submit / jobs / status          │        queue + concurrency
   logs(cursor) / cancel           │        log file per job
                                   └──▶ nothing waits on the client
```

* `submit` returns a **job id immediately**; the process keeps running.
* `logs` is a **cursor into a file**, not a connection. Drop it, reattach later,
  read from the same byte offset. Dropping a watcher never kills a job.
* `cancel` is an explicit RPC. It kills the whole process group.
* Jobs are queued per **group** with a concurrency limit; `max_jobs` in
  `agent.json` is the default group's limit.

That is also what makes a multiplexed transport viable: a stream carrying a job
watch can live for minutes without blocking anything else.

## Layers

| Layer | Where | Responsibility |
|---|---|---|
| Interaction | `envoy-service` (server/client) | One short request per stream: submit, list, status, log cursor, cancel, file ops |
| Execution | `envoy-core::job` / `exec` / `log` | Job table, queueing, process groups, timeouts, cancellation, log records |
| Policy | `envoy-core::config` | `agent.json`: enabled + caller allowlist + `max_jobs` |
| Transport | `envoy-service::transport` | `AgentDialer` / `AgentStream` / `AgentHost`; Unix socket now, conduit later |
| Assembly | `bridge/` (standalone crate) | Mounts the agent on a conduit session; resolves device names to authenticated ids |

## Crates

- `envoy-proto`: protocol v2 — `Op`, `Reply`, `ClientMsg`, `Job`, `JobState`,
  `JobResult`, `LogRecord`, and length-prefixed postcard framing.
- `envoy-core`: job table (`Jobs`), detached execution (`exec`), append-only job
  logs with resume cursors (`log`), filesystem primitives (`fs`), policy
  (`config`), audit trail.
- `envoy-service`: `AgentServer::serve_stream` (one request per stream) and a
  client (`Remote<D>`, plus stream-level helpers). Transport traits.
- `envoy-cli` (`envoy`): daemon (`envoy serve`), job and file commands, the
  interactive console (`envoy ui`), and the stdio MCP server.
- `bridge/` (`envoy-bridge`): conduit adapter — **not** a workspace member, so
  `envoy` builds without `conduit` checked out.

## Quick start

The daemon auto-starts on first use; jobs outlive your terminal.

```bash
envoy run "make -j8 && echo built"     # queue, follow, exit with its code
envoy submit "cargo test"             # print a job id and return
envoy jobs
envoy logs 7 --follow                 # reattach at any time
envoy logs 7 --offset 4096            # page a finished job's output
envoy cancel 7

envoy read  /etc/hosts                # files stream over the same socket
envoy write notes.txt "hello"         # atomic: .part + rename
envoy ls    ~/src
envoy copy local a.bin local /tmp/b.bin
```

`envoy run` is `submit` + `logs --follow`. With a single argument it runs
through `sh -c`; multiple arguments are executed verbatim (no shell) unless
`--shell` is passed.

## Terminal console

`envoy ui` is the interactive view. Its layout and vocabulary are copied from
opencode's TUI, because that is the terminal idiom this repo already lives in:

```
 envoy ● local   3 jobs · 1 running · 0 queued   follow   opencode
┌ jobs ──────────────────────────────┐┌ #3 · printf ... · done success ─────────┐
│ ✓   1 0s   echo build-ok           ││ line one                                │
│ ●   2 1s   sleep 90                ││ line two                                │
│ ✓   3 0s   printf 'line one...'    ││ warn                                    │
└────────────────────────────────────┘└─────────────────────────────────────────┘
┌ command ──────────────────────────────────────────────────────────────────────┐
│ > _                                                              tab to type │
└───────────────────────────────────────────────────────────────────────────────┘
 ctrl+p palette  n new  c cancel  f follow  tab focus  ctrl+x q quit
```

| Key | Action |
|---|---|
| `tab` / `shift+tab` | cycle focus (jobs → log → command) |
| `j`/`k`, `↑`/`↓`, `g`/`G` | move in the job list |
| `n` | type a command; `enter` submits, `esc` cancels |
| `c` | cancel the selected job |
| `f` | follow output (otherwise `pgup`/`pgdn`, `home`/`end` scroll) |
| `r` | refresh now |
| `ctrl+p` | command palette (new, cancel, follow, sidebar, theme, refresh, quit) |
| `ctrl+x` then `q`/`n`/`c`/`b`/`t`/`s` | leader shortcuts, as in opencode |
| `q` / `ctrl+c` / `ctrl+d` | quit |

It talks to the same daemon over the same short RPCs — a UI exit never affects
a running job.

## MCP tools

`envoy mcp serve` exposes six primitives, in the spirit of a minimal agent
harness: `exec`, `read`, `write`, `edit`, `ls`, `job` (`list` / `status` /
`logs` / `cancel`).

Deliberately absent: `copy` (compose `read` + `write`), policy editing (CLI
only), plans, sub-agents, and any hidden permission layer. The daemon's policy
gate decides what may run; everything heavier is the caller's job.

## Policy and trust

`~/.envoy/agent.json`:

```json
{ "enabled": false, "allow": [], "max_jobs": 4 }
```

- **Local** (same-user daemon socket, mode `0600`) always works: `enabled` and
  the allowlist govern *remote* callers, and a local shell can run the same
  commands anyway.
- **Remote** callers are identified by the transport. Over the conduit bridge
  that is the mTLS-authenticated `device_id`, so the allowlist is no longer a
  string the caller invents:

```bash
envoy enable
envoy allow 3fa9c1d2a0b7e451      # a device id, or the name the bridge resolves
```

## Multi-machine: the bridge

`bridge/` glues `envoy` to `conduit` (QUIC/mTLS sessions, relay fallback, mDNS).
It is a separate workspace on purpose — neither project depends on the other.

```bash
# terminal 1: conduit session engine + the envoy agent service (id 16)
cd bridge && cargo run --release

# terminal 2: any device in the account, by name or id
envoy --on laptop run "uname -a"
envoy --on 3fa9c1d2a0b7e451 submit "sleep 300"
```

The bridge prints the account's devices at startup. Its local socket accepts
`[u32 BE length][device name or id]` and then splices the connection to that
peer's agent service; failures are reported as an agent-protocol `Denied` reply
so the CLI prints the reason instead of a bare EOF. See
[bridge/README.md](./bridge/README.md).

## Test

```bash
cargo test --workspace          # envoy only
cd bridge && cargo test         # needs ../conduit checked out
```

Covered end to end: submit returning before the child exits, a dropped watcher
not killing its job, group queueing, cancel of running and queued jobs, state
recovery across restarts, log cursor resume with partial trailing records,
ambiguous-edit refusal, atomic write rollback, the daemon socket serving jobs
across independent connections, and a rendered TUI frame (header, job list,
output pane, hints) via ratatui's test backend.

## Platform

Unix only for now: the daemon and the bridge both speak over Unix sockets.
