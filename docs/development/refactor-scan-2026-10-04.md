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
No builds or tests were run for the initial scan; implementation checks appear below.
The recommendations above are maintainability proposals, not verified bugs.

## Implementation progress

Steps 1 and 2 were committed together at the owner's request as `3f8fcb607`
(`refactor(providers): extract shared wire helpers and deduplicate defaults`).

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

### Step 2: provider helper decomposition

- Reduced `providers/common.rs` from 1,766 lines to a 65-line facade over
  private chat, HTTP, request, reasoning, prompt-cache, streaming, and token-count
  modules. Existing caller imports and public visibility remain stable.
  Its existing tests now live in `common/tests.rs`.
- Reduced `providers/shared/mod.rs` from 1,832 lines to a 39-line facade.
  Stream assembly, tool-output/compacted-history conversion, prompt lineage,
  cache-token usage, and UTF-8 decoding now have dedicated modules.
  SSE payload extraction and boundary detection joined the existing SSE module.
- Moved private-helper tests alongside their implementations. Retained existing
  Responses adapters and sanitizer modules. No new dependencies or runtime
  behavior changes; semantic deduplication remains separate from extraction.
- Structural comparison with `ast-grep` verified all 181 function bodies
  (including tests), six structs, four enums, one trait, eight constants, and
  one macro across the moved surfaces. Comparison permits module-local visibility,
  indentation changes, and the required HTTP error-handler path qualification;
  string literals remain protected from whitespace normalization.
- `cargo nextest run --locked -p vtcode-llm`: 1,147 passed, zero skipped.
- `./scripts/check-dev.sh --quiet` passed formatting, warnings-denied Clippy,
  compilation, and shell lint. Scoped `git diff --check` and moved-file whitespace
  checks passed. Verified all 15 original public declarations retain facade exports.
- Final `cargo check --locked -p vtcode-llm` passed after the extraction stabilized.
- Updated the LLM crate's module map and facade/visibility guidance.
- Next step: planning artifact decomposition, preserving validation and repair
  feedback through the existing `validate_plan_content` facade.

### Step 3: planning artifact decomposition

- Reduced `planning_workflow/artifacts.rs` from 2,071 lines to a 25-line facade
  over section parsing, quote-aware step parsing, tracker conversion, validation,
  and verification classification. Existing caller imports and public APIs remain
  stable; shared internal helpers have module-scoped visibility.
- Kept validator-owned repair feedback, acceptance rules, embedded tracker markers,
  and path pairing unchanged. Parsing serves both validation and tracker conversion
  without circular module dependencies. Existing agentic-testing cases now live in
  a dedicated test module; no new dependencies or behavior changes.
- Structural comparison with `ast-grep` verified all 75 function bodies, three
  structs, one enum, the report implementation, and 17 constants. Comparison permits
  module-local visibility, indentation, and optional trailing commas while preserving
  string literals. Verified all 10 public declarations retain facade exports.
- `cargo nextest run --locked -p vtcode-core -E 'test(planning_workflow) | test(planning_task_tracker)'`:
  122 passed, 3,929 skipped. This covers planning artifacts, tracker persistence,
  metadata parsing, command checks, policy transitions, and planning runtime callers.
- `./scripts/check-dev.sh --quiet` passed formatting, warnings-denied Clippy,
  compilation, and shell lint. Final `cargo check --locked -p vtcode-core`, scoped
  whitespace checks, and `git diff --check` passed. No full workspace test run.
- Audited core module guidance and recorded the new artifact responsibility map.
- Next step: application decomposition, starting with the WebMCP controllers and
  their existing evidence, persistence, and editor boundaries.

### Step 4a: WebMCP evidence and settings controllers

- Reduced `apps/webmcp/src/main.ts` from 1,846 to 1,533 lines. Extracted the typed
  required-element lookup to `app-elements.ts`, evidence presentation/observation
  to `evidence-controller.ts`, and bridge settings/setup presentation to
  `settings-controller.ts`. Event wiring and editor/proposal/backend ownership stay
  in the application entry point.
- Controllers consume narrow callbacks and current connection facts. Settings read
  the live backend on each render, preserving pairing/disconnection updates.
  Evidence uses the existing bounded, sanitized recorder; failures remain isolated
  from the instrumented tool's result or original error. No dependencies added.
- TypeScript syntax-tree comparison verified 22 moved and 69 retained function
  bodies. The only body adaptations are current-backend access in settings and
  discovery-name setter calls in registration. Existing strings, dialog guards,
  setup command quoting, and approval logic remain unchanged.
- Added six controller tests for post-call editor state, result/error identity,
  failing evidence capture, discovery reset, literal display, backend replacement,
  modal guards/focus, setup command quoting, and credential-free persistence with
  storage recovery. `bun run test`: 66 passed, zero skipped.
- `bun run typecheck`, `bun run build`, `bun audit --audit-level=high`,
  `./scripts/check-dev.sh --quiet`, and `git diff --check` passed. Vite reports
  a JavaScript chunk larger than 500 kB; this extraction does not split bundles.
- Rendered browser checks could not run: the browser-use runtime was unavailable
  and the computer-use browser inventory was empty. DOM tests do not establish
  real dialog geometry, keyboard behavior, or connected-browser integration.
- Added app-local module guidance after auditing the new ownership boundaries.
- Next increment: VS Code views and terminal/config services within step 4;
  rendered WebMCP checks remain an explicit validation gap.

### Step 4b: VS Code quick-action and workspace-status views

- Reduced `extensions/vscode-extension/src/extension.ts` from 3,534 to 3,037
  lines. Quick-action descriptions and their tree adapter now live in
  `views/quickActions.ts`; workspace-status descriptions and their adapter live in
  `views/workspaceInsights.ts`. Activation, command wiring, trust, configuration
  updates, and terminal ownership remain in the entry point.
- Passed the existing executable-path getter and status-tooltip builder as narrow
  services to workspace insights. Restricted-workspace rendering still avoids
  reading executable configuration. Tree providers keep reading current state
  through the existing callbacks. No new dependencies or runtime behavior changes.
- TypeScript syntax-tree comparison verified all 75 original function bodies,
  five classes, and 15 interfaces across the entry point and extracted modules;
  only service injection and its activation call were adapted.
- Added five Node tests covering restricted/trusted command lists, CLI availability,
  HITL/full-auto/provider branches, parse feedback, shared tooltip arguments,
  live state, refresh notifications, and tree-item metadata. They compile the real
  view modules with TypeScript and load an isolated VS Code API fixture.
- Added real `test:views` and `typecheck:views` scripts plus a focused TS config.
  The existing compile/typecheck/lint/test scripts only print skip messages.
  Updated the development guide and added app-local ownership/verification guidance.
- `npm run test:views`: five passed, zero skipped. `npm run typecheck:views`,
  direct view lint through the existing ESLint config, and
  `npm run bundle -- --production` passed. Rebuilt the local esbuild dependency
  after detecting an installed-binary platform mismatch; manifests/dependency
  versions and lockfile remain unchanged apart from the new verification scripts.
- `npm audit --omit=dev --audit-level=high` found zero production dependency
  vulnerabilities. The entry-point typecheck retains the same eight diagnostics
  as the baseline; full-project typechecking also fails on legacy tests. No full
  extension test pass or rendered Extension Development Host validation is claimed.
- `./scripts/check-dev.sh --quiet`, scoped Markdown lint, and staged
  `git diff --check` passed.
- Next increment: terminal/config services within step 4. Existing typecheck
  diagnostics and rendered app/extension checks remain explicit validation gaps.

### Step 4c: shared CLI process execution and configuration commands

- Reduced `extensions/vscode-extension/src/extension.ts` from 3,037 to 2,618
  lines and `utils/vtcodeRunner.ts` from 194 to 125 lines. Both CLI runners now
  use `services/processExecution.ts` for spawning, streaming, progress,
  cancellation, and completion. Their existing trust checks and caller-specific
  config/context/environment preparation remain in their respective wrappers.
- Removed duplicate executable-path and logging helpers from the entry point;
  its config-argument adapter delegates to the existing URI-based helper.
  Kept different workspace-root and environment-selection contracts separate.
- Moved the four HITL/tool-policy/MCP configuration registrations into
  `commands/configurationCommands.ts`. The entry point retains subscription
  ownership and supplies live summary, trust, output, error, and guide services.
  Summary lookup still occurs after awaiting the config picker; command IDs,
  registration order, trust gates, feedback, and TOML mutation APIs are preserved.
- Structural comparison verified 70 retained/delegated function bodies, all four
  moved callbacks, both original process implementations, caller preflight, and
  remaining activation wiring. Removed one unused catch binding identified by
  lint; its error-handling behavior is unchanged. No new production dependencies.
- Added 11 service tests and reused one compilation/fixture helper with the five
  existing view tests. The combined run passed all 16 tests without skips.
  Coverage includes literal argv, a real Node child process with shell
  metacharacters and explicit environment values, streaming, cancellation,
  spawn errors, live progress environment, trust rejection, picker-time config
  changes, stale summary reload, failed updates, provider state, and disposal.
- Added real `test:services` and `typecheck:services` scripts and a focused config.
  Service typecheck/lint, production bundle, production dependency audit,
  `./scripts/check-dev.sh --quiet`, and scoped diff checks passed. The direct
  entry-point typecheck retains exactly the same eight baseline diagnostics.
- Updated module guidance and development instructions. Tests mock VS Code and
  configuration APIs; actual CLI execution, filesystem edits, full activation,
  and rendered/interactive terminal checks remain unverified.
- Next increment: interactive terminal ownership/lifecycle extraction within
  step 4, then the transport/UI infrastructure reuse phase.

### Step 4d: interactive terminal ownership and launch lifecycle

- Reduced the entry point from 2,618 to 2,552 lines by extracting
  `services/interactiveTerminal.ts`; the entry point supplies live
  environment/config/context/trust/error callbacks and owns service disposal.
  Retained terminal name, cwd, icon, reuse, delayed launch, and command formatting.
- Fixed delayed work surviving a terminal close or extension shutdown. Session
  identity prevents an in-flight context flush from sending into a closed terminal
  or affecting its replacement. Trust is checked before and after the flush;
  flush failures reach the command error handler instead of becoming unhandled
  promise rejections. Disposal cancels the timer and closes the terminal once.
- Added nine focused lifecycle tests. All 25 combined view/service/terminal tests
  pass. Tests use mock terminal APIs and a fake clock; real host profiles and
  shell interaction remain unverified. Existing shell formatting is preserved;
  a separate shell-launch hardening increment needs profile-specific coverage.
- Service typecheck, service lint, production bundle, repository fast gate,
  Markdown lint, and scoped diff checks passed. AST comparison confirmed 68
  retained or moved functions; the entry-point typecheck and lint retain their
  eight and 21 baseline diagnostics respectively.
- Next increment: assess shell-safe interactive launching, then step 5 transport
  framing and UI protocol reuse. No production dependencies added.

### Step 4e: native interactive CLI launch

- Removed shell command interpolation and the activation timer from the terminal
  service. Executable and config paths containing shell operators are now forwarded
  as `TerminalOptions.shellPath` and an argument array as `shellArgs`.
- The command awaits IDE context flush before creation. Concurrent requests share
  the pending session; trust revocation and shutdown cancel pending creation,
  failures release the session for retry, and close/reopen ownership is preserved.
- Documented the intentional environment change: the CLI is the terminal process,
  so shell profiles, aliases, and virtual-environment activation commands are not
  evaluated first. VS Code's inherited/configured terminal environment still applies;
  exiting VT Code ends the terminal process instead of returning to a shell prompt.
- All 28 combined Node tests pass, including 12 terminal cases. Adversarial cases
  assert executable and argv values containing shell operators, substitutions,
  quotes, newlines, Unicode, and empty arguments. A real Node process independently
  verifies forwarded arguments. No shell-text fallback exists.
- Service typecheck/lint, production dependency audit, production bundle,
  repository fast gate, scoped Markdown lint, and staged diff checks passed.
  Retained entry-point bodies and baseline typecheck/lint diagnostics were compared;
  the existing eight typecheck and 21 lint diagnostics remain unchanged.
- Native VS Code terminal behavior, Windows quoting, and full activation require
  host checks and are not claimed tested. No production dependencies added.
- Next increment: step 5 transport framing and UI protocol reuse.

### Step 5a: shared bounded subprocess line framing

- Added `vtcode-commons::line_framing` as the single bounded byte-framing
  implementation. ACP and Copilot retain their local adapters and error shapes;
  ACP explicitly excludes LF from the cap, while Copilot includes it. CR remains
  content. Copilot server-client headers also use the shared reader through the
  existing Copilot adapter. No production dependencies or crate boundaries changed.
- Retained drain-to-LF/EOF behavior, zero-cap and empty-line handling, final partial
  lines, byte-oriented truncation, and caller-buffer reuse. Pending-call guards,
  write-queue backpressure, routing, JSON-RPC IDs, and child teardown are untouched.
- Added four commons tests and two adapter regressions. An independent full-input
  oracle checks both policies across five inputs, nine caps, and nine chunk sizes;
  explicit cases cover exact caps, CRLF, oversized final lines, and partial-read
  errors. All 43 focused framing/transport/Copilot-client tests passed with the
  Copilot feature enabled, including timeout, EOF, and prompt cancellation cases.
- Warnings-denied locked checks for all three affected crates, the repository
  fast gate, formatting, scoped Markdown lint, and staged diff checks passed.
  Updated affected module guidance and documented the delimiter/error/cancellation
  contracts in `docs/development/stdio-line-framing.md` and the async guide.
- The reader is not cancellation-safe; its public contract requires callers to
  terminate on read failure rather than assuming a fresh frame boundary. Existing
  transport reader ownership is preserved. Live external ACP/Copilot subprocesses
  were not exercised; verification used unit and in-memory transport fixtures.
- Next increment: independently map and extract shared UI protocol payloads.
