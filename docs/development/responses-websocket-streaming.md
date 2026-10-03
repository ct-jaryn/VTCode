# Responses WebSocket streaming

`provider.openai.websocket_mode = true` enables persistent Responses WebSocket transport for generation and
both normalized and legacy streaming APIs on eligible native OpenAI and compatible Responses endpoints.
The option defaults to `false`; unsupported backends retain HTTP transport. WebSocket events always use
`store = false`.

## Lifecycle and cancellation

One response lane serializes generation and streaming on the connection. A lease removes the socket and
continuation cache from their reusable slots before startup. A returned stream owns that lease and the shared
Responses processor; it needs no provider borrow or detached background task.

The first request warms up with `response.create` and `generate = false`. After the warmup completes, the
generated event chains from its response ID. Streaming startup reads lifecycle events until the first
deliverable text, reasoning, tool-call event, or validated completion. Ping frames receive pong replies;
control frames never become model events.

Only `response.completed` permits successful final assembly and socket restoration. The shared processor
preserves text, refusal, reasoning classification, provider tool-call IDs and arguments, usage, and recovery
from empty final output when usable deltas arrived. Failure, incomplete response, malformed events, close,
EOF, deadline expiry, cancellation, or early stream drop invalidate the socket and its continuation cache.
The lease stays active through final usage delivery until `Done` is delivered; dropping between those events
invalidates the connection even when the server has already completed.
Dropping a future while it waits for the lane leaves the active response alone.

Connections have a 30-second timeout. Each WebSocket request has one deadline from
`timeouts.streaming_ceiling_seconds`, including lane acquisition, connection, warmup, and generation; a zero
setting uses 600 seconds. Receiving lifecycle or control frames does not reset this deadline.

## Continuation

The cache records request input plus completed assistant and tool-call output in the history builder's
canonical replay representation, after applying the model's reasoning policy just as callers do.
History comparisons ignore only `prompt_cache_breakpoint` annotations.
Matching model, instructions, tools, and a complete history prefix permits `previous_response_id` with
only genuinely new input, such as tool results or the next user message. Completed assistant text and
tool calls are not resent.

Changed model, instructions, tools, edited or compacted history, and uncertain output matching start a
full-input chain. Empty authoritative output, opaque output, or phase metadata that cannot be recovered
through the response contract disable continuation for that response. Reconnecting always drops
connection-local response IDs.

This follows the incremental-input and warmup contracts in the official
[OpenAI WebSocket guide](https://developers.openai.com/api/docs/guides/websocket-mode).

## Fallback and service tiers

Before a deliverable event reaches the caller, any WebSocket startup failure clears transport state and
uses HTTP SSE for that request, without another WebSocket attempt. This includes active-response,
connection-limit, and missing-previous-response errors. A later request may reconnect.

After a text, reasoning, or tool-call event has been exposed, a failure emits a stream error without HTTP
replay. The legacy adapter retains its existing event enum and carries tools and usage in the final response.
It applies the model's reasoning policy after final assembly, including recovery from retained deltas.
Because tool deltas are assembled into the legacy final response, failures during tool-only startup still
use HTTP until a legacy event reaches the caller. Normalized tool-start events cross that boundary immediately.

The model-scoped tier rejection cache applies before WebSocket and HTTP requests. Every warmup and
generated event forwards the effective service tier, including `ultrafast`, as required by the official
[Ultrafast guide](https://developers.openai.com/api/docs/guides/ultrafast-mode). Structured WebSocket tier
rejections are classified before diagnostic formatting, cached, and routed to tier-less HTTP. Both HTTP
streaming entrypoints retry a confirmed rejection once without the tier, retaining authentication,
turn metadata, and a fresh client request ID. A failed retry uses the existing error classification and
transport fallback policy: an unsupported Responses endpoint can fall back to Chat Completions when
allowed, while a required Responses route surfaces the error. The tier retry remains bounded to one attempt.

## Local verification

Scripted local servers assert ordered deltas, exact continuation inputs, request counts, tier forwarding,
fallback boundaries, cancellation, serialized generation, and ping/deadline behavior. No paid API calls
are required.

```sh
cargo nextest run --locked -p vtcode-llm -E 'test(websocket) | test(service_tier) | test(responses_stream) | test(responses_adapter) | test(stream_decoder)'
cargo nextest run --locked -p vtcode-config -E 'test(openai_config)'
cargo check --locked -p vtcode-llm -p vtcode-config
cargo clippy --locked -p vtcode-llm -p vtcode-config --all-targets -- -D warnings
cargo fmt --all -- --check
./scripts/check-dev.sh
```
