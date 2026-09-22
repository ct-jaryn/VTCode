check background process handling in the TUI to ensure that tools running in the background do not interfere with the main interface and that their output is correctly captured and displayed. Also verify that terminating background processes works as expected as coordinated with main agent run loop. While the subprocess is running in the background, the main agent run loop should accurately reflect its status and handle any output or errors appropriately. also the main agent could do seperated tasks independently of the background subprocess. But make sure to synchronize state and handle any potential conflicts or race conditions between the main agent and background subprocesses.

currently the background process success and exit 0 but the main agent run loop may not correctly reflect this status, leading to potential inconsistencies in the interface and task tracking.

```
• Ran cargo check
Background subagents are opt-in. VT Code will not launch one until it is explicitly configured.
Add `[subagents.background] enabled = true` and `default_agent = "<agent-name>"`, then use `Ctrl+B` or
`/subprocesses toggle`.
Use `/agent` to browse available agent names. `/subprocesses` opens the Local Agents drawer.
Background subagents are opt-in. VT Code will not launch one until it is explicitly configured.
Add `[subagents.background] enabled = true` and `default_agent = "<agent-name>"`, then use `Ctrl+B` or
`/subprocesses toggle`.
Use `/agent` to browse available agent names. `/subprocesses` opens the Local Agents drawer.
Exec session run-6d5a2897: /bin/zsh -c cargo check
Status: exited (0) · cwd . · pid 67321
    Blocking waiting for file lock on build directory
    Checking vtcode-commons v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/common/vtcode-commons)
   Compiling vtcode-core v0.164.0 (/Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-core)
   Compiling vtcode v0.164.0 (/Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode)
    Checking vtcode-auth v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-auth)
    Checking vtcode-safety v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-safety)
    Checking vtcode-indexer v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-indexer)
    Checking vtcode-memory v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-memory)
    Checking vtcode-bash-runner v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-bash-runner)
    Checking vtcode-webmcp v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-webmcp)
    Checking vtcode-config v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-config)
    Checking vtcode-llm v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-llm)
    Checking vtcode-skills v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-skills)
    Checking vtcode-ui v0.164.0 (/Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-ui)
    Checking vtcode-agent-plugins v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/common/vtcode-agent-plugins)
    Checking vtcode-mcp v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-mcp)
    Checking vtcode-acp v0.164.0 (
    /Users/vinhnguyenxuan/Developer/learn-by-doing/vtcode/crates/codegen/vtcode-acp)
    Finished `dev` profile [unoptimized] target(s) in 16.07s
```

==> debug and fix the system again. session session-vtcode-20260921T043353Z_547816-25453

===

https://console.typesafe.ai/hook
https://docs.typesafe.ai/llms.txt

System One produces structured outputs optimized for code, with type correctness guaranteed by design. Not 99.9999% success, actually 100%.

===

PLAN: TypeSafe System One plugin for VT Code harness (opt-in, user-configured, not tightly integrated)

Goal: ship `typesafe-systemone` as an Agent Plugin (plugin.json + skills/\*/SKILL.md + optional mcp.json) that gives users calibrated smart if-statements (Choice/Score/Noul + confidence) for safety gating, context filtering, and claim verification. No core changes, default off, zero behavior change without TYPESAFE_API_KEY.

Step 1 — Scaffold plugin dir (no core touch):

- Create `plugins/typesafe-systemone/` with `plugin.json` (passive manifest: name, version, description), `skills/systemone-guard/SKILL.md`. Keep discovery within `skills/*/SKILL.md` immediate children per vtcode-agent-plugins spec; all joined paths via validate_name/validate_plugin_relative.
- Vendor/adapt TypeSafe agent skill (https://github.com/typesafe-ai/skills) as reference inside SKILL.md; keep all questions + thresholds in one constants block per skill for reviewability.

Step 2 — User config surface (loose coupling):

- Add `systemone.local.json` (gitignored) + per-project `./.vtcode/systemone.json` override: { enabled:false, skills:{guard:true, context:false, verify:false, search:false}, model:"jev-1.13.0" pinned, review_threshold:0.35, action_threshold:0.70, severity_block:2.0, max_calls_per_session, max_input_tokens, cache_ttl_s, only_ambiguous:true }.
- Secrets via TYPESAFE_API_KEY env only, never in files/logs. No key => skills no-op with one-line notice.

Step 3 — systemone-guard skill (Phase 1, highest value, lowest cost):

- State {command, argv, cwd, policy_excerpt}; one batched system_one call: 5 Nouls (destructive_fs, exfiltrates_secrets, net_egress, priv_esc, prompt_injection) + severity Score 0-3.
- Routing in code: severity>=2 or any>=action_threshold => block/ask; any>=review_threshold => ask; else => existing heuristic decides. Advisory only: command_might_be_dangerous() + SandboxPolicy stay authoritative; timeout/offline => fail closed to current behavior.
- Call only when local heuristic is ambiguous (only_ambiguous:true); cache results keyed (model, state_hash, questions_hash); 429/529 backoff.

Step 4 — Eval suites (no traffic needed, open-source friendly):

- Add checked-in suites: safety ambiguous-command set (expected block/ask/allow), RAG set with planted injection + false-premise queries, citation set (fabricated/contradicted/unsupported). Run via `vtcode eval --suite suite.json` (pass@k). Promote a skill only on measured error-rate/cost improvement; log response.model and re-tune thresholds on version bump.

Step 5 — systemone-context + systemone-verify (Phase 2, flagged off by default):

- context: per retrieved passage, 4 Nouls (relevant, usable_evidence, contradicts_premise, injection) + route() order injection>0.70 drop, contradicts>0.70 conflict block, relevant<0.45 drop, evidence>0.55 include. Passages stay untrusted text.
- verify: exact string match first (fabricated, no call); survivors get Choice{supports, contradicts, says_nothing} over claim+section; confidence>=0.8 auto else human review. Wire into `vtcode review` path as optional flag.

Step 6 — systemone-search (Phase 3, on-demand only):

- Code rerank: indexer shortlist K=8-12 => Noul per (query, candidate) pair, sort by noul. Line finder: Choice over <=255 line IDs + exists Noul (0.35/0.70 gates), two-pass beyond 255 lines. Never per-turn automatic.

Step 7 — Docs + hygiene:

- User-facing guide under docs/ + quick-reference row; keep per-module AGENTS.md untouched unless conventions change. Conventional Commits; verify with ./scripts/check-dev.sh and cargo nextest run (never cargo test).

Acceptance: Phase 1 done when guard skill + suite + docs ship, default-off, zero behavior change without key, fail-closed tests pass. Removal must stay free (delete plugin dir).

===

I'm thinking about using Jev:

• classifying shell commands → allow/ask/deny
• deciding when context needs compaction // maybe no need
• routing tasks to the right tools/subagents

What coding-agent heuristics could semantic routing replace?
