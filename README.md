# Envoy (`envoy/`)

Detached remote execution, a policy gate, and a stdio MCP server. Zero network
dependencies: peers are reached through whatever transport you plug in. The
default transport is `conduit run`'s control socket — over a Unix socket, with
no compile-time coupling in either direction.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/Elysia5250/envoy/main/scripts/install.sh | sh
envoy status          # first run starts the local daemon
```

macOS and Linux, x86_64 and aarch64. No root and no TUN: the agent speaks over
`control.sock`, and works standalone without conduit. Self-hosting releases and
the full matrix are in [docs/INSTALL.md](./docs/INSTALL.md).

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
| Transport adapter | `envoy-service::conduit` | HTTP/WebSocket client of `conduit run`; no conduit crate, no second engine |

## Crates

- `envoy-proto`: protocol v2 — `Op`, `Reply`, `ClientMsg`, `Job`, `JobState`,
  `JobResult`, `LogRecord`, and length-prefixed postcard framing.
- `envoy-core`: job table (`Jobs`), detached execution (`exec`), append-only job
  logs with resume cursors (`log`), filesystem primitives (`fs`), policy
  (`config`), audit trail.
- `envoy-service`: `AgentServer::serve_stream` (one request per stream), a
  client (`Remote<D>`, plus stream-level helpers), transport traits, and the
  `conduit` runtime adapter (`ConduitClient`, `ConduitDialer`, `WsByteStream`,
  accept pool).
- `envoy-cli` (`envoy`): daemon (`envoy serve`), job and file commands, the
  interactive console (`envoy ui`), and the stdio MCP server.
The old `bridge/` crate (which linked the conduit crates and started a second
engine) has been removed — it conflicts with `conduit run` owning the device
identity and TUN. It remains in git history up to `55944b0`.

## Quick start

```bash
cd ~/your-project
envoy                                  # opens the console in this directory
envoy -c                               # continue the most recent session for this directory
envoy login                            # or type /login inside the console (API Key / OAuth)
envoy auth list                        # list stored credentials
envoy models                           # list available models
```

That is the main entry point: bare `envoy` starts the terminal console, with the
current directory as the working directory for everything you run. Before `/login`
it starts in **Shell** mode (commands execute directly as detached daemon jobs);
once connected via `/login` (or env vars like `OPENAI_API_KEY` /
`ANTHROPIC_API_KEY`) it switches to **Agent** mode where prompts stream from
your LLM and invoke `exec`/`read`/`write`/`edit`/`ls`/`job` on `local` or any
`conduit` peer.

Scripted use (the daemon auto-starts; jobs outlive your terminal):

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

`envoy` / `envoy ui` follows `opencode`'s TUI layout and workflow:

- **Inline transcript stream** (main pane): user prompt cards (`┃`), streaming
  assistant Markdown replies (with collapsible `▸ Thinking` reasoning), and
  collapsible tool/shell execution cards (live stdout/stderr, job `#id`,
  elapsed time, exit code, target machine).
- **Right sidebar**: session context (`dir`, `model`, `mode`, `target`, token
  usage) and a live index of detached daemon jobs (`ctrl+x b` or `/sidebar`).
- **Multiline editor**: `Shift+Enter`, `Alt+Enter`, or `Ctrl+J` inserts newlines;
  `Tab` toggles `Agent` / `Shell` mode; `!cmd` always executes directly as a
  shell job.
- **`/` command menu & `/cd` autocomplete**: type `/` to open the filterable
  command menu (`/login`, `/models`, `/cd`, `/machine`, `/sessions`, `/new`,
  `/mode`, `/expand`, `/sidebar`, `/theme`, `/help`, `/quit`); `/cd ` offers
  live subdirectory completion (`Tab` completes, `Enter` switches).
- **Providers & OAuth (`/login`)**: API Keys (OpenAI, Anthropic, DeepSeek,
  Gemini, OpenRouter, OpenCode Zen/Go), OAuth (ChatGPT Plus/Pro Browser PKCE &
  Headless Device Flow, Anthropic Claude Pro/Max PKCE, GitHub Copilot Device
  Flow), and custom OpenAI/Anthropic-compatible endpoints (Ollama, vLLM, relays).
  Credentials are saved to `~/.envoy/auth.json` (`0600`).

| Key | Action |
|---|---|
| `enter` / `shift+enter` | submit prompt or command / insert newline (`alt+enter`, `ctrl+j` also insert newline) |
| `!cmd` | run a shell command directly as a detached job (even in `Agent` mode) |
| `tab` | in editor: toggle `Agent` / `Shell` mode (or complete `/` command / `/cd` dir) |
| `shift+tab` | cycle focus (`input` → `transcript` → `sidebar`) |
| `ctrl+o` | collapse / expand reasoning or tool/shell output cards |
| `/` | filterable command menu (`/login`, `/models`, `/cd`, `/machine`, `/sessions`, …) |
| `ctrl+p` | command palette (login, models, sessions, switch machine, theme, quit) |
| `ctrl+x` then `q`/`n`/`c`/`b`/`t`/`l`/`m`/`s` | leader shortcuts (quit, new session, cancel, sidebar, theme, login, models, refresh) |
| `pgup`/`pgdn`, `ctrl+u`/`ctrl+f`, `g`/`G` | scroll transcript / jump top or bottom |
| `esc` / `ctrl+c` | clear input / cancel active turn or focus transcript / quit |

Bare commands run in the console's working directory (`--cwd` or `/cd` to
change it). Sessions are saved under `~/.envoy/sessions/` (`envoy -c` resumes
the latest session for the current directory). It talks to the same daemon over
short RPCs — a UI exit never affects a running job. When conduit is connected,
the sidebar shows `agents/peers` and `/machine` / `ctrl+p` lists only peers
advertising service 16.

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
- **One daemon per data directory**: `envoy serve` takes an exclusive `flock`
  on `<data-dir>/envoy.lock` before touching the socket, so a second instance
  fails fast instead of unlisting the live socket and sharing `jobs/state.json`.
- `envoy serve --local-only` skips the conduit accept pool (alias:
  `--no-conduit`).
- **Remote** callers are identified by the transport. Over conduit that is the
  mTLS-authenticated `device_id`, so the allowlist is not a string the caller
  invents:

```bash
envoy enable
envoy allow 3fa9c1d2a0b7e451      # the device id from `envoy devices`
```

## Multi-machine: conduit

`conduit run` is the single network daemon on each machine: it owns the device
key, the QUIC/relay sessions, and optionally the TUN device. It exposes all of
that on `~/.conduit/control.sock` as HTTP/1.1 + JSON and WebSocket. **envoy is
just a client of that socket** — no conduit crate, no second engine, no shared
data directory.

```bash
# terminal 1 (each machine): the network daemon
conduit run

# terminal 2 (each machine): the agent daemon; parks 3 idle accept slots
envoy serve

# anywhere in the account
envoy devices                    # names, dns names, virtual ips, status
envoy --on laptop run "uname -a"
envoy --on laptop submit "cargo build --release"
envoy --on 100.64.0.2 logs 12 --follow
```

How it is wired:

- **Outbound** — `GET /v1/services/16/open/{target}` upgrades to a WebSocket and
  becomes a transparent byte stream carrying envoy protocol v2. Conduit resolves
  `device_id`, `dns_name`, `virtual_ip`, or a unique device name, and answers
  `400/404/409/503` with `{"error": "..."}` so the CLI prints the real reason.
- **Inbound** — `envoy serve` keeps a few idle
  `GET /v1/services/16/accept?name=envoy` connections. When a peer opens a
  stream, conduit sends one text frame naming the mTLS-authenticated
  `peer_device_id`, then switches to binary passthrough; envoy wraps it in
  `WsByteStream` and calls `serve_stream(peer_device_id, stream)`.
- **Identity** — the allowlist therefore matches a device id conduit has
  already authenticated, not a caller-supplied string.
- **Discovery** — `GET /v1/status` and `GET /v1/devices` drive `envoy status`
  and `envoy devices` (its `AGENT` column is "advertises service 16").
  `GET /v1/events` feeds the TUI's live `agents/peers` count and its machine
  picker. A peer that does not advertise 16 is filtered out instead of being
  dialled and failing.
- **Refusals** are structured: `{"error": "...", "code": "..."}` with stable
  codes (`device_not_found`, `ambiguous_target`, `peer_offline`,
  `service_not_registered`, ...), rendered as
  `... refused: <reason> [<code>, HTTP <status>]`.

The control socket path comes from `--conduit-socket`, `CONDUIT_CONTROL_SOCK`,
`$CONDUIT_DATA_DIR/control.sock`, or `~/.conduit/control.sock`.

This path has been exercised end to end against the real thing: local
`conduit-coord`, two devices joined into one account, two `conduit run
--no-tun` daemons, two `envoy serve` instances, then `envoy --on <device> run`
over the WebSocket bridge — including the mTLS caller id in the job owner and
the allowlist refusal path.

**Agent traffic never uses L3.** conduit's `100.64.0.0/10` + `*.conduit` is for
your own tools (ssh, curl, databases, grain); the agent protocol stays on the
WebSocket bridge, where every stream carries an mTLS-authenticated device id.
That is also why `envoy` has no `--listen-vpn` and no IP→device reverse
mapping.

## Test

```bash
cargo test --workspace
```

The control-socket contract is covered by a mock `conduit` server
(`crates/envoy-service/tests/conduit_control.rs`): outbound upgrade and a full
job round trip, the accept pool with the handoff metadata and allowlist refusal,
HTTP error propagation, and `/v1/status` + `/v1/devices` parsing. The real
bridge is exercised manually with two local devices; see
[docs/MANUAL-TEST.md](./docs/MANUAL-TEST.md#c-多机需要-conduit-run--账号).

Covered end to end: submit returning before the child exits, a dropped watcher
not killing its job, group queueing, cancel of running and queued jobs, state
recovery across restarts, log cursor resume with partial trailing records,
ambiguous-edit refusal, atomic write rollback, the daemon socket serving jobs
across independent connections, and a rendered TUI frame (header, job list,
output pane, hints) via ratatui's test backend.

## Platform

Unix only for now: the daemon and conduit's control socket both speak over Unix
sockets.
