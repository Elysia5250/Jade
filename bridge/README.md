# envoy-bridge

The assembly crate between [`conduit`](../../conduit) (network sessions) and
[`envoy`](../) (execution). It has its own workspace so that `envoy` keeps
building without `conduit` checked out, and `conduit` never learns about agents.

## What it does

1. **Mounts the agent service.** An `EnvoyService` implements
   `conduit_fabric::Service` with `id() == 16`. `on_stream` clones the peer's
   authenticated `device_id` and `tokio::spawn`s
   `AgentServer::serve_stream(device_id, channel)`, then returns immediately.
   This is not optional: the engine loop is serial, and an agent stream can be a
   log watch that runs for minutes.
2. **Routes local control connections.** A client connects to
   `<envoy-data-dir>/bridge.sock` and writes

   ```
   [u32 BE length][device name or device id]
   ```

   The bridge resolves the target against `EngineHandle::device_set()` (exact
   id first, then a case-insensitive unique name match), opens a stream with
   `EngineHandle::open(device_id, 16)`, and splices both directions.

3. **Resolves machines by identity.** Names come from `DeviceCert.name`, which
   is signed by the account; ids are derived from device public keys. envoy's
   allowlist therefore matches on a value the peer cannot claim.

## Failure reporting

Resolution and dial errors are written back as an agent-protocol `Reply::Denied`
frame, so `envoy --on <device>` prints the reason ("not an active device",
"peer offline", ambiguous name) instead of failing the handshake with an EOF.

## Running

```bash
# one-time, in the conduit project
conduit init --coord-url https://HOST:7722 --pin <PIN>
conduit account create ...

cd bridge
cargo run --release
# envoy-bridge listening on ~/.envoy/bridge.sock
# this device: workstation (3fa9c1d2...)
# peer: laptop (91ff0e2c...)
```

On the target machine, allow the caller:

```bash
envoy enable
envoy allow workstation      # or the device id
```

Then from anywhere in the account:

```bash
envoy --on laptop run "uname -a"
envoy --on laptop submit "cargo build --release"
envoy --on laptop logs 12 --follow
```

## Options

| Flag | Meaning |
|---|---|
| `--data-dir` | envoy data directory (default `~/.envoy`) |
| `--socket` | control socket (default `<data-dir>/bridge.sock`) |
| `--conduit-data-dir` | conduit store (default `~/.conduit`) |
| `--force-relay` | never try a direct path |
| `--no-mdns` | disable LAN discovery |

Logging: `RUST_LOG=envoy_bridge=debug,conduit_fabric=info`.
