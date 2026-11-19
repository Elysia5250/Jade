# 002: Postcard on the local agent socket

- Status: accepted
- Date: 2026-11-19

## Context

The console, headless CLI, and daemon exchange high-frequency streaming events (`TextDelta`, `ReasoningDelta`, tool outputs, snapshots) and RPCs (`Op`/`Reply`) over local Unix sockets (`agent.sock`, `envoy.sock`) and conduit WebSocket streams. We need compact, low-overhead framing without pulling in a heavy schema compiler or codegen pipeline.

## Decision

Wire messages on `envoy.sock` (`envoy-proto`) and `agent.sock` (`envoy-agent::runtime::protocol`) use length-prefixed `postcard` framing via `serde`.

Because `postcard` is **not** self-describing:
- Wire structs must **never** use `#[serde(skip_serializing_if = ...)]` or field-skipping attributes that change wire layout conditionally.
- Every new wire event or RPC variant requires a postcard round-trip test and a replay/sequence-number story.
- Incompatible protocol changes bump the protocol version in the stream handshake (no in-band version negotiation).

## Consequences

- **Easier:** Tiny payloads, zero codegen build steps, fast serialization, and shared Rust types across `envoy-proto`, `envoy-service`, `envoy-agent`, and `envoy-cli`.
- **Harder:** Wire compatibility is strictly positional; carelessly adding `skip_serializing_if` or reordering enum variants silently corrupts decoding unless caught by round-trip tests.
- **Explicitly not done:** No JSON-RPC on internal daemon sockets (JSON-RPC is confined to external MCP stdio/HTTP boundaries); no protobuf/flatbuffers codegen.
