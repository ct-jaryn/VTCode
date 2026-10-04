# Large-file and reusable-component refactor plan

Scan date: 2026-10-04. Checkout: `4513b8883`, plus existing owner changes.
Scan scope: tracked Rust and application/script source; analysis and planning.
Implementation progress is recorded below. `docs/project/TODO.md` was not read or modified.

## Inventory and interpretation

- 2,392 tracked Rust files, 829,793 physical lines, including tests and vendored patches.
- 175 Rust files have at least 1,000 lines after excluding `patches/`.
- Crate totals including tests: core 264,492; binary `src/` 212,299;
  LLM 85,201; UI 82,034; config 34,345; commons 21,555.
- The accompanying [CSV inventory](refactor-scan-2026-10-04.csv) lists every
  tracked Rust file with physical line count and a path-based classification.
  Inline tests remain included in source-file counts.
- Size is a review signal, not defect evidence. No complexity, performance,
  or security defect is established by these counts.
- Largest files include dedicated test suites: turn-loop tests (3,726),
  OpenAI provider tests (3,660), subagent tests (3,337), and tool-outcome
  helper tests (2,912). Split these by behavior only when navigation warrants it;
  do not prioritize them as production monoliths.

The duplication probe compared trimmed 18-line windows with a minimum text
length. Matches were inspected manually: repeated imports and test fixtures
were excluded from production recommendations. This is a candidate scan,
not an exhaustive semantic clone analysis or compiler reachability analysis.

## Production responsibility hotspots

Paths below are relative to the repository; line counts include inline tests.

| Priority | File | Lines | Proposed responsibility boundary | Risk |
| --- | --- | ---: | --- | --- |
| P1 | `crates/codegen/vtcode-llm/src/providers/shared/mod.rs` | 1,832 | Cache lineage/usage, function-output conversion, compacted history, stream aggregation, UTF-8/SSE framing | Medium |
| P1 | `crates/codegen/vtcode-llm/src/providers/common.rs` | 1,766 | HTTP/error-body helpers, chat wire mapping, reasoning replay, token counting, cache policy | Medium |
| P1 | `crates/codegen/vtcode-core/src/tools/handlers/planning_workflow/artifacts.rs` | 2,071 | Embedded tracker codec, plan section/step parsing, verification classification, validation/reporting | Medium |
| P1 | `extensions/vscode-extension/src/extension.ts` | 3,534 | Activation wiring, quick-action views, config/settings commands, process/terminal launch | Medium |
| P1 | `apps/webmcp/src/main.ts` | 1,846 | Evidence dialog, settings/connection UI, workspace drafts, review/approval workflow | Medium |
| P2 | `crates/codegen/vtcode-core/src/tools/registry/execution_facade.rs` | 1,984 | Reentrancy guard, admission/safety adapters, execution pipeline, result/error processing | High |
| P2 | `src/agent/runloop/unified/turn/turn_processing/llm_request/copilot_runtime.rs` | 2,139 | Permission adapter, local terminal bridge, observed-tool presentation, stream conversion | High |
| P3 | `src/agent/runloop/unified/turn/session_loop_runner/orchestration/mod.rs` | 2,664 | Session bootstrap, input/turn coordination, turn persistence, ordered shutdown/postamble | High |
| P2 | `scripts/release.sh` | 1,675 | Changelog generation, Homebrew publishing, release orchestration | High |

These are proposed local modules, not new crates or universal utility layers.
Provider helpers remain in `vtcode-llm`; planning stays in core; runtime
coordination stays in the binary; view controllers stay in their apps.
Keep existing facades/re-exports while moving implementations.

## Concrete deduplication opportunities

### 1. OpenCode model normalization and validation

Evidence: `providers/opencode_go.rs:202` and `opencode_zen.rs:228` share
supported-model construction and the clone-only-when-normalization-is-needed
validation branch. They already have `providers/opencode_shared.rs`.

Extend that existing module with a narrow normalized-model validation helper.
Pass provider identity, supported models, and resolved model explicitly.
Keep protocol selection, credentials, constructor contracts, and route-specific
capabilities in each provider. Avoid a new provider trait or macro framework.
Test empty/default model, prefixed model, unsupported model, and already
normalized model for both providers.

### 2. UI constants with an existing dependency direction

Evidence: `vtcode-config/src/constants/ui.rs:6` and
`vtcode-ui/src/tui/config/constants/ui.rs:5` duplicate slash palette and modal
constants. UI already depends on config; config does not depend on UI.

Inventory public constant names and values first. Re-export identical stable
defaults from config where appropriate; retain UI-only rendering calculations
in UI. Do not blindly replace the whole file: transcript bottom padding is
explicitly documented as a compatibility duplicate, and UI owns its viewport
clamping function. Keep the existing product-name parity test.

### 3. ACP/Copilot stdio transport primitives

Evidence: `vtcode-acp/src/transport.rs` and
`vtcode-llm/src/copilot/transport.rs` both implement bounded write queues,
pending-call guards, JSON-RPC dispatch, bounded reads, and child teardown.
Their matching notification tests are supporting evidence; production code
was also inspected.

Start with bounded framing in an existing commons infrastructure module.
ACP's `read_bounded_line` at line 369 excludes the newline from retained
content/cap accounting; Copilot's reader at line 391 includes it and reuses a
caller buffer. Establish and test an explicit delimiter policy before sharing.
ACP supports string/numeric incoming IDs and configurable JSON-RPC version;
Copilot uses numeric IDs. Preserve those adapter contracts and error types.
Only consider moving the full transport after framing reuse proves useful.
Do not add an LLM dependency on ACP or move domain errors into commons.

### 4. Two live UI command/handle protocols

Evidence: `vtcode-ui/src/tui/core_tui/types/protocol.rs:266` and
`app/types/protocol.rs:401` duplicate handle send/append/replace operations.
Both modules are declared and exported through their respective type facades.
The app handle additionally owns deferred input and transient activity.

Map callers and enum differences before selecting one canonical command
vocabulary. Extract shared payload types first; preserve facade imports.
Keep app event ownership and deferred/transient state explicit. A full handle
merge requires migration tests for both paths, not a mechanical text move.

### Rejected clone candidates

- Repeated controller imports and allow attributes are not reusable behavior.
- Agent preview formatting in slash-command runtime is marked `cfg(test)`;
  its overlap with startup UI is not a second production implementation.
- Runloop and core agent infrastructure are different layers. Their aggregate
  sizes do not justify deduplicating one against the other.
- Existing deferred validation-type work and the AsyncMcpManager actor rewrite
  are not reopened by this scan.

## Incremental execution plan

1. **Small deduplication:** OpenCode validation helper, then audited constant
   re-exports. Separate commits; no unrelated provider changes.
2. **Provider decomposition:** move cohesive groups from `common.rs` and
   `shared/mod.rs` verbatim; preserve visibility/re-exports. Then remove
   duplication only where multiple callers share the same contract.
3. **Planning decomposition:** split tracker codec, section/step parsing, and
   verification classification. Keep `validate_plan_content` as the facade.
   Preserve acceptance rules and useful repair feedback.
4. **Application decomposition:** extract WebMCP evidence/settings controllers
   using the existing evidence/persistence/editor modules; extract VS Code
   view and terminal/config services using its existing command registry and
   backend integration. Inject narrow state/callbacks instead of copying globals.
5. **Infrastructure reuse:** characterize ACP/Copilot framing and cancellation;
   share framing first. Independently migrate shared UI protocol payloads.
6. **Execution facade:** move the reentrancy implementation, then extract
   contiguous execution stages. Preserve safety/admission order, prevalidation,
   settlement mode, snapshots, timeout annotation, and structured error shape.
7. **Copilot/runtime orchestration:** map state ownership and lifecycle exits
   before extraction. Reuse existing `session_setup`, `session_teardown`,
   orchestration `turn_tail`, and interaction-loop helpers. Keep one authoritative
   state machine; do not introduce a parallel runner or a giant context bag.
8. **Release script:** move pure changelog functions first, using established
   release libraries. Validate in fixtures/dry-run paths; never run publication
   as a refactor check.

Each step should be reviewable and independently revertible. Prefer cohesive
modules of a few hundred lines; this is a navigation goal, not a hard line cap.
Success means fewer implementation owners for shared behavior and clearer
state ownership, rather than simply more files. Every extraction must be wired
into live callers; do not retain unused parallel implementations.

## Verification required during implementation

- Rust: `cargo fmt --all -- --check`, `./scripts/check-dev.sh`, focused
  `cargo nextest run --locked -p <affected-crate>`, and relevant warnings-denied
  `cargo check --locked`. Broaden tests when interfaces or multiple consumers change.
- Provider splits: history replay, tool-result pairing, reasoning ordering,
  cache usage, split UTF-8, SSE boundaries, finish reasons, and cancellation.
- Planning: existing artifacts/verification tests, quoted metadata, shell
  metacharacters, concrete verification, vague steps, and tracker round trips.
- Transport: cap minus/at/plus one, delimiter accounting, oversized-line draining,
  partial EOF, malformed frames, IDs, timeout/drop cleanup, queue pressure,
  secret redaction, and child/task shutdown. Test both adapters.
- UI/runtime: wide/narrow resize, scrolling, selection, links, overlays,
  deferred events, streaming, completion, interrupt, new session, and resume.
  Run the documented PTY/inline-event regression suites for affected paths.
- Apps: existing package lint/type/build/test commands plus rendered interaction
  checks for connection, drafts, approval, and terminal lifecycle changes.
- Release: Bash syntax, available shell lint, and fixture-based changelog and
  packaging checks with publication disabled.
- Update module guidance after significant boundary/API changes; update docs
  when behavior changes. No new production dependencies are planned.

## Validation of this scan

Ran tracked-file inventory, line counting, clone-candidate comparison, targeted
source reads, module-export/dependency inspection, and Git status checks.
No builds or tests were run: this change contains analysis artifacts only.
The recommendations above are maintainability proposals, not verified bugs.

## Implementation progress

### Step 1: small deduplication

- OpenCode Go and Zen delegate normalized request validation to one helper in
  `providers/opencode_shared.rs`. Provider-specific normalization, supported
  models, error context, empty-model acceptance, and request immutability remain
  unchanged. The normalized-model branch retains clone-on-demand behavior.
- Added four dispatcher regressions covering accepted/prefixed/blank models,
  unknown models, separate provider allowlists, and message validation.
- Re-exported 19 identical viewport, slash palette, and modal defaults from
  config through the existing UI paths. No new config constants or dependencies.
  UI-specific rendering values and transcript-padding calculation remain local.
- Audited UI module guidance and documented shared-default re-exports.
- Focused validation: seven OpenCode tests and 200 UI modal/slash/constants/
  viewport tests passed. Independently compared all UI constant declarations
  against the pre-change version to verify names, types, and expressions.
- `./scripts/check-dev.sh --quiet` passed formatting, warnings-denied Clippy,
  compilation, and shell lint. `cargo check --locked -p vtcode-llm -p vtcode-ui`
  passed. Scoped `git diff --check` passed; no full workspace test run.
- Next step: provider helper decomposition, using verbatim moves and existing
  re-exports before any additional semantic deduplication.
