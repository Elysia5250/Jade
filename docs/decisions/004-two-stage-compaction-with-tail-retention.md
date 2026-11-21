# 004: Two-stage context compaction with retained recent tail

- Status: accepted
- Date: 2026-11-19

## Context

Long coding sessions accumulate large tool outputs (`read`, `grep`, `exec`) that quickly saturate model context windows. Naive truncation either drops recent working context, breaks `assistant(tool_calls)` / `tool` message pairing (causing provider 400 errors), or destroys prompt-cache prefixes on every turn.

## Decision

Context compaction (`jade-agent::agent::context`) runs in two stages governed by vendor-native harness budgets (`/budget`):
1. **Stage 1 (Prune):** Fold older tool outputs into compact placeholders while leaving recent tail turns completely untouched.
2. **Stage 2 (Summary):** When still over budget, summarize the older conversation head across **atomic `assistant(tool_calls) + tool` boundaries**, keeping a recent tail verbatim and preserving the stable system-prompt prefix for prompt-cache hits.
3. **Overflow fallback:** If a provider still rejects a prompt as too long, the loop calibrates the model's recorded cap (`model_caps.json`), runs one automatic compaction pass, and retries once.
4. **Visual continuity:** Compaction updates model history (`Compacted` event) while keeping the user's visual transcript intact; `/handoff` provides an explicit clean-slate session reset.

## Consequences

- **Easier:** Long sessions degrade gracefully without breaking tool-call invariants or thrashing prompt caches; misreported provider limits self-correct on first overflow.
- **Harder:** Summary boundaries must be tested carefully so orphaned `tool` messages never reach a provider, and token estimation must be calibrated per turn.
- **Explicitly not done:** No infinite overflow retry loops (capped at 1 retry); no silent deletion of the user's visible TUI transcript.
