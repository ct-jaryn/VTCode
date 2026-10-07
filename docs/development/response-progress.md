# Response progress and latency

The inline and fullscreen interfaces show one temporary progress row as soon as a request is accepted. The row
follows actual work: initialization, context preparation, checkpoint saving, model wait, processing, response reception,
retry backoff, permission checks, approval waits, and admitted tool execution. Individual tool rows and queued prompts
keep their existing presentation. The footer uses the same phase label and elapsed seconds.

`ProgressOperation`, `ProgressPhase`, and `ProgressUpdate` live in `vtcode-commons::ui_protocol`. The UI accepts a new
operation identity, updates only that active identity, and clears only the matching operation. Finished identities
remain superseded so delayed updates cannot resurrect an old row. `InlineHandle::begin_progress` returns an owning
guard; `transfer` and `resume_progress` retain the acceptance clock when the interaction loop hands off a model turn.
Guard drop clears progress on completion, preparation failure, provider failure, cancellation, and session handoff.
The UI deduplicates phase updates against the displayed operation. Copilot runtime requests can temporarily show
tool execution or approval waits; when they settle, the request caller restores model progress before more text.
Keyboard and paste handlers only emit input events. They do not create provisional operations: a submission can
be consumed by a local command, overlay, or focused process before the runtime accepts a request.

Progress is presentation state. It does not grant permissions, set `ActivityState`, change input ownership, or enter
conversation history, exports, canonical events, or provider requests. The transcript widget reserves one row outside
the scroll, link, and selection body. Modals and other overlays clip it through the existing render order. When progress
consumes the entire transcript allocation, transcript selection uses an empty area; overlay selections retain their
own viewport. Elapsed seconds update on shared ticks; approval waits stay static. Animated phases reuse `ShimmerState`
and `tui-shimmer`
with the existing 33 ms frame interval and two-second sweep. Reduced motion and screen-reader policy retain static
labels. Animation requests a redraw without invalidating transcript reflow.

## Measurements

Enable the existing debug diagnostics for `vtcode.response_latency`. Measurements use `Instant` and log operation,
step, and attempt identifiers with durations; they never include prompts, credentials, response bodies, or provider
error strings. Relevant observations are:

| Observation | Meaning |
| --- | --- |
| `accepted_to_feedback_ms` | Acceptance to the frame painting the transient row |
| `checkpoint_ms` | Awaited checkpoint preparation, including durable publication and retention |
| `preparation_ms` | Outer turn preparation before the execution clock starts |
| `accepted_to_prepared_ms` | Includes earlier interaction-loop preparation |
| `request_assembly_ms` | Provider request construction |
| `dispatch_to_first_event_ms` | Provider dispatch to the first observed provider activity |
| `dispatch_to_visible_output_ms` | Provider dispatch to the first nonempty visible text emission |
| `accepted_to_visible_output_ms` | Includes preparation before the execution clock |

Completion-only streaming responses emit a visible-output observation after sanitized fallback rendering. Already
streamed text does not emit a duplicate completion observation, and suppressed or blank output does not count.
Hidden reasoning is provider activity, not visible text. Non-streaming requests record response availability as their
first event; visible-text emission diagnostics currently cover streaming requests. Terminal paint timing can be
measured separately with a PTY fixture. These observations do not alter execution budgets, retry limits, or timeout
clocks. Compare rebuilt release binaries in three paired runs and report provider wait separately.

## Checkpoint preparation

The prompt worker acquires the existing exclusive rewind lock, captures content, serializes JSON, and durably
publishes the checkpoint and navigation record on the blocking pool. Its lease remains held until the turn settles.
Independent dirty-worktree inspection starts before checkpoint preparation and is awaited before request dispatch.
If checkpointing fails, drain that worker and retain the existing prompt restoration path.

Hot retention discovers retired records before parsing live checkpoint JSON. With no retired records, it skips live
record parsing, content-store opening, and garbage collection. Reclamation still checks every live reference before
deleting retired sessions, and corrupt metadata defers cleanup. Explicit full maintenance also collects old orphaned
content when no retired records exist. Existing resource, catalog, connection, and request-prefix caches retain their
current ownership and invalidation rules.
