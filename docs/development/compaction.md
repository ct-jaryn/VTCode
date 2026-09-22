# Compaction Engine

How VT Code keeps long sessions inside the model window. See
`crates/codegen/vtcode-core/src/compaction/mod.rs` for the implementation.

## Strategy dispatch

`/compact` picks one strategy per provider/model
(`manual_compaction_strategy`):

| Provider / model                              | Strategy           | How it works                                                                                        |
| --------------------------------------------- | ------------------ | --------------------------------------------------------------------------------------------------- |
| OpenAI Responses models (`/responses/compact`) | `NativeStandalone` | Server-side standalone endpoint; returns a `response.compaction` item plus retained items, used as-is. |
| OpenResponses models (`/v1/responses/compact`)  | `NativeStandalone` | Vendor-neutral standalone endpoint; retains returned messages and opaque `compaction` items for replay. |
| Vercel AI Gateway `openai/*` routes            | `NativeStandalone` | Gateway serves `POST /v1/responses/compact` (forwarded to OpenAI). Gated on `openai/` model prefix and the gateway endpoint; other routes use `Local`. |
| xAI Grok models (`POST /v1/responses/compact`) | `NativeStandalone` | xAI serves an OpenAI-compatible compact endpoint for curated Grok models on `api.x.ai`. Gated on the model allowlist and API host; otherwise `Local`. |
| Anthropic Claude Opus 4.x / Sonnet 4.6+        | `NativeInline`     | Threshold-triggered `compact_20260112` edit on a `generate` request with `pause_after_compaction`. Falls back to `Local` when compaction does not fire or the edit is rejected. |
| Merge Gateway, OpenRouter, Gemini, DeepSeek, MiniMax, local, all others | `Local` | No native compact endpoint: Merge Gateway documents no `/responses/compact` (its native `/v1/responses` shape differs from OpenAI's); OpenRouter reports compaction unsupported (its `context-compression` plugin is opt-in middle-out truncation, never enabled implicitly). Universal fallback: bounded fork summarized via the provider's one-shot response collector (which transparently consumes a normalized stream for streaming-only routes), rebuilt as summary plus retained recent messages. Works everywhere. |

## Anthropic context-edit ladder

On the `NativeInline` path VT Code sends the documented edit ladder in order —
`clear_thinking_20251015` (keep 2 thinking turns), then
`clear_tool_uses_20250919` (trigger 100k input tokens, keep 3 tool uses),
then `compact_20260112` (trigger floor 50k, `pause_after_compaction: true`).
The clearing edits are included only when the provider reports
`supports_context_edits`; otherwise the request carries the compact edit alone.
Beta headers (`compact-2026-01-12`, `context-management-2025-06-27`) are derived
from the request payload by the Anthropic provider, so callers never set them
manually. Helper: `anthropic_inline_compaction_edits`.

The response's `compaction` block is provider state, not a replacement for a
local `Previous conversation summary` message. VT Code keeps the complete raw
block (including signatures, cache controls, and unknown provider fields) in
the assistant message's `reasoning_details` and replays it as the first
Anthropic message on the next request. Signed blocks are required to remain
first, and thinking/redacted-thinking blocks from before that signed boundary
are dropped because Anthropic's preserved-thinking validation cannot accept
them after the summarized transcript has been removed. A threshold response
whose block has `content: null` is treated as a failed native compaction and
uses the local fallback.

Anthropic usage with compaction may include separate `iterations` for the
compaction and message sampling passes. VT Code aggregates recognized
`message`, `fallback_message`, `advisor_message`, and `compaction` iterations for normalized
usage and cost estimates; top-level usage is used only when no recognized
iteration data is present.

Anthropic's newer on-demand `compact-2026-09-04` signed-summary beta is not
exposed by the current `/compact` engine. The engine is synchronous and uses
the threshold/pause strategy; adding background keep-tail swapping requires a
separate history lifecycle and beta capability contract.

## Input bounding and overflow retry

Local and inline strategies bound their input with `bound_history_for_summarization`;
standalone native compaction uses the marker-preserving
`bound_history_for_native_compaction`. Both apply `compaction_history_budget`
(window minus overhead), keeping the newest complete protocol groups and
degrading to previews when no group fits. Before bounding,
local paths run `prune_oversized_tool_outputs`: `Tool` messages over
4,096 tokens are replaced with previews (call IDs preserved, so groups stay
protocol-valid) so one dump cannot evict whole turns from the fork. Flat
summaries and hierarchical bands share one capacity-retry contract
(`generate_summary_with_capacity_retry`): on a context-capacity rejection the
request is retried with progressively halved budgets, skipped when a retry
cannot shrink the input. When the window is unknown (`None` budget) the first
fork is verbatim, but the retry derives a fallback budget from the failing
attempt (`input_tokens / 2`) so it can still shrink instead of resending the
identical fork. Empty provider summaries fail with a diagnostic
(`provider returned an empty summary`) rather than producing an empty
`Previous conversation summary:` history. All summary failures carry route
diagnostics (`provider / model`, `input_tokens`, `budget`) in the error chain
so `/compact` reports the actionable cause instead of only
`Failed to generate compaction summary`. Local summaries are bounded on output with
`bound_compacted_history_to_context`; native provider responses are passed
through unchanged because their retained items and opaque compaction state are
the canonical next context window. The native endpoint is responsible for
returning a replayable window that fits its model context.

## Per-route policy

`CompactionRoutePolicy` (`threshold_ratio`, `retain_ratio`,
`max_overflow_retries`) tunes compaction per provider/model route in the
DeepSeek-harness shape. Defaults (`1.0`, `0.16`, `1`) preserve current
behavior: the threshold ratio only ever fires compaction *earlier*, the
continuity tail is `min(20_000, window × retain_ratio)` (identical for windows
≥ 125k, scaled down for small windows so the summary survives output
bounding), and one halved retry happens on capacity errors. Route overrides
live in the `ROUTE_POLICIES` table matched on provider/model substrings;
it is intentionally empty until a route demonstrates a need.

## Verification

```bash
cargo nextest run -p vtcode-core --lib compaction
./scripts/check-dev.sh
```
