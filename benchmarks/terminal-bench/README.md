# Jade Headless & Terminal-Bench 2.0 Integration

`jade` supports non-interactive execution for CI pipelines and containerized benchmarks (Harbor / Terminal-Bench 2.0) while preserving interactive TUI defaults.

## Headless Flags

- `-p, --print <PROMPT>` — Run a single turn non-interactively (pass `-p -` to read prompt from `stdin`).
- `--model <MODEL>` (`JADE_MODEL`) — Override provider/model (`openai/gpt-5.4`, `anthropic/claude-sonnet-4-5`, or bare model ID).
- `--permission-mode <default|accept-edits|plan|auto>` — Set permission baseline.
- `--dangerously-skip-permissions` — Bypass interactive `Ask` prompts (still enforces `Managed`/`User`/`Project` `Deny` rules) and default sandbox to `danger-full-access`.
- `--sandbox <workspace-write|read-only|off|danger-full-access>` — Override tool OS sandbox inside containers lacking `bwrap`/user-namespaces.
- `--max-steps <N>` — Raise step ceiling up to `250` (default `100` in `-p` mode; `25` in TUI).
- `--output-format <text|json|stream-json>` — Stream plain text, emit a single JSON summary object, or stream NDJSON `SeqEvent` records.

## Environment-Only Provider Auth

Zero `~/.jade/auth.json` setup is required inside containers:

- Builtin providers: `OPENAI_API_KEY` (+ `OPENAI_BASE_URL`), `ANTHROPIC_API_KEY` (+ `ANTHROPIC_BASE_URL`), `DEEPSEEK_API_KEY` (+ `DEEPSEEK_BASE_URL`), `GEMINI_API_KEY` (+ `GEMINI_BASE_URL`), `XAI_API_KEY` (+ `XAI_BASE_URL`), `DASHSCOPE_API_KEY` (+ `DASHSCOPE_BASE_URL`), `ZHIPU_API_KEY` (+ `ZHIPU_BASE_URL`), `MOONSHOT_API_KEY` (+ `MOONSHOT_BASE_URL`), `MINIMAX_API_KEY` (+ `MINIMAX_BASE_URL`), `ARK_API_KEY` (+ `ARK_BASE_URL`), `MISTRAL_API_KEY` (+ `MISTRAL_BASE_URL`), `GROQ_API_KEY` (+ `GROQ_BASE_URL`), `OPENROUTER_API_KEY` (+ `OPENROUTER_BASE_URL`), `SILICONFLOW_API_KEY` (+ `SILICONFLOW_BASE_URL`), `AIHUBMIX_API_KEY` (+ `AIHUBMIX_BASE_URL`), `AI302_API_KEY` (+ `AI302_BASE_URL`), `OPENCODE_API_KEY`, `OPENCODE_GO_API_KEY`.
- Custom OpenAI/Anthropic relay: `JADE_API_KEY` + `JADE_BASE_URL` + `JADE_PROTOCOL` (`openai` | `anthropic`) + `JADE_MODEL`.

## Building & Running in Containers (Podman / Docker)

Because host Linux builds link against the host `glibc` (for example Fedora's newer `glibc` will fail inside older Ubuntu 22.04 / Debian 12 benchmark containers), build a static `musl` binary inside `rust:alpine`:

```bash
podman run --rm -v "$PWD:/src:Z" -w /src docker.io/library/rust:alpine \
  sh -c 'apk add --no-cache musl-dev && cargo build --release --target x86_64-unknown-linux-musl -p jade-cli'
```

Verify it is statically linked (`static-pie linked`):

```bash
file target/x86_64-unknown-linux-musl/release/jade
```

Smoke-test inside a plain container without Harbor:

```bash
podman run --rm \
  -v "$PWD/target/x86_64-unknown-linux-musl/release/jade:/usr/local/bin/jade:ro,Z" \
  -e OPENAI_API_KEY \
  -e OPENAI_BASE_URL \
  docker.io/library/ubuntu:22.04 \
  /usr/local/bin/jade \
    --data-dir /tmp/jade-data \
    --cwd /tmp \
    --model openai/gpt-5.4 \
    --dangerously-skip-permissions \
    --sandbox off \
    --output-format json \
    -p "Print hello-podman and run uname -a"
```

Use [jade_agent.py](jade_agent.py) with Harbor (`-e podman` requires `podman-compose` and an active `podman.socket`):

```bash
systemctl --user enable --now podman.socket

PYTHONPATH=$PWD/benchmarks/terminal-bench \
harbor run \
  -d terminal-bench@2.0 \
  --agent-import-path jade_agent:JadeHarborAgent \
  -e podman \
  -m openai/gpt-5.4 \
  -l 1 \
  -n 1
```

