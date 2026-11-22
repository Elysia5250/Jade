# Plan: Harness extensibility (P0 + P1)

- Status: complete
- Owner: Jade
- Last updated: 2026-11-22

## Goal

Upgrade Jade's harness extensibility across P0 and P1 without breaking wire
compatibility (`AGENT_PROTOCOL_VERSION`), file-size caps, or security
invariants (`Managed > User > Project`, workspace trust gate, project sandbox):

- **P0**: Daemon-owned persistent `McpPool`, Managed `/etc/jade/mcp.json`
  (fail-closed), `notifications/tools/list_changed` live refresh, MCP
  `resources` + `prompts`, and deferred `mcp_search` above 24 tools.
- **P1-A**: Narrow `Tool` + `ToolRegistry`, narrow `ModelProvider` +
  `ProviderRegistry`, declarative `extra_headers` with `${SESSION_ID}` and
  CRLF sanitization, and multi-endpoint `jade auth add`.
- **P1-B**: Declarative `plugin.json` bundle discovery/lifecycle (`Managed >
  User > Project`, plugins never auto-apply `allow` rules) wired across all 6
  subsystems (`skills`, `agents`, `commands`, `hooks`, `mcp`, `permissions`),
  plus custom `/command` markdown templates in TUI, headless, and plugins.

## Progress

- [x] P0: `McpScope::Managed`, config fingerprint, `McpPool`, `resources`/`prompts`/`list_changed`, deferred `mcp_search`, `jade mcp resources|prompts`
- [x] P1-A: `Tool`/`ToolRegistry`, `ModelProvider`/`ProviderRegistry`, `StoredProvider::extra_headers`, `JADE_EXTRA_HEADERS`, `jade auth add`
- [x] P1-B: `plugins/` (`plugin.json`, `plugins.json`, local install/remove), `commands/` (`commands/*.md` + `$ARGUMENTS`/`$1..$9` + `<plugin>:<cmd>`), 6-subsystem wiring, `jade plugin`, TUI `/plugin` + custom `/command` autocomplete, headless expansion
- [x] Docs & verification (`docs/architecture.md`, `cargo test --workspace`, `cargo check --workspace --all-targets`, `scripts/check-file-sizes.py`)

## Decisions

- Kept `ModelRoute` unchanged so `AgentResp::ModelsRefreshed` stays postcard
  wire-compatible without bumping `AGENT_PROTOCOL_VERSION`; `extra_headers`
  lives on JSON-persisted `StoredProvider`.
- Plugin `permissions.json` auto-applies `deny` and `ask` rules only; `allow`
  rules are surfaced via `jade plugin inspect` / `/plugin inspect` for explicit
  user approval.
- Built-in slash commands always take precedence and can never be shadowed by
  custom `/command` files.

## Verification

```bash
cargo check --workspace --all-targets
cargo test --workspace
python3 scripts/check-file-sizes.py
```
