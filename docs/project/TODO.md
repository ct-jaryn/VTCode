===

Plan for the cheap targeted timing test:

Goal: decide whether open_focused's full wrap-upfront in build_cached_block (transcript_review.rs) is the open-freeze, before committing to lazy Tool wrapping.
Test (throwaway, not committed): temp nextest test in transcript_review.rs that builds sessions at realistic scales and prints timings via eprintln! with -- --nocapture:

1. 64 blocks × 200 short lines (normal large session) — time open + one refresh append.
2. 1 block × 20k lines (single TUI_TOOL_OUTPUT_CAPTURE_MAX_LINES-scale flood) — time open.
3. Case 2 + committed search "alpha" — time refresh append (incremental path) vs fresh recompute_matches (full path).
   Evaluate:

- If case 2 open is >500ms in debug (roughly >50–100ms release-equivalent), the wrap-upfront is confirmed as the freeze → proceed to lazy Tool-block wrapping (store raw lines, wrap visible window + search/export on demand).
- If case 1 dominates instead, the cost is per-block reflow count → proceed to capping refresh_messages work per frame or throttling follow-while-open.
- If all under budget, no structural change: close the lag report as fixed by the incremental-search + Esc changes in cf89fb8e4.
  Improve step (only if confirmed): smallest slice first — lazy-wrap just ReviewSourceKind::Tool blocks behind the existing CachedToolOutputBlock shape, keep Core path as-is; re-run the same timing test plus the 38 viewer tests.

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
