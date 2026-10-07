===

Planning-contract restatement — guidelines.rs:456-458 vs system.rs:55/64/34 state the same three planning rules; they co-render in planning mode. Highest token win, but plan_blocks.rs parses <proposed_plan> output shaped by that text — riskiest item, worth its own change.
tool-specs inline literals — crates/common/vtcode-utility-tool-specs/src/lib.rs:226, 261 hardcode 1/50000/10000 next to the consts at :55-59; outside the prompts focus.
generate_temporal_context (temporal.rs:26) is production-dead (only generate_temporal_date_context is used); deleting it is defensible but touches doctests.
Verifier/sandbox prompt↔enforcement splits — intentionally two homes per inline comments and .vtcode/memory gotchas; do not merge.
Two audit claims turned out false on verification, so nothing was changed there: the guards::MAX_SAME_FILE_PATH_READ_CALLS twin exists (read_guard.rs:33), and the Skills truncation the DEFERRED_TOOLS_MAX_DESC_CHARS comment cites exists (vtcode-skills/src/render.rs:133).

===

Known limitations

1. The interactive TUI uses alternate-screen fullscreen rendering (like vim or less). This limits terminal scrollback and some screen-reader virtual buffers. Workarounds: headless ask/exec, Transcript Review raw mode (R), [ for native scrollback, or v to read in your editor.
2. Complex live-updating rows (progress, background-task indicators) are simplified under reduced-motion and screen-reader modes, but not every animated surface has a separate static equivalent yet.
3. Windows support and some terminal-specific key bindings (for example Shift+Enter multiline input) vary by terminal; the keyboard-shortcuts guide documents per-terminal notes and fallbacks.

We treat these as bugs when they block real workflows. Reporting them helps us prioritize.

===

scan for large and monolith files and module and plan deduplication and refactor and extract reusable components.

===

prioritized refactor plan (docs/development/refactor-scan-2026-10-04.md) and full Rust file inventory (docs/development/refactor-scan-2026-10-04.csv).

===

Investigate and fix the agent-loop/tooling regression shown in this session:

`/Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/.vtcode/sessions/session-vtcode-20261006T035519Z_302954-43619`

Observed failure:

> Reads of README.md exhausted the per-file cap (6) this turn, and a verifier was run behind a filtering pipe so it did not clear the verification gate; retries then tripped the blocked-call fuse. Tools are disabled for this pass.

The agent appears to stop before completing the task after:

- hitting the per-file read cap,
- failing to satisfy the verification gate because the verifier ran through a pipe/filter,
- retrying until the blocked-call fuse disables tools.

Also investigate this `apply_patch` failure:

> Tool 'apply_patch' failed: Execution failed: Patch context mismatch in 'README.md': use complete current lines and preserve internal…

Tasks:

1. Reconstruct the failing agent/tool-call sequence from the session and identify the exact root causes.
2. Check recent commits for regressions affecting the agent loop, read limits, verification gate, blocked-call fuse, `apply_patch`, retry/recovery logic, and tool routing.
3. Fix the underlying behavior rather than only handling this specific session.
4. Ensure reaching a per-file cap cannot leave the agent stuck when it already has enough context to continue.
5. Ensure valid verification commands still satisfy the verification gate when output is piped or filtered, where safe.
6. Improve recovery from `apply_patch` context mismatch. Refresh stale file context or retry with current complete lines instead of repeatedly failing with the same patch.
7. Prevent repeated recoverable failures from unnecessarily tripping the blocked-call fuse.
8. Preserve existing safety limits and avoid weakening protections just to make this session pass.

Keep the fix surgical, KISS, and DRY. Follow existing VT Code conventions.

Verify with focused tests that reproduce:

- per-file read-cap exhaustion,
- verifier commands with pipes/filters,
- stale `apply_patch` context,
- repeated recoverable tool failures,
- successful continuation of the agent loop after recovery.

Also run the relevant existing checks and inspect recent commits/diffs to identify which change introduced the regression.

Report:
`problem | evidence/session event or file:line | regression commit if found | fix | verification`

===

# Targeted Transcript Review Timing Probe

## Summary

Add a temporary, uncommitted test in `crates/codegen/vtcode-ui/src/tui/core_tui/app/session/transcript_review/tests.rs`. Measure open, append refresh, and search costs before choosing an optimization. No public API changes or production dependencies.

## Probe

- Use deterministic Tool captures with unique IDs, short indexed lines, and sparse `alpha` matches. Fix review width/height at 80×24 and use default Rich mode.
- Build fixtures, append captures, clone comparison states, and perform assertions outside timed intervals. Use `Instant`, `black_box`, and `eprintln!`.
- Run one warm-up and three measured repetitions with fresh viewer states; report each sample and median.

| Case | Fixture                              | Measurements                                                                |
| ---- | ------------------------------------ | --------------------------------------------------------------------------- |
| 1    | 64 captures × 200 lines              | `open_focused`; refresh after adding a two-line tail capture                |
| 2    | One capture × 20,000 lines           | `open_focused`                                                              |
| 3    | Case 2 with committed `alpha` search | Append refresh; isolated incremental search versus full `recompute_matches` |

For case 3, preserve the large cached capture and append a new tail capture containing one non-match and one match. Prepare equivalent states immediately after `refresh_messages`; compare incremental and full search with identical prefix lowercase caches. Also report end-to-end refresh separately.

Use direct test fixtures for the appended 65th capture, avoiding FIFO eviction as a timing confound. Assert the intended capture and row counts.

## Attribution and Decision Gates

- Separately measure source collection, all `build_cached_block` calls, and wrapping-only work over the same plain-text payload. Keep these diagnostic passes separate from open timing.
- If case 1 dominates, compare **one × 12,800** against **64 × 200**, using identical total text. Tool-only fixtures cannot establish a Core reflow bottleneck; add a corresponding Core comparison before making that claim.
- Case 2 consistently above **500ms debug**, with wrapping accounting for most of the cost: proceed to a separate increment that lazily wraps only `ReviewSourceKind::Tool`, behind `CachedToolOutputBlock`, retaining the Core path. Re-run this probe plus scroll, search, evidence-link, copy/export, and resize regressions.
- Confirmed block-count overhead: target the measured refresh/source/reflow stage. Do not throttle follow behavior unless repeated refresh work is the demonstrated bottleneck.
- All cases comfortably within budget: make no structural change. Reject this freeze hypothesis; close the lag report only if its original reproduction also succeeds.
- Near-threshold or noisy results: repeat the affected case in release. Do not infer release timing from a fixed debug multiplier.

## Run and Cleanup

```
cargo nextest run --locked -p vtcode-ui --lib \
  -E 'test(transcript_review_timing_probe)' -- --nocapture
```

Assert exact expected search rows and equality between incremental and full results. Timing values are observations, not pass/fail assertions.

Remove only the temporary probe afterward. Check the scoped diff and final worktree status, preserving the existing unrelated edit. Report timings, attribution, and the selected next step; commit nothing.

===

check /new command doesn't trigger new session properly and lagging a very long delay ~10s and very slow. also when the program exit, there is a noticeable delay before it fully terminates.

===

check there is always a noticeable delay after user's first message and subsequent responses from the model, it affects the overall responsiveness and user experience. find a way to optimize the response time and improve user experience and feedback loop. currently only global loading status is shown, which may not provide sufficient feedback to the user. suggestions:

- Implement a more granular loading indicator that shows the progress of individual tasks or messages.
- Optimize the backend processing to reduce latency for initial and subsequent responses.
- Consider preloading or caching frequently used resources to speed up response times.
- Provide immediate visual feedback to the user when a new session is initiated or a command is executed.
- Investigate and address any underlying performance bottlenecks that contribute to the observed delays.

KISS and DRY principles should be applied when implementing these optimizations. Ensure that any changes made do not introduce unnecessary complexity or redundancy.

===

https://developers.openai.com/api/docs/guides/decisions
