# Runtime Guidance and Project Instructions

VT Code has two distinct prompt sources:

| Source | Loaded from | Purpose | Trust boundary |
| --- | --- | --- | --- |
| Compiled runtime guidance | `crates/codegen/vtcode-core/src/prompts/runtime_guidance.rs` | Small, universal user-facing behavior included in Default, Minimal, Lightweight, and Specialized profiles | Part of the application runtime |
| Project instruction map | User/workspace `AGENTS.md`, `CLAUDE.md`, and `.vtcode/rules/` | Project conventions, local architecture, and maintainer workflows | User-controlled context, never a security boundary |

The compiled section is deterministic, cached with the static profile, and
kept below its approximate 420-token cap. It must not read, embed, or generate
content from repository instruction files. Profile-specific operating details
remain in the prompt builder; correctness-critical behavior belongs in runtime
policy, schemas, tests, or lints.

## User-facing progress contract

The compiled guidance tells the model that its text between tool calls is
what the user reads. It says in one sentence what it will do before starting,
updates only on findings, direction changes, or blockers, and finishes with the
outcome first, then what changed, what it checked, and anything the user must
do. Structured tool-call events remain the authoritative status signal. These
are user-facing updates, not a transcript of every call.

Compact transcript mode may collapse successful command bodies while retaining
complete output in Transcript Review. The model must not rerun commands merely
to reveal hidden output; material findings belong in a visible progress update
or the final reply. The provider-neutral collapsed-output disclosure reinforces
this after each affected tool result.

## Continuity and long-running work

### Runtime observability and cancellation

`AgentRuntime` keeps `ThreadEvent` authoritative while tracing bounded
diagnostics for model latency, turn latency, input/output token usage, output
and reasoning byte counts, tool-call count, finish reason, cancellation, and
timeouts. These fields are diagnostic only and must not be persisted as a
parallel lifecycle schema. A provider error or timeout fails the turn; an
interrupted stream is never reported as successful completion. Open tool calls
are closed with a terminal failed status, while partial streamed text remains
partial and is not promoted to a successful final answer. Follow-up steering
received during streaming is applied to the live history at the next turn-loop
iteration boundary, or retained for the next turn when the turn ends first.

Request segment fingerprints include the provider route, model, context capacity,
effective reasoning tag, tool/parallel/cache capabilities, and current tool catalog
epoch. Repeated requests reuse immutable prompt and ordered tool bytes; capability
changes invalidate the segment identity. Shell profile, environment, and harness
limits belong to the frozen segment prefix. Provider cache routing keys include
the fingerprint where the transport supports explicit keys. Local fingerprint
reuse is not proof of a provider cache hit: use returned usage cache-hit metrics
to measure that separately for each provider.

Tool documentation density is resolved separately from tool authorization. Both
request paths use Minimal guidance at 32,000 context tokens or below, or when the
Default prompt exceeds the configured system-prompt token or monetary budget;
otherwise they retain Default guidance. Missing pricing does not imply cheap
execution. Parallel-call hints require the active provider's parallel-tool
capability. Both profiles preserve inspection, verification, and terminal-owned
WebMCP permission guidance.

The runtime keeps prompt additions small and cache-stable while preserving the
newest working context. Automatic compaction applies the shared configured
trigger ratio (90% by default) to the effective provider/session budget and uses a continuity
tail target of approximately 20,000 estimated tokens. It retains complete
user/assistant/tool protocol groups verbatim, removes an incomplete trailing tool
call, and summarizes only the older prefix. Unless an explicit harness threshold
is configured, the effective hard threshold is the resolved model capacity,
bounded by the provider route and a positive `context.max_context_tokens` safety
ceiling, minus the next request's output reservation. The default safety ceiling
is zero (automatic). Known request output limits take precedence; otherwise
4,096 tokens are reserved. Explicit thresholds may lower this boundary but
cannot bypass it. Prompt and tool overhead count toward pressure; per-turn
tracing records the context denominator. A derived soft boundary marks
compaction pending for the next outer turn boundary; the effective prompt
threshold compacts before the next model request. Provider-native compaction results are normalized
through the same tail rules, with local fallback when the provider does not
return a usable tail.

Long-running command sessions have an explicit `wait` action. A wait deadline
returns a bounded in-progress result without killing the process, so the model
does not need to spend repeated turns issuing 30-second polls. Full command
output is written to the tool-output spool; responses expose only a bounded
preview and its spool metadata. A spool reference is emitted only after its
file is open and has not reported a write failure. Completed references include
`spool_state`, the exact byte count, and a SHA-256 digest and are reopened only
after descriptor-relative containment, regular-file identity, length, and digest
validation. Pending live references permit only bounded, explicitly unverified
reads. Exited
sessions with an unfinished spool retain the session and defer the reference
until a later wait can safely observe the complete file.

Background execution is owned by `ExecSessionManager`, the single registry for
pipe and PTY sessions. `exec_command` accepts `background: true`, and `Ctrl+B`
promotes the active foreground session into the same registry without replacing
its `session_id`. The launch response keeps the bounded preview and provides
the lifecycle state, child PID when available, and reusable wait/continuation
arguments. At most three live background processes are reserved per VT Code
runtime; the fourth launch fails before spawning and existing sessions are not
evicted. Exited background metadata remains inspectable until explicit close,
while runtime shutdown closes all process groups and descendants. The runtime
continues to emit the existing `ThreadEvent` item lifecycle events rather than
introducing a parallel background-process event contract.

Managed background subprocesses publish a terminal completion notification after
their `BackgroundRecord` has been persisted as `Stopped` or `Error`; user-launched
background exec sessions publish the same terminal signal directly from the
shared exec-session watcher. Before publishing, the watcher makes a bounded
attempt to drain and retain final output, and pruning retains an exited
background session until that delivery finishes; the run loop then refreshes
Local Agents immediately. The local agent state and transcript can therefore show
terminal results without a `/subprocesses refresh` or explicit
`write_stdin` poll. Clean exits are `Stopped`, spontaneous non-zero exits are
`Error`, and user-requested managed or raw exec-session stops remain `Stopped`.
When the main interaction loop is idle, it appends one
bounded authoritative completion note and schedules at most one follow-up
reasoning turn. A completion that arrives during an active model/tool turn is
deferred to the next safe boundary, and newer user input always takes priority;
direct user commands do not fabricate an unrelated autonomous turn. The
explicit `wait` action remains available when a caller needs a synchronous
observation. The canonical completion record is emitted as
`background_subprocess_completed` in event schema 0.16.0.

Cross-turn resume hint body is transient, not universal guidance: when a turn ends with a
live foreground session, the next turn start injects a bounded `Exec session resume:` hint
via `append_transient_turn_notes` (same path for normal next-turn and session
restore/resume). The hint carries at most 4 session lines (160 bytes per
command, <1 KiB single-session) plus a pre-filled `write_stdin` wait, and the
runtime never auto-executes the wait. Turn-end `turn.completed` (schema 0.16.0)
and `SnapshotTurnDiagnostics` both record `in_progress_exec_sessions` (bounded
to 4) for ATIF correlation, including retained background sessions that remain
live for asynchronous work. This hint body stays out of `runtime_guidance.rs` so the
universal section is not taxed on turns with no live session; per-tool
`guidelines.rs` `write_stdin` guidance carries only a one-line pointer that the
hint may appear.

The provider-facing history also has an aggregate tool-preview budget per
turn (32 KiB execution, 96 KiB planning). After exhaustion, new payload bodies are replaced by bounded metadata,
but scalar control signals such as success, exit code, completion status,
verification requirements, and retryability remain visible. The metadata tells
the agent not to repeat equivalent calls merely to recover hidden output, and
checkpoint diagnostics record how many previews were suppressed.
Diagnostics also report requested, admitted, and derived unadmitted tool-call
counts so budget or policy rejections cannot disappear from turn accounting.
Read-only results reused by same-turn caches, cross-turn target caches, or
bounded history replay all increment the same reuse counter. Request assembly
also collapses legacy duplicate output-disclosure notices to one current marker.

Interactive follow-ups are durable steering intents. Each queued intent has a
UUID, the session envelope stores at most 16 pending intents and a 64-ID applied
window, and the intent is acknowledged only after its tagged user message is
durably checkpointed. Recovery compares IDs in the envelope with tagged history,
not just instruction text, so duplicate text remains meaningful. Delivery is
mid-turn: the tagged user message is appended to live history at the next
turn-loop iteration boundary, right after the current tool-call batch.

Even when `.vtcode/prompts/system.md` replaces the static base prompt, the
compiled section is reattached after prompt layers are resolved. This keeps the
universal baseline present without treating workspace prompt content as a
security boundary.

The dynamic instruction pipeline remains enabled by default. It discovers user
and workspace sources in precedence order, loads nested files for the active
directory, applies path-scoped rules and exclusions, and appends the resulting
project appendix separately from the compiled base prompt. `AGENTS.md` files
therefore remain useful maintainer maps without becoming an implicit source of
universal VT Code behavior.

When changing this boundary, run:

```bash
cargo nextest run -p vtcode-core
cargo check --locked
./scripts/check-dev.sh --changed
```

Release archives are independently allowlisted to contain the binary, man
page, and shell completions only. They must never include `AGENTS.md` or other
workspace guidance.
