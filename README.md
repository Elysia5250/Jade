# Envoy (`envoy/`)

Standalone local execution, policy gate, wire server/client, and stdio MCP workspace (zero network engine or file-sync dependencies).

## Crates

- `envoy-proto`: Wire protocol (`AgentOp`, `AgentCall`, `AgentMsg`, `AgentEvent`, `Entry`) and length-prefixed postcard framing (`write_framed`, `read_framed`).
- `envoy-core`: `Runner` (process-group exec with timeout/cancel, range read, atomic `.part` write, directory listing), `AgentConfigStore` (`agent.json`), `audit.log`, and `JobLog` pruning.
- `envoy-service`: Transport-agnostic `AgentServer::serve_stream` and client helpers (`open_call`, `probe`, `pump`, `ReadStream`, `WriteStream`, `read`, `write`, `list`, `AgentClient`), plus boundary traits `AgentDialer`, `AgentStream`, and `AgentHost`.
- `envoy-cli` (`envoy`): Standalone CLI (`status`, `enable`, `disable`, `allow`, `deny`, `run`, `read`, `write`, `ls`, `copy`) and stdio MCP server (`mcp serve`).

## Boundary

When ready to connect or redesign remote transport on top of `conduit`, adapters implement `envoy_service::AgentDialer` / `envoy_service::AgentHost` and pipe any `AsyncRead + AsyncWrite + Unpin + Send + 'static` stream into `AgentServer::serve_stream`. See [ARCHITECTURE.md](./ARCHITECTURE.md) for the full engine integration contract.

## Test

```bash
cargo test --workspace
```
