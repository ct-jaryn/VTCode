# Copilot runtime ownership

The binary keeps one Copilot runtime host and one prompt-stream adapter. Both
live under `agent/runloop/unified/turn/turn_processing/llm_request/`.

| Owner | State and responsibility | Lifecycle boundary |
| --- | --- | --- |
| `copilot_runtime.rs`: `CopilotRuntimeHost` | Borrowed registry, UI/session state, permission caches, safety validator, hooks, harness budgets, and runtime request dispatch | The request renderer borrows the host; host drop aborts its remaining local terminal sessions |
| Host local terminal sessions | Terminal tasks, output/exit snapshots, observed-tool association, and terminal release/kill/wait operations | Explicit release or host drop aborts remaining work; completion publishes terminal exit state |
| Host observed tool calls | Started/finished flags, previous output, and inline PTY presentation | Observed status updates control output deltas and final presentation |
| `copilot_runtime/streaming.rs` | Prompt text/reasoning accumulation, finish-reason conversion, queued-update draining, and prompt cancellation guard | Polling installs the guard; dropping an active stream cancels the prompt; successful completion disarms it before emitting `Completed` |
| `llm_request/mod.rs` | Starts the prompt session, retains the runtime request receiver, and runs the shared streaming renderer | Startup interruption/first-progress timeout and renderer success/error remain request-owned |

The existing `prompt_session_to_stream` entrypoint delegates synchronously to the
private adapter, retaining its return shape and caller path. Runtime requests
remain on their separate receiver. The adapter does not acquire permission,
execute tools, or own terminal tasks.

The stream loop disables its update receive arm after channel closure. On prompt
completion, it drains queued updates before constructing the final response.
Reasoning normalization retains paragraph breaks, Unicode characters, closing
punctuation, and the spacing rules between chunks. Join/provider errors retain
the shared runtime error mapping.

Permission handling, exposed-tool allowlists, tool budgets, hook rewrites,
sandbox-aware execution, verification guards, terminal state, and harness events
remain in the host. Splitting these areas requires tracing their shared state
and lifecycle exits first; do not create another runtime host or execution path.

Run binary runtime regressions and the existing provider cancel-handle regression:

```sh
cargo nextest run --locked -p vtcode -p vtcode-llm -E 'test(copilot_runtime) | test(prompt_session_cancel_handle) | test(inline_events)'
```

The adapter's unit tests cover conversion boundaries. They do not construct a
live `PromptSession` or exercise stream-drop/completion races end to end. The
provider regression verifies cancel notification, active-prompt cleanup, and
completion-task abortion separately. A live Copilot CLI/provider session remains
outside this refactor's validation.
