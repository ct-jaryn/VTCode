use std::path::{Path, PathBuf};
use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};
use vtcode_core::core::agent::refusal;
use vtcode_core::llm::provider as uni;
use vtcode_core::tools::names::canonical_tool_name;
use vtcode_core::tools::tool_intent::{
    ShellActivity, classify_shell_activity, shell_args_as_executed, shell_command_is_admitted_verification_attempt,
};

use crate::agent::runloop::unified::tool_pipeline::{ToolExecutionStatus, ToolPipelineOutcome};
use crate::agent::runloop::unified::turn::tool_outcomes::read_extent;
use crate::agent::runloop::unified::turn::tool_outcomes::{
    is_empty_shell_search, is_grep_style_no_match, output_field_is_empty,
};

/// Threshold: number of consecutive file mutations before the Anti-Blind-Editing
/// warning fires. NL2Repo-Bench recommends verifying after every few edits.
pub(crate) const BLIND_EDITING_THRESHOLD: usize = 6;
pub(crate) const ANTI_BLIND_EDITING_WARNING: &str = "[!] Anti-Blind-Editing: run a verifier (build/test/lint — e.g. `cargo check`, `go test`, or `pytest`) and let it exit 0 before further edits.";
/// Ends with the text of [`VERIFIER_SHELL_FORM_NOTE`] so the shell forms it
/// describes match what the execution kernel elides; a test keeps the two in
/// lockstep because `concat!` cannot splice a cross-crate const.
pub(crate) const ANTI_BLIND_EDITING_DIRECTIVE: &str = "Several edits have landed without a build/test/lint run since the last check, so further code mutations are blocked until a verifier exits 0 (docs-only edits stay allowed). Run your project's build/test/lint tool with `exec_command` (e.g. `cargo check`, `go test`, `npm test`, or `pytest`), standalone or as a pure `&&` chain. Cap output with `max_output_tokens`. A verifier piped only into `head` or `tail` runs without the truncator and counts as standalone; filtering pipes (`| grep`), `;`, and `||` make the exit status another command's, so they do not clear the gate.";
/// Fix-up window granted after a failed verification attempt. A failed
/// `cargo check` / `cargo nextest run` must not deadlock the turn: the agent
/// needs a bounded number of edits to address the reported failure before
/// re-verifying. Each failed verifier refreshes this window, so blind editing
/// (many edits with no verifier attempt) stays blocked while fix-verify loops
/// can make progress.
pub(crate) const FAILED_VERIFICATION_FIX_ALLOWANCE: u8 = 2;
/// Warning rendered when a pending gate's verifier result was lost (the exec
/// session ended before the verifier's output was captured).
pub(crate) const VERIFICATION_RESULT_LOST_WARNING: &str =
    "[!] Verification result lost: the exec session ended before the verifier's output was captured.";
/// Model-facing directive paired with [`VERIFICATION_RESULT_LOST_WARNING`]:
/// a standalone verifier re-run is the only way to clear the pending gate.
pub(crate) const VERIFICATION_RESULT_LOST_DIRECTIVE: &str = "Verification result lost: the exec session ended before the verifier's output was captured. Re-run the verification command standalone or as a pure `&&` chain to confirm or reject the recent edits.";
/// Warning rendered while the failed-verifier fix-up window is active. Distinct
/// from [`ANTI_BLIND_EDITING_WARNING`] so the pending-verification block notice
/// does not imply verification was never run when the verifier already failed.
pub(crate) const FAILED_VERIFICATION_FIX_WARNING: &str =
    "[!] Verification failed: bounded fix edits granted before the gate re-arms.";
/// Model-facing directive paired with [`FAILED_VERIFICATION_FIX_WARNING`]:
/// the verifier ran and reported failure, so text responses must repair the
/// reported failure and re-run a standalone verifier instead of claiming
/// completion.
pub(crate) const FAILED_VERIFICATION_FIX_DIRECTIVE: &str = "The last verification command ran and failed. A bounded fix window is active: apply fixes for the reported failure, then re-run the verification command standalone or as a pure `&&` chain. The work is accepted once a verifier exits 0.";
/// Warning rendered when a verifier behind a filtering pipe or a `;`/`||`
/// join (e.g. `cargo check 2>&1 | grep error`) succeeded while the gate is
/// pending: the exit status belongs to another command, so the verifier's
/// success cannot clear the gate. Pure `head`/`tail` truncator shapes never
/// land here: the execution kernel runs them as standalone verifiers, and the
/// tracker classifies the command as executed
/// ([`vtcode_core::tools::tool_intent::shell_args_as_executed`]).
pub(crate) const PIPED_VERIFICATION_WARNING: &str = "[!] Piped verifier did not clear the verification gate: the exit status belongs to another command, not the verifier.";
/// Model-facing directive paired with [`PIPED_VERIFICATION_WARNING`]: without
/// this feedback a piped success reads as "verified" to the model and the
/// pending gate deadlocks the turn on unverified text responses.
pub(crate) const PIPED_VERIFICATION_DIRECTIVE: &str = "The verification command ran behind a filtering pipe or a `;`/`||` join, so its exit status belongs to another command (e.g. `grep`) and it did not clear the verification gate. Re-run the verifier standalone or as a pure `&&` chain of verifiers; a pipe only into `head` or `tail` also counts as standalone. Cap output with `max_output_tokens` instead of filtering it.";
/// Bounded in-turn autonomous recovery attempts when the model emits text
/// instead of a verifier while the gate is pending.
///
/// Without this, two explanatory text responses end the turn as `Blocked` and
/// force the user to type `continue` — a manual step that stalls long-running
/// autonomous work. Each attempt resets the text-response streak once and
/// injects a project-aware directive naming the exact verifier command (see
/// `default_verifier_for_workspace`), giving the model one more bounded
/// chance to verify before the turn blocks. Mirrors Codex's Stop-hook test
/// gate philosophy: the harness keeps the turn alive until verification is
/// attempted, rather than punishing the first explanatory responses.
pub(crate) const MAX_VERIFICATION_AUTO_RECOVERY_ATTEMPTS: u8 = 2;
/// Renderer line paired with the autonomous verification-recovery directive.
/// Distinct from [`ANTI_BLIND_EDITING_WARNING`] so transcripts show that the
/// harness granted an automatic retry (with attempt counts) instead of
/// repeating the initial warning.
pub(crate) const VERIFICATION_AUTO_RECOVERY_WARNING: &str =
    "[i] Verification still pending — autonomous recovery: run the named verifier now instead of replying with text.";
/// Cross-turn counterpart to [`MAX_VERIFICATION_AUTO_RECOVERY_ATTEMPTS`]:
/// how many additional autonomous turns the session loop may schedule after
/// a verification-blocked turn before requiring manual `continue`.
pub(crate) const MAX_VERIFICATION_AUTO_RECOVERY_TURNS: u8 = 2;
/// Consecutive failed harness auto-verifications before the harness stops
/// executing verifiers itself and escalates to a manual blocked handoff
/// carrying the failure log. Reset by any success, completed turn, or fresh
/// user input, so only a genuinely stuck suite trips it.
pub(crate) const MAX_VERIFICATION_CONSECUTIVE_FAILURES: u8 = 3;
/// Bound (in chars) for the harness auto-verification failure excerpt kept
/// for the escalated blocked handoff. The full tool output stays in history
/// and the spool; the handoff carries only the tail needed to triage.
pub(crate) const VERIFICATION_FAILURE_EXCERPT_CHARS: usize = 2000;
/// Tool-call id for the harness-synthesized verifier execution. Fixed (not
/// model-issued) so transcripts and history unambiguously attribute the call
/// to autonomous recovery rather than the model.
pub(crate) const HARNESS_AUTO_VERIFY_CALL_ID: &str = "harness-auto-verify";

/// Effective in-turn directive-retry budget, honoring
/// `[agent.harness.verification].in_turn_attempts` with the compiled constant
/// as fallback when no workspace config is present (tests, headless paths).
pub(crate) fn verification_in_turn_attempts(vt_cfg: Option<&vtcode_core::config::loader::VTCodeConfig>) -> u8 {
    vt_cfg
        .map(|cfg| cfg.agent.harness.verification.in_turn_attempts)
        .unwrap_or(MAX_VERIFICATION_AUTO_RECOVERY_ATTEMPTS)
}

/// Effective cross-turn recovery-turn budget, honoring
/// `[agent.harness.verification].cross_turn_turns`.
pub(crate) fn verification_cross_turn_turns(vt_cfg: Option<&vtcode_core::config::loader::VTCodeConfig>) -> u8 {
    vt_cfg
        .map(|cfg| cfg.agent.harness.verification.cross_turn_turns)
        .unwrap_or(MAX_VERIFICATION_AUTO_RECOVERY_TURNS)
}

/// Whether tracker-aware auto-continuation is enabled
/// (`[agent.harness.continuation].auto_continue_tracker`).
pub(crate) fn tracker_auto_continue_enabled(vt_cfg: Option<&vtcode_core::config::loader::VTCodeConfig>) -> bool {
    vt_cfg
        .map(|cfg| cfg.agent.harness.continuation.auto_continue_tracker)
        .unwrap_or(true)
}

/// Effective cross-turn tracker auto-continue budget
/// (`[agent.harness.continuation].cross_turn_turns`).
pub(crate) fn tracker_cross_turn_turns(vt_cfg: Option<&vtcode_core::config::loader::VTCodeConfig>) -> u8 {
    vt_cfg.map(|cfg| cfg.agent.harness.continuation.cross_turn_turns).unwrap_or(32)
}

/// Cap on incomplete tracker items listed in continuation prompts.
const TRACKER_CONTINUE_ITEM_CAP: usize = 4;

/// Outcome of a live `task_tracker` probe, distinguishing completion from
/// probe failure so caches do not retain stale incomplete steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrackerProbeOutcome {
    /// Checklist exists and at least one step is not `completed`.
    Incomplete(Vec<String>),
    /// Tracker is empty or every step is `completed` — authoritative clear.
    Complete,
    /// Tool missing / execute failed / malformed payload — keep last cache.
    Unavailable,
}

/// Classify a `task_tracker` `action=list` payload for cache/gate decisions.
pub(crate) fn tracker_probe_outcome(payload: &serde_json::Value) -> TrackerProbeOutcome {
    let Some(status) = payload.get("status").and_then(serde_json::Value::as_str) else {
        return TrackerProbeOutcome::Unavailable;
    };
    if status == "empty" {
        return TrackerProbeOutcome::Complete;
    }
    let Some(checklist) = payload.get("checklist") else {
        return TrackerProbeOutcome::Unavailable;
    };
    let Some(raw_items) = checklist.get("items").and_then(serde_json::Value::as_array) else {
        // Malformed checklist without items: do not treat as Complete (that
        // would clear caches and stop auto-continue on a broken probe shape).
        return TrackerProbeOutcome::Unavailable;
    };
    let items: Vec<String> = raw_items
        .iter()
        .filter(|item| item.get("status").and_then(serde_json::Value::as_str) != Some("completed"))
        .filter_map(|item| {
            let description = item.get("description").and_then(serde_json::Value::as_str)?;
            let status = item.get("status").and_then(serde_json::Value::as_str).unwrap_or("pending");
            let index = item.get("index").and_then(serde_json::Value::as_u64);
            Some(match index {
                Some(index) if index > 0 => format!("#{} {} ({})", index, description, status),
                _ => format!("{} ({})", description, status),
            })
        })
        .take(TRACKER_CONTINUE_ITEM_CAP)
        .collect();
    if items.is_empty() {
        TrackerProbeOutcome::Complete
    } else {
        TrackerProbeOutcome::Incomplete(items)
    }
}

/// Parse incomplete step labels from a `task_tracker` list payload.
///
/// Thin wrapper over [`tracker_probe_outcome`] for tests and call sites that
/// only need the incomplete `Option` shape.
#[cfg(test)]
pub(crate) fn parse_incomplete_tracker_items(payload: &serde_json::Value) -> Option<Vec<String>> {
    match tracker_probe_outcome(payload) {
        TrackerProbeOutcome::Incomplete(items) => Some(items),
        TrackerProbeOutcome::Complete | TrackerProbeOutcome::Unavailable => None,
    }
}

/// Count completed checklist items in a `task_tracker` `action=list` payload.
///
/// Used for progress-reset of the cross-turn auto-continue episode budget.
/// Returns `0` when the tracker is empty/absent.
pub(crate) fn parse_tracker_completed_count(payload: &serde_json::Value) -> u32 {
    let Some(status) = payload.get("status").and_then(serde_json::Value::as_str) else {
        return 0;
    };
    if status == "empty" {
        return 0;
    }
    payload
        .get("checklist")
        .and_then(|c| c.get("items"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("status").and_then(serde_json::Value::as_str) == Some("completed"))
        .count() as u32
}

/// Apply a live probe to an incomplete-items cache.
///
/// `Incomplete` replaces the cache; `Complete` **clears** it; `Unavailable`
/// leaves the previous value so transient probe failures do not drop
/// auto-continue. Returns the effective incomplete slice after the update.
pub(crate) fn apply_tracker_probe_to_cache(
    cache: &mut Option<Vec<String>>,
    probe: TrackerProbeOutcome,
) -> Option<&[String]> {
    match probe {
        TrackerProbeOutcome::Incomplete(items) => {
            *cache = Some(items);
        }
        TrackerProbeOutcome::Complete => {
            *cache = None;
        }
        TrackerProbeOutcome::Unavailable => {}
    }
    cache.as_deref()
}

/// Load incomplete `task_tracker` step descriptions via the live tool registry.
///
/// Returns `None` when the tracker is absent, empty, fully completed, or the
/// probe fails. Prefer [`probe_tracker_incomplete`] when a cache must
/// distinguish complete from unavailable.
pub(crate) async fn incomplete_tracker_items(
    tool_registry: &vtcode_core::tools::registry::ToolRegistry,
) -> Option<Vec<String>> {
    match probe_tracker_incomplete(tool_registry).await {
        TrackerProbeOutcome::Incomplete(items) => Some(items),
        TrackerProbeOutcome::Complete | TrackerProbeOutcome::Unavailable => None,
    }
}

/// Live tracker probe that distinguishes complete from unavailable.
pub(crate) async fn probe_tracker_incomplete(
    tool_registry: &vtcode_core::tools::registry::ToolRegistry,
) -> TrackerProbeOutcome {
    let Some(tool) = tool_registry.get_tool(vtcode_core::config::constants::tools::TASK_TRACKER) else {
        return TrackerProbeOutcome::Unavailable;
    };
    let Ok(payload) = tool.execute(serde_json::json!({ "action": "list" })).await else {
        return TrackerProbeOutcome::Unavailable;
    };
    tracker_probe_outcome(&payload)
}

/// Live completed-item count for progress-reset. Returns `None` on probe failure.
pub(crate) async fn tracker_completed_count(tool_registry: &vtcode_core::tools::registry::ToolRegistry) -> Option<u32> {
    let tool = tool_registry.get_tool(vtcode_core::config::constants::tools::TASK_TRACKER)?;
    let payload = tool.execute(serde_json::json!({ "action": "list" })).await.ok()?;
    Some(parse_tracker_completed_count(&payload))
}

/// Build the model-facing auto-continue follow-up for incomplete tracker work.
/// Stable opening shared with `is_internal_harness_follow_up`, which keys the
/// quiet path off this constant instead of a duplicated literal.
pub(crate) const TRACKER_CONTINUE_FOLLOW_UP_PREFIX: &str = "The task tracker still has incomplete steps:";

pub(crate) fn tracker_continue_follow_up(incomplete: &[String]) -> String {
    let joined = incomplete.join(", ");
    format!(
        "{TRACKER_CONTINUE_FOLLOW_UP_PREFIX} {joined}. This follow-up is the harness resuming \
         the work, so no user reply is needed. The next step is to continue with the next incomplete step \
         using tools and update task_tracker as steps complete. A status-only recap does not advance the \
         tracker. The turn can end when the tracker is complete, or when a user decision or a \
         permission/policy block stops progress."
    )
}

/// Stable opening for recoverable blocked-end auto-continue when no tracker
/// items remain (or none were created). Session evidence: build/auto turns
/// ending with recovery fallback / blocked-tool fuse parked at `Continue…`.
pub(crate) const RECOVERABLE_BLOCKED_CONTINUE_FOLLOW_UP_PREFIX: &str =
    "The previous turn ended on a recoverable block:";

pub(crate) fn recoverable_blocked_continue_follow_up(reason: &str) -> String {
    format!(
        "{RECOVERABLE_BLOCKED_CONTINUE_FOLLOW_UP_PREFIX} {reason} This follow-up is the harness resuming \
         the work, so no user reply is needed. Retry the requested work with tools; if a policy block \
         repeated, switch to a read-only approach or ask the user. A status-only recap does not advance \
         the work. The turn can end when the request is complete, or when a user decision or a \
         permission/policy block stops progress."
    )
}

pub(crate) fn recoverable_blocked_auto_continue_directive(reason: &str) -> String {
    format!(
        "Blocked-end auto-continue: the harness queued this turn after a recoverable block ({reason}). \
         No user reply is needed. Resume the original request; do not restate a status recap as the answer."
    )
}

/// Label of the system directive paired with a session-resume tracker continuation.
pub(crate) const TRACKER_RESUME_DIRECTIVE_LABEL: &str = "Resume continuation";
/// Label of the system directive paired with an in-session tracker auto-continue.
pub(crate) const TRACKER_AUTO_CONTINUE_DIRECTIVE_LABEL: &str = "Tracker auto-continue";

/// System directive paired with a queued tracker continuation. Both the
/// session-resume and in-session paths share this wording so the stated
/// consequence (the harness resumed; a recap does not advance the tracker)
/// cannot drift between them.
pub(crate) fn tracker_continue_directive(label: &str, incomplete: &[String]) -> String {
    format!(
        "{label}: task_tracker still has incomplete steps: {}. The harness queued this continuation, so no \
         user reply is needed. The next step is the next concrete tracker step; a status-only recap does not \
         advance the tracker while work remains.",
        incomplete.join(", ")
    )
}

/// Shared recoverable blocked-reason shapes for every mode's auto-continue.
/// Keep this the single allow-list so plan-mode and tracker classifiers cannot
/// drift (fuse / no-final / budget / recovery-fallback wording).
/// Deny lists stay per-mode: evaluation order differs (plan allows first so
/// `PLANNING_COMPLETED_TURN_FALLBACK_REASON` is not shadowed by
/// "approval-ready plan").
pub(crate) const RECOVERABLE_BLOCK_ALLOW_TOKENS: &[&str] = &[
    "recovery fallback",
    "recovery could not confirm",
    "recovery exhausted",
    "recovery was exhausted",
    "reached the safety cap",
    "safety cap",
    "preview budget",
    "tool preview budget",
    "turn budget",
    "tool budget",
    "tool loop budget",
    "tool loop",
    "tool-call budget",
    "tool follow-up",
    "tool-free recovery",
    "wall clock",
    "blocked due to repeated",
    "blocked after repeated",
    "without a harness-visible final assistant response",
    "blocked tool-call limit",
    "recovery tool-call limit",
    "consecutive blocked calls",
    "tool-call safety limit",
    "max tool",
    "per-turn tool",
    "read cap",
    "work budget",
    "budget exhausted",
    "budget ran out",
];

fn matches_recoverable_block_shape(lower: &str) -> bool {
    RECOVERABLE_BLOCK_ALLOW_TOKENS.iter().any(|token| lower.contains(token))
}

fn contains_any_token(lower: &str, tokens: &[&str]) -> bool {
    tokens.iter().any(|token| lower.contains(token))
}

/// Deny tokens shared by the tracker and plan-mode auto-continue classifiers.
/// They mirror production blocked-reason constants and true-handoff vocabulary:
/// `RECOVERY_CONTRACT_VIOLATION_REASON` ("final tool-free synthesis pass" /
/// "attempted more tool calls"), `PENDING_VERIFICATION_BLOCK_REASON`
/// ("verification is still pending"), `POST_TOOL_CONTEXT_COMPACTION_FAILED_REASON`
/// ("compaction could not reduce"), and UNMATCHED_TOOL_RESULT. When one of those
/// constants' wording changes, update the token here in the same commit — the
/// auto-continue gates classify by substring.
const RECOVERABLE_BLOCK_BASE_DENY_TOKENS: &[&str] = &[
    "permission",
    "user input",
    "request_user_input",
    "interview",
    "verification is still pending",
    "compaction could not reduce",
    "unmatched tool result",
    "attempted more tool calls",
    "final tool-free synthesis pass",
];

/// Tracker auto-continue deny tokens beyond [`RECOVERABLE_BLOCK_BASE_DENY_TOKENS`].
const TRACKER_AUTO_CONTINUE_EXTRA_DENY_TOKENS: &[&str] = &[
    "safety fuse",
    "manual intervention",
    "unverified assistant responses",
    "anti-blind",
    "verification gate",
    "context exceeded",
    "stale recovery state",
    "awaiting approval",
    "approval-ready plan remains",
];

/// Whether a blocked/completed turn reason is recoverable for tracker auto-queue
/// (budget/preview/tool-free recovery) rather than a user-input handoff.
///
/// Matches **production blocked-reason constants** (turn_loop / post_tool
/// recovery), not paraphrases. Unknown / missing reasons are not auto-queued
/// when the turn did not complete.
pub(crate) fn tracker_auto_continue_is_recoverable_block(reason: Option<&str>) -> bool {
    // A provider refusal is terminal for the refused request: resending it is
    // refused again. Checked before the substring classifiers because the
    // notice may quote provider or response text containing any token.
    if reason.is_some_and(refusal::is_refusal_notice) {
        return false;
    }
    let Some(reason) = reason.map(str::to_ascii_lowercase) else {
        // Only used when the outer gate already marked the turn Completed.
        // Blocked { reason: None } must not auto-queue.
        return false;
    };
    // Deny production constants that must never auto-queue (true handoffs).
    // RECOVERY_CONTRACT_VIOLATION_REASON: "...final tool-free synthesis pass...attempted more tool calls."
    // PENDING_VERIFICATION_BLOCK_REASON: "...verification is still pending."
    // POST_TOOL_CONTEXT_COMPACTION_FAILED_REASON: "context exceeded...compaction could not reduce"
    // STALE_APPROVED_PLAN_PAUSE_BLOCK_REASON: "stale recovery state"
    // UNMATCHED_TOOL_RESULT / planning interview-approval handoffs / permission / safety fuse.
    if contains_any_token(&reason, RECOVERABLE_BLOCK_BASE_DENY_TOKENS)
        || contains_any_token(&reason, TRACKER_AUTO_CONTINUE_EXTRA_DENY_TOKENS)
    {
        return false;
    }
    matches_recoverable_block_shape(&reason)
}

/// Pure gate for outer-loop tracker auto-continue after a turn end.
///
/// `auto_continue_enabled` is the raw kill-switch (do not pre-AND
/// `planning_active` — this gate owns that check).
/// `final_text_is_safety_handoff` is true when the turn's final assistant
/// text is a permission/policy/safety handoff — those never auto-queue, even
/// on `Completed` ends with incomplete tracker work.
/// `final_text_requires_user_input` is true when the final text asks the user
/// a genuine question/decision — Completed ends must not auto-queue past the ask.
///
/// Completed turns require incomplete tracker work. Recoverable **blocked**
/// ends continue even without tracker items: session evidence shows
/// recovery-fallback and blocked-tool fuse turns parking at the user
/// `Continue…` prompt in build/auto when no tracker is active.
pub(crate) fn should_queue_tracker_auto_continue(
    auto_continue_enabled: bool,
    planning_active: bool,
    turn_completed: bool,
    blocked_reason: Option<&str>,
    is_verification_block: bool,
    incomplete_items: Option<&[String]>,
    cross_turn_turns: u8,
    final_text_is_safety_handoff: bool,
    final_text_requires_user_input: bool,
) -> bool {
    if !auto_continue_enabled || planning_active || cross_turn_turns == 0 {
        return false;
    }
    if final_text_is_safety_handoff || final_text_requires_user_input {
        return false;
    }
    if turn_completed {
        return incomplete_items.is_some_and(|items| !items.is_empty());
    }
    if is_verification_block || blocked_reason.is_none() {
        return false;
    }
    tracker_auto_continue_is_recoverable_block(blocked_reason)
}

/// Pure gate for resume auto-queue of incomplete tracker work.
pub(crate) fn should_queue_tracker_resume_continuation(
    auto_continue_enabled: bool,
    cross_turn_turns: u8,
    incomplete_items: Option<&[String]>,
) -> bool {
    auto_continue_enabled && cross_turn_turns > 0 && incomplete_items.is_some_and(|items| !items.is_empty())
}

/// Pure gate for plan-mode outer auto-continue.
///
/// Continues incomplete planning only on recoverable **blocked** ends when the
/// plan is not yet ready for approval. Ordinary completed planning turns are
/// never auto-continued (they may be interview or approval handoffs). Never
/// auto-approves.
///
/// `consecutive_empty_fallbacks` breaks the empty-turn self-loop: turns that
/// end with the deterministic `PLANNING_COMPLETED_FALLBACK_RESPONSE` (no LLM
/// synthesis, no tool activity) must not re-queue forever. After
/// [`MAX_PLAN_EMPTY_FALLBACK_AUTO_CONTINUE`] consecutive empties the gate
/// closes and the user must `continue` manually.
pub(crate) const MAX_PLAN_EMPTY_FALLBACK_AUTO_CONTINUE: u8 = 2;

/// Stable marker of the deterministic empty-turn fallback text in
/// `turn_loop::PLANNING_COMPLETED_FALLBACK_RESPONSE`. Matched by substring so
/// the gate stays pure (no cross-module constant import) and robust to
/// surrounding file-list appends.
pub(crate) fn is_plan_empty_fallback_text(text: &str) -> bool {
    text.contains("without a final plan synthesis")
        && text.contains("the next turn can reuse it without re-reading files")
}

pub(crate) fn should_queue_plan_mode_auto_continue(
    auto_continue_enabled: bool,
    planning_active: bool,
    plan_ready_for_approval: bool,
    turn_completed: bool,
    blocked_reason: Option<&str>,
    is_verification_block: bool,
    cross_turn_turns: u8,
    consecutive_empty_fallbacks: u8,
) -> bool {
    if !auto_continue_enabled || !planning_active || cross_turn_turns == 0 {
        return false;
    }
    if plan_ready_for_approval || is_verification_block || turn_completed {
        return false;
    }
    if consecutive_empty_fallbacks >= MAX_PLAN_EMPTY_FALLBACK_AUTO_CONTINUE {
        return false;
    }
    blocked_reason.is_some_and(plan_mode_recoverable_block)
}

/// Recoverable planning blocked-reason classifier.
///
/// Allow-list is evaluated **first**. Production
/// `PLANNING_COMPLETED_TURN_FALLBACK_REASON` ("Planning turn ended via
/// recovery fallback … approval-ready plan …") must auto-queue; a deny-first
/// match on "planning turn ended" / "approval-ready plan" incorrectly blocked
/// that path and forced a user `continue` nudge. Interview/approval/permission
/// handoffs stay denied after the allow-list misses.
pub(crate) fn plan_mode_recoverable_block(reason: &str) -> bool {
    // Refusals never auto-continue; see `tracker_auto_continue_is_recoverable_block`.
    if refusal::is_refusal_notice(reason) {
        return false;
    }
    let lower = reason.to_ascii_lowercase();
    // True handoffs deny even when recovery/budget tokens are also present
    // (compound reasons must not auto-queue past a permission/interview wait).
    // "awaiting" is deliberately broader than the tracker's "awaiting approval".
    if contains_any_token(&lower, RECOVERABLE_BLOCK_BASE_DENY_TOKENS) || lower.contains("awaiting") {
        return false;
    }
    // Production recovery constants (including PLANNING_COMPLETED_TURN_FALLBACK_REASON)
    // are recoverable. Deny tokens like "planning turn ended" / "approval-ready
    // plan" must not shadow "recovery fallback".
    //
    // Entry-turn Blocked shapes common after mid-turn `start_planning`: the
    // model hits the read-only gate (blocked-tool fuse) or ends tools without a
    // published final. Both must auto-continue planning research instead of
    // parking at the user `Continue…` prompt.
    if matches_recoverable_block_shape(&lower) {
        return true;
    }
    // Remaining planning handoffs (interview/approval without recovery tokens).
    if lower.contains("planning turn ended") || lower.contains("approval-ready plan") {
        return false;
    }
    false
}

/// User-facing plan progress line (title + phase/status only).
pub(crate) fn plan_progress_line(
    title: &str,
    ready_for_approval: bool,
    open_decisions: usize,
    step_count: usize,
) -> String {
    let label = title.trim();
    let name = if label.is_empty() { None } else { Some(label) };
    if ready_for_approval {
        return match name {
            Some(name) if step_count > 0 => format!("• Plan {name} — ready for approval ({step_count} steps)"),
            Some(name) => format!("• Plan {name} — ready for approval"),
            None if step_count > 0 => format!("• Plan — ready for approval ({step_count} steps)"),
            None => "• Plan — ready for approval".to_string(),
        };
    }
    if open_decisions > 0 {
        return match name {
            Some(name) => format!("• Plan {name} — open decisions: {open_decisions}"),
            None => format!("• Plan — open decisions: {open_decisions}"),
        };
    }
    match name {
        Some(name) => format!("• Plan {name} — research/synthesis"),
        None => "• Plan — research/synthesis".to_string(),
    }
}

/// Stable opening marker for the harness-generated plan-mode auto-continue
/// directive. The planning exit trigger treats any user message carrying this
/// marker as machine-generated (not a genuine user turn), so the two sites
/// must share one literal instead of drifting.
pub(crate) const PLAN_MODE_AUTO_CONTINUE_MARKER: &str = "Plan-mode auto-continue:";

/// Shared tail of every plan-mode continuation message. States the
/// consequence (planning stays read-only, the harness resumed the turn, code
/// changes wait for approval) and the next step. It deliberately contains the
/// stay phrase `continue planning` and no implementation cue, so even without
/// the [`PLAN_MODE_AUTO_CONTINUE_MARKER`] guard it could never read as an
/// exit-and-implement intent.
const PLAN_MODE_CONTINUE_DIRECTIVE_TAIL: &str = "Planning stays active and read-only, so the next step is to continue \
planning: read-only research and synthesis toward one compact `<proposed_plan>`. The harness queued this \
continuation, so no user reply is needed, and code changes wait for plan approval.";

/// Follow-up prompt for plan-mode auto-continue turns.
pub(crate) fn plan_mode_continue_follow_up() -> String {
    format!(
        "{PLAN_MODE_AUTO_CONTINUE_MARKER} no validated persisted plan is ready for approval yet. \
{PLAN_MODE_CONTINUE_DIRECTIVE_TAIL}"
    )
}

/// System directive paired with an in-session plan-mode auto-continue.
pub(crate) fn plan_mode_auto_continue_directive() -> String {
    format!(
        "{PLAN_MODE_AUTO_CONTINUE_MARKER} planning remains active and no validated plan is ready for approval. \
{PLAN_MODE_CONTINUE_DIRECTIVE_TAIL}"
    )
}

/// System directive paired with a plan-mode continuation queued on session
/// resume after a recoverable blocked handoff.
pub(crate) fn plan_mode_resume_directive() -> String {
    format!(
        "Resume continuation: planning remains active after a recoverable blocked handoff. \
{PLAN_MODE_CONTINUE_DIRECTIVE_TAIL}"
    )
}

#[cfg(test)]
mod tracker_continue_tests;

/// Whether the harness may execute the project verifier itself when the model
/// exhausts its directive retries. Kill-switch:
/// `[agent.harness.verification].auto_execute = false` restores
/// directive-only recovery.
pub(crate) fn verification_auto_execute_enabled(vt_cfg: Option<&vtcode_core::config::loader::VTCodeConfig>) -> bool {
    vt_cfg.map(|cfg| cfg.agent.harness.verification.auto_execute).unwrap_or(true)
}

/// Effective consecutive-failure escalation threshold, honoring
/// `[agent.harness.verification].max_consecutive_failures`.
pub(crate) fn verification_max_consecutive_failures(vt_cfg: Option<&vtcode_core::config::loader::VTCodeConfig>) -> u8 {
    vt_cfg
        .map(|cfg| cfg.agent.harness.verification.max_consecutive_failures)
        .unwrap_or(MAX_VERIFICATION_CONSECUTIVE_FAILURES)
}

/// Resolve the verifier command the harness should run itself: the explicit
/// `[agent.harness.verification].default_verifier_override` first (validated
/// as a standalone verifier or pure `&&` chain — anything else falls back to
/// detection so a misconfigured override can never smuggle a mutation or a
/// status-masking pipeline into autonomous execution), then
/// [`vtcode_core::tools::tool_intent::default_verifier_for_workspace`].
/// Returns `None` when neither yields a runnable verifier; callers must then
/// fall through to the manual blocked handoff.
pub(crate) fn resolve_harness_verifier_command(
    vt_cfg: Option<&vtcode_core::config::loader::VTCodeConfig>,
    workspace_root: &Path,
) -> Option<String> {
    if let Some(override_command) = vt_cfg
        .and_then(|cfg| cfg.agent.harness.verification.default_verifier_override.as_deref())
        .map(str::trim)
        .filter(|command| !command.is_empty())
    {
        let args = serde_json::json!({"cmd": override_command});
        if matches!(
            classify_shell_activity(vtcode_core::config::constants::tools::EXEC_COMMAND, &args),
            ShellActivity::Verification
        ) {
            return Some(override_command.to_string());
        }
        tracing::warn!(
            override_command,
            "Ignoring [agent.harness.verification].default_verifier_override: not a standalone verifier or pure && chain; falling back to workspace detection"
        );
    }
    vtcode_core::tools::tool_intent::default_verifier_for_workspace(workspace_root)
}

/// Threshold: number of consecutive read/search operations before the Navigation
/// Loop warning fires.
pub(crate) const NAVIGATION_LOOP_THRESHOLD: usize = 15;

/// Trip count for same-binary, same-root directory listings (`ls`/`find`/`fd`)
/// before the turn balancer schedules recovery. Each binary+root pair keeps
/// its own coarse family (`exec::inspection::<base>::<root>`) across argument
/// variations, so rescanning one target three times signals churn even when
/// no exact request repeats, while scans of distinct trees stay below the
/// tripwire (legitimate exploration).
pub(crate) const LISTING_LOOP_TRIP_COUNT: usize = 3;

/// Planning listing tripwire: planning owns dedicated convergence guards (6
/// consecutive / 10 total low-signal), so three
/// successful listings are legitimate exploration there rather than churn.
pub(crate) const PLANNING_LISTING_LOOP_TRIP_COUNT: usize = 5;

/// Planning recovery thresholds for low-signal navigation. These are kept
/// below the hard planning tool-call ceiling so the model gets one bounded,
/// tool-free synthesis pass while the evidence is still useful.
pub(crate) const PLANNING_CONSECUTIVE_LOW_SIGNAL_THRESHOLD: u8 = 6;
pub(crate) const PLANNING_TOTAL_LOW_SIGNAL_THRESHOLD: u8 = 10;
/// Execution-mode total low-signal guard. Planning converges via its adaptive
/// thresholds (6 consecutive / 10 total); execution mode previously converged
/// only through per-family repeats, the 15-step navigation loop, or the final
/// balancer window, so diverse churn (a new query each time) ran until the
/// turn budget. The counter's window resets on any mutation or verification,
/// so this fires only for churn uninterrupted by productive work.
pub(crate) const EXECUTION_TOTAL_LOW_SIGNAL_THRESHOLD: u8 = 12;

/// Optimized loop detection with bounded signature keys and exponential backoff.
pub(crate) struct LoopTracker {
    attempts: FxHashMap<String, (usize, Instant)>,
    low_signal_attempts: FxHashMap<String, (usize, Instant)>,
    coarse_inspection_attempts: FxHashMap<String, (usize, Instant)>,
    /// Counter for consecutive mutating file operations without execution/verification
    pub consecutive_mutations: usize,
    /// True after the mutation threshold until a verification command completes.
    pub verification_pending: bool,
    /// Bounded fix-up edits allowed while verification stays pending.
    /// Set to [`FAILED_VERIFICATION_FIX_ALLOWANCE`] after a failed verifier so
    /// a broken build can be repaired; consumed by successful fix-up mutations.
    /// Persisted in `SessionStats` so `continue` turns keep the same window.
    pub fix_edits_remaining: u8,
    /// Prevent repeated warning output while verification remains pending.
    pub verification_warning_emitted: bool,
    /// Prevent repeated inline block notices for a single verification checkpoint.
    pub verification_block_notice_emitted: bool,
    /// Set when a pending gate's verifier result was lost (verifier-level
    /// Failure/Timeout, or a lost exec session). Consumed once by the
    /// tool-outcome handlers so the lost-result directive is surfaced after
    /// the tool response lands.
    pub verification_result_lost_notice_pending: bool,
    /// Set when an admitted piped verifier (e.g. `cargo check 2>&1 | tail -5`)
    /// succeeded while the gate was pending. A pipeline's exit status cannot
    /// clear the gate, and without feedback the piped success reads as
    /// "verified" to the model. Consumed once by the tool-outcome handlers;
    /// never persisted in [`Self::verification_snapshot`] because it is
    /// turn-scoped coaching, not gate state.
    pub piped_verification_notice_pending: bool,
    /// Bounded in-turn autonomous recovery attempts consumed when the model
    /// emits text instead of a verifier while the gate is pending. Turn-scoped
    /// (reset each turn, cleared on verification success); never persisted in
    /// [`Self::verification_snapshot`] because it is retry budget, not gate
    /// state. Survives [`Self::reset_after_balancer_recovery`] like the other
    /// verification fields — navigation recovery is not verification.
    pub verification_auto_recovery_attempts: u8,
    /// Whether the harness has already executed the project verifier itself
    /// this turn (one shot per turn). Set when the auto-execute path fires,
    /// regardless of outcome, so a failing verifier cannot trigger an
    /// unbounded execute→text→execute cycle inside one turn. Cleared on
    /// verification success with the rest of the gate; never persisted.
    pub auto_verification_executed: bool,
    /// Live verifier identity for this turn; a running response is not a verdict.
    pub pending_verifier_session_id: Option<String>,
    /// Counter for consecutive read/search operations without action or synthesis
    pub consecutive_navigations: usize,
    /// Number of times navigation-loop recovery has fired in this session.
    pub navigation_loop_recoveries: usize,
    /// Consecutive low-signal navigation outcomes in this turn.
    pub consecutive_low_signal_navigations: u8,
    /// Total low-signal navigation outcomes in this turn.
    pub total_low_signal_navigations: u8,
    /// Lifetime low-signal outcomes for checkpoint diagnostics. Unlike the
    /// adaptive window counters, this never resets within the turn.
    pub low_signal_tool_calls: u32,
    /// At most one adaptive planning synthesis pass is scheduled per turn.
    pub planning_low_signal_synthesis_triggered: bool,
    /// At most one execution-mode total low-signal synthesis pass per turn.
    /// Like the planning latch, this survives [`Self::reset_after_balancer_recovery`].
    pub execution_total_low_signal_triggered: bool,
    /// Unique normalized navigation signatures in the current consecutive
    /// window. Non-semantic output controls (for example, a preview budget)
    /// must not make the same inspection look like a new request.
    nav_signatures: FxHashSet<String>,
}

impl LoopTracker {
    pub(crate) fn new() -> Self {
        Self {
            attempts: FxHashMap::with_capacity_and_hasher(16, Default::default()),
            low_signal_attempts: FxHashMap::with_capacity_and_hasher(8, Default::default()),
            coarse_inspection_attempts: FxHashMap::with_capacity_and_hasher(8, Default::default()),
            consecutive_mutations: 0,
            verification_pending: false,
            fix_edits_remaining: 0,
            verification_warning_emitted: false,
            verification_block_notice_emitted: false,
            verification_result_lost_notice_pending: false,
            piped_verification_notice_pending: false,
            verification_auto_recovery_attempts: 0,
            auto_verification_executed: false,
            pending_verifier_session_id: None,
            consecutive_navigations: 0,
            navigation_loop_recoveries: 0,
            consecutive_low_signal_navigations: 0,
            total_low_signal_navigations: 0,
            low_signal_tool_calls: 0,
            planning_low_signal_synthesis_triggered: false,
            execution_total_low_signal_triggered: false,
            nav_signatures: FxHashSet::default(),
        }
    }

    /// Tuple counterpart to `SessionStats::verification_snapshot`, so turn
    /// setup and persistence share one call shape instead of threading two
    /// loosely-coupled halves across five call sites. A zero-pending snapshot
    /// never carries fix-ups; the clamp keeps a stale caller from building an
    /// inconsistent gate.
    pub(crate) fn with_verification_snapshot(snapshot: (bool, u8)) -> Self {
        let mut tracker = Self::new();
        tracker.verification_pending = snapshot.0;
        tracker.fix_edits_remaining = if snapshot.0 { snapshot.1 } else { 0 };
        tracker
    }

    /// Record an attempt and return the count
    pub(crate) fn record(&mut self, signature: String) -> usize {
        let entry = self.attempts.entry(signature).or_insert((0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
        entry.0
    }

    fn record_low_signal(&mut self, signature: String) -> usize {
        let entry = self.low_signal_attempts.entry(signature).or_insert((0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
        entry.0
    }

    /// Get the maximum repetition count, optionally filtering by a predicate on the signature
    pub(crate) fn max_count_filtered<F>(&self, exclude: F) -> usize
    where
        F: Fn(&str) -> bool,
    {
        self.attempts
            .iter()
            .filter_map(|(sig, (count, _))| if exclude(sig) { None } else { Some(*count) })
            .max()
            .unwrap_or(0)
    }

    pub(crate) fn max_low_signal_count(&self) -> usize {
        self.low_signal_attempts.values().map(|(count, _)| *count).max().unwrap_or(0)
    }

    /// Highest repeat count among coarse directory-listing families
    /// (`exec::inspection::<base>::<root>` with base `ls`/`find`/`fd`), taking
    /// the max across binary+root pairs. Repeated bare listings of the same
    /// target carry no new semantic question (unlike distinct `rg`/`grep`
    /// queries or scans of different trees), so one tool rescanning a root
    /// three times counts as loop churn even when every command string
    /// differs. Mixed targets (`ls src` + `ls crates` + `ls tests`) stay
    /// below the trip count: each root is tracked in its own family.
    pub(crate) fn max_coarse_listing_count(&self) -> usize {
        self.coarse_inspection_attempts
            .iter()
            .filter_map(|(family, (count, _))| {
                // Family shape: exec::inspection::<base>::<root>
                family
                    .split("::")
                    .nth(2)
                    .is_some_and(|base| matches!(base, "ls" | "find" | "fd"))
                    .then_some(*count)
            })
            .max()
            .unwrap_or(0)
    }

    /// Dominant churn signature for recovery-reason annotations: the highest
    /// repeat count across the low-signal ledger and the coarse listing
    /// ledger, preferring whichever is larger so listing-triggered recovery
    /// still names the looped family even though the low-signal promotion
    /// only records the final repeat. Returns owned data so callers can keep
    /// the annotation alive while mutating the tracker.
    pub(crate) fn dominant_churn(&self) -> Option<(String, usize)> {
        let low_signal = self
            .low_signal_attempts
            .iter()
            .max_by_key(|(_, (count, _))| *count)
            .map(|(family, (count, _))| (family.clone(), *count));
        let coarse = self
            .coarse_inspection_attempts
            .iter()
            .max_by_key(|(_, (count, _))| *count)
            .map(|(family, (count, _))| (family.clone(), *count));
        match (low_signal, coarse) {
            (Some(low), Some(coarse)) if coarse.1 > low.1 => Some(coarse),
            (Some(low), _) => Some(low),
            (None, Some(coarse)) => Some(coarse),
            (None, None) => None,
        }
    }

    /// Number of redundant navigations (total - unique) in the current window.
    /// The navigation-loop guard requires at least 3 redundant requests.
    pub(crate) fn repeated_navigation_count(&self) -> usize {
        self.consecutive_navigations.saturating_sub(self.nav_signatures.len())
    }

    fn reset_low_signal_attempts(&mut self) {
        self.low_signal_attempts.clear();
        self.coarse_inspection_attempts.clear();
    }

    fn reset_low_signal_navigation_counters(&mut self) {
        self.consecutive_low_signal_navigations = 0;
        self.total_low_signal_navigations = 0;
    }

    /// Clear the per-turn navigation window after a non-navigation tool.
    /// Callers pass `low_signal_family.is_none()` so diverse productive reads
    /// keep their repetition history while low-signal churn resets.
    fn reset_navigation_window(&mut self, clear_low_signal_attempts: bool) {
        self.consecutive_navigations = 0;
        self.nav_signatures.clear();
        self.reset_low_signal_navigation_counters();
        if clear_low_signal_attempts {
            self.reset_low_signal_attempts();
        }
    }

    fn record_navigation_signal(&mut self, is_low_signal: bool) {
        if is_low_signal {
            self.low_signal_tool_calls = self.low_signal_tool_calls.saturating_add(1);
            self.consecutive_low_signal_navigations = self.consecutive_low_signal_navigations.saturating_add(1);
            self.total_low_signal_navigations = self.total_low_signal_navigations.saturating_add(1);
        } else {
            // Productive inspection breaks only the consecutive streak. The
            // total remains turn-scoped so diverse empty searches still
            // converge on synthesis.
            self.consecutive_low_signal_navigations = 0;
        }
    }

    pub(crate) fn reset_after_balancer_recovery(&mut self) {
        self.attempts.clear();
        self.reset_low_signal_attempts();
        self.nav_signatures.clear();
        // Navigation recovery is not verification. Preserve mutation pressure,
        // an active verification checkpoint, its bounded fix window, the
        // in-turn auto-recovery budget, the harness-executed once-flag, and
        // the associated one-shot notices. Only a successful standalone
        // verifier may clear those fields.
        self.consecutive_navigations = 0;
        self.reset_low_signal_navigation_counters();
    }

    pub(crate) fn verification_is_pending(&self) -> bool {
        self.verification_pending || self.consecutive_mutations >= BLIND_EDITING_THRESHOLD
    }

    /// Snapshot the session-persisted gate state for `SessionStats`.
    /// Persist both halves together so resumed turns reconstruct the same
    /// gate instead of drifting (a pending gate with a lost fix window
    /// deadlocks a broken build).
    pub(crate) fn verification_snapshot(&self) -> (bool, u8) {
        (self.verification_is_pending(), self.fix_edits_remaining)
    }

    pub(crate) fn mark_verification_pending(&mut self) {
        self.verification_pending = true;
    }

    /// One-shot accessor for the lost-verification-result notice queued by
    /// [`update_repetition_tracker`]. Handlers consume it after the tool
    /// response lands so the directive never splits an assistant batch.
    pub(crate) fn take_verification_result_lost_notice(&mut self) -> bool {
        std::mem::take(&mut self.verification_result_lost_notice_pending)
    }

    /// One-shot accessor for the piped-verifier notice queued by
    /// [`update_repetition_tracker`]. Handlers consume it after the tool
    /// response lands so the directive never splits an assistant batch.
    pub(crate) fn take_piped_verification_notice(&mut self) -> bool {
        std::mem::take(&mut self.piped_verification_notice_pending)
    }

    /// Grant a bounded fix-up window after a failed verifier. The gate stays
    /// pending (completion still requires a successful standalone verifier),
    /// but the next [`FAILED_VERIFICATION_FIX_ALLOWANCE`] successful mutations
    /// are admitted so a broken build can be repaired instead of deadlocking.
    pub(crate) fn record_failed_verification(&mut self) {
        self.pending_verifier_session_id = None;
        self.verification_pending = true;
        self.fix_edits_remaining = FAILED_VERIFICATION_FIX_ALLOWANCE;
    }

    fn record_successful_mutation(&mut self) {
        // Consume the fix-up window first: repair edits must not grow the
        // blind-editing counter while the gate already requires re-verify.
        if self.verification_pending && self.fix_edits_remaining > 0 {
            self.fix_edits_remaining = self.fix_edits_remaining.saturating_sub(1);
            return;
        }
        self.consecutive_mutations = self.consecutive_mutations.saturating_add(1);
        if self.consecutive_mutations >= BLIND_EDITING_THRESHOLD {
            self.verification_pending = true;
        }
    }

    /// Config-aware attempt recorder; prefer it at call sites with workspace
    /// config access so `[agent.harness.verification].in_turn_attempts` is
    /// honored. Returns `true` when budget remained (the caller should reset
    /// the text-response streak, inject the project-aware recovery directive,
    /// and `Continue` the turn instead of `Block`ing); `false` once exhausted.
    pub(crate) fn record_verification_auto_recovery_with_limit(&mut self, max_attempts: u8) -> bool {
        if self.verification_auto_recovery_attempts >= max_attempts {
            return false;
        }
        self.verification_auto_recovery_attempts = self.verification_auto_recovery_attempts.saturating_add(1);
        true
    }

    pub(crate) fn verification_auto_recovery_attempts(&self) -> u8 {
        self.verification_auto_recovery_attempts
    }

    /// Whether the harness may execute the project verifier itself this turn.
    /// One shot per turn: once fired (any outcome), further text responses
    /// consume only directive retries, then the turn blocks for cross-turn
    /// recovery. Never true when the gate is clear.
    pub(crate) fn should_auto_execute_verifier(&self) -> bool {
        self.verification_is_pending()
            && (!self.auto_verification_executed || self.pending_verifier_session_id.is_some())
    }

    /// Record that the harness executed the project verifier itself this
    /// turn. Unconditional: every outcome (success, failure, lost result)
    /// flows through the normal tracker paths, which clear or preserve the
    /// gate; the flag only prevents a second harness execution this turn.
    pub(crate) fn record_auto_verification_executed(&mut self) {
        self.auto_verification_executed = true;
    }

    fn mark_verification_complete(&mut self) {
        self.pending_verifier_session_id = None;
        self.consecutive_mutations = 0;
        self.verification_pending = false;
        self.fix_edits_remaining = 0;
        self.verification_warning_emitted = false;
        self.verification_block_notice_emitted = false;
        self.verification_result_lost_notice_pending = false;
        self.piped_verification_notice_pending = false;
        self.verification_auto_recovery_attempts = 0;
        self.auto_verification_executed = false;
    }
}

/// Check if an identical tool call (same name + same args) was already executed
/// recently in the working history. Returns the output of the most recent
/// matching tool response if found.
///
/// This catches cross-turn duplicates that the per-turn `LoopTracker` misses
/// because it is reset at the start of each turn. Scans the last
/// `MAX_HISTORY_SCAN` messages to keep the check bounded.
///
/// File-read pagination is normalised so that re-reading the same file with a
/// different `offset` or `limit` is recognised as the same logical read.
/// `code_search` uses a separate replay identity that retains the effective
/// `max_results`; its loop identity is separate.
///
/// Tool-call IDs are scoped to the nearest preceding Assistant batch. A later
/// batch may reuse an ID for another tool, so both the batch and tool name must
/// match before its Tool response can satisfy this replay lookup.
pub(crate) fn find_duplicate_in_history(
    history: &[uni::Message],
    tool_name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
) -> Option<String> {
    const MAX_HISTORY_SCAN: usize = 120;
    let target_signature = read_normalized_signature_key(tool_name, args);

    let scan_start = history.len().saturating_sub(MAX_HISTORY_SCAN);
    let target_tool_name = canonical_tool_name(tool_name);
    let mut current_batch: FxHashMap<String, (String, serde_json::Value)> = FxHashMap::default();
    let mut matching_responses = Vec::new();

    for (offset, msg) in history[scan_start..].iter().enumerate() {
        let abs_idx = scan_start + offset;
        match msg.role {
            uni::MessageRole::Assistant => {
                current_batch.clear();
                if let Some(ref tool_calls) = msg.tool_calls {
                    for tc in tool_calls {
                        if let Some(ref func) = tc.function {
                            let tc_args: serde_json::Value = serde_json::from_str(&func.arguments)
                                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
                            current_batch.insert(tc.id.clone(), (canonical_tool_name(&func.name).to_string(), tc_args));
                        }
                    }
                }
            }
            uni::MessageRole::Tool => {
                let Some(call_id) = msg.tool_call_id.as_deref() else {
                    continue;
                };
                let Some((batch_tool_name, tc_args)) = current_batch.get(call_id) else {
                    continue;
                };
                if batch_tool_name == target_tool_name
                    && read_normalized_signature_key(batch_tool_name, tc_args) == target_signature
                    && read_extent::extent_covers(tc_args, args)
                    && tool_response_is_replayable(msg)
                {
                    matching_responses.push((abs_idx, tc_args.clone(), msg));
                }
            }
            _ => {}
        }
    }

    for (response_index, tc_args, msg) in matching_responses.into_iter().rev() {
        let invalidated = tool_name == vtcode_core::config::constants::tools::CODE_SEARCH
            && history_has_scoped_mutation_after(history, response_index, &tc_args, workspace_root);
        if !invalidated {
            return Some(msg.content.as_text().to_string());
        }
    }
    None
}

fn tool_response_is_replayable(message: &uni::Message) -> bool {
    let content = message.content.as_text();
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.len() > 128 * 1024 {
        return false;
    }

    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Object(output)) => {
            if output.contains_key("error") || output.contains_key("error_type") || output.contains_key("failure_kind")
            {
                return false;
            }
            if output.get("blocked").and_then(serde_json::Value::as_bool) == Some(true)
                || output.get("verification_required").and_then(serde_json::Value::as_bool) == Some(true)
            {
                return false;
            }
            if matches!(output.get("success"), Some(serde_json::Value::Bool(false)) | Some(serde_json::Value::Null)) {
                return false;
            }
            if output.get("success").is_some_and(|value| !value.is_boolean()) {
                return false;
            }
            !output.get("status").and_then(serde_json::Value::as_str).is_some_and(|status| {
                matches!(
                    status.to_ascii_lowercase().as_str(),
                    "failed"
                        | "failure"
                        | "error"
                        | "denied"
                        | "permission_denied"
                        | "rejected"
                        | "timeout"
                        | "timed_out"
                        | "cancelled"
                        | "canceled"
                        | "interrupted"
                        | "aborted"
                        | "blocked"
                        | "skipped"
                        | "not_started"
                        | "not_executed"
                        | "pending"
                        | "in_progress"
                        | "not_run"
                )
            })
        }
        Ok(serde_json::Value::String(value)) => text_response_is_replayable(&value),
        Ok(serde_json::Value::Array(_) | serde_json::Value::Number(_) | serde_json::Value::Bool(_)) => true,
        Ok(serde_json::Value::Null) => false,
        Err(_) => text_response_is_replayable(trimmed),
    }
}

fn text_response_is_replayable(content: &str) -> bool {
    let trimmed = content.trim();
    const FAILURE_PREFIXES: &[&str] = &[
        "error:",
        "execution denied",
        "permission denied",
        "timeout",
        "timed out",
        "cancelled",
        "canceled",
        "failed",
        "failure",
        "denied",
        "rejected",
        "blocked",
        "aborted",
        "interrupted",
        "skipped",
        "not started",
        "not executed",
        "not run",
        "pending",
        "in progress",
    ];
    !FAILURE_PREFIXES.iter().any(|prefix| {
        trimmed
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
    })
}

fn history_has_scoped_mutation_after(
    history: &[uni::Message],
    response_index: usize,
    search_args: &serde_json::Value,
    workspace_root: &Path,
) -> bool {
    let mut pending_mutations: FxHashMap<String, Vec<PathBuf>> = FxHashMap::default();
    for message in history.iter().skip(response_index.saturating_add(1)) {
        match message.role {
            uni::MessageRole::Assistant => {
                // Tool-call IDs are scoped to one Assistant batch and may be
                // reused later. Unanswered calls from an earlier batch were
                // never executed, so they must not survive this boundary.
                pending_mutations.clear();
                let Some(tool_calls) = message.tool_calls.as_ref() else {
                    continue;
                };
                for tool_call in tool_calls {
                    let Some(function) = tool_call.function.as_ref() else {
                        continue;
                    };
                    let Ok(args) = serde_json::from_str::<serde_json::Value>(&function.arguments) else {
                        continue;
                    };
                    if !vtcode_core::tools::tool_intent::classify_tool_intent(&function.name, &args).mutating {
                        continue;
                    }
                    let paths = vtcode_core::tools::mutation_target_paths(&function.name, &args);
                    if !paths.is_empty() {
                        pending_mutations.insert(tool_call.id.clone(), paths);
                    }
                }
            }
            uni::MessageRole::Tool => {
                let Some(call_id) = message.tool_call_id.as_deref() else {
                    continue;
                };
                let Some(paths) = pending_mutations.remove(call_id) else {
                    continue;
                };
                if tool_response_is_success(message)
                    && paths.iter().any(|path| {
                        vtcode_core::tools::code_search_scope_contains_mutated_path(search_args, path, workspace_root)
                    })
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn tool_response_is_success(message: &uni::Message) -> bool {
    let Ok(output) = serde_json::from_str::<serde_json::Value>(&message.content.as_text()) else {
        return false;
    };
    let Some(output) = output.as_object() else {
        return false;
    };
    if output.contains_key("error") || output.contains_key("error_type") || output.contains_key("failure_kind") {
        return false;
    }
    if output.get("status").is_some_and(|status| status.as_str() != Some("success")) {
        return false;
    }

    match output.get("success") {
        Some(serde_json::Value::Bool(success)) => *success,
        Some(_) => false,
        None => output
            .get("status")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|status| status == "success"),
    }
}

fn output_has_empty_search_results(output: &serde_json::Value) -> bool {
    output
        .get("results")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|results| results.is_empty())
        && !output_has_actionable_recovery_guidance(output)
        && !output_has_error_signal(output)
}

fn output_has_actionable_recovery_guidance(output: &serde_json::Value) -> bool {
    ["hint", "next_action", "critical_note", "warning"].iter().any(|key| {
        output
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    }) || output
        .get("fallback_tool")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
        || output.get("hints").and_then(serde_json::Value::as_array).is_some_and(|hints| {
            hints
                .iter()
                .any(|hint| hint.as_str().is_some_and(|value| !value.trim().is_empty()))
        })
}

fn output_has_error_signal(output: &serde_json::Value) -> bool {
    ["error", "error_type", "stderr", "stderr_preview", "message"]
        .iter()
        .any(|key| !output_field_is_empty(output.get(*key)))
}

fn output_reuses_recent_result(output: &serde_json::Value) -> bool {
    [
        "loop_detected",
        "reused_recent_result",
        "spool_ref_only",
        "result_ref_only",
    ]
    .iter()
    .any(|key| output.get(*key).and_then(serde_json::Value::as_bool) == Some(true))
}

fn error_is_missing_resource(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "not found",
        "no such file",
        "resource not found",
        "spool file not found",
        "session output file not found",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Detect the session-loss error text emitted by the exec session manager
/// ("exec session '<id>' not found. ...") and the PTY session manager
/// ("PTY session '<id>' not found"), mirroring the phrasing in `vtcode-core`
/// exec_session and pty session_ops tool errors.
fn error_text_indicates_lost_session(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("not found") && (lower.contains("exec session") || lower.contains("pty session"))
}

/// Return whether `(canonical_name, args)` is a follow-up on an existing
/// exec/PTY session rather than a fresh command run.
///
/// `canonical_name` must already be canonicalized (see [`canonical_tool_name`]).
/// Any non-`run` `unified_exec` action counts as a follow-up
/// (poll/wait/inspect/continue/input, plus list/code/write/close), as does
/// any non-`run` PTY session tool (`read_pty_session`, `send_pty_input`,
/// `close_pty_session`, `list_pty_sessions`, `exec_pty_cmd`): session
/// follow-ups carry a `session_id`, not command text, so they never classify
/// as [`ShellActivity::Verification`]. A missing-session failure on one of
/// them therefore needs its own lost-result branch in
/// [`update_repetition_tracker`]: the verifier output it was waiting on died
/// with the session. Fresh `run` calls are excluded: a run creates its
/// session, so it cannot lose a prior verifier's result.
fn is_session_follow_up(canonical_name: &str, args: &serde_json::Value) -> bool {
    use vtcode_core::config::constants::tools;
    if canonical_name == tools::WRITE_STDIN {
        return true;
    }
    if matches!(
        canonical_name,
        tools::UNIFIED_EXEC
            | tools::READ_PTY_SESSION
            | tools::SEND_PTY_INPUT
            | tools::CLOSE_PTY_SESSION
            | tools::LIST_PTY_SESSIONS
            | tools::EXEC_PTY_CMD
    ) && !vtcode_core::tools::tool_intent::is_command_run_tool_call(canonical_name, args)
    {
        return true;
    }
    false
}

fn is_low_signal_outcome(outcome: &ToolPipelineOutcome, canonical_tool_name: &str, args: &serde_json::Value) -> bool {
    match &outcome.status {
        ToolExecutionStatus::Success { output, command_success, .. } => {
            output_has_empty_search_results(output)
                || output_reuses_recent_result(output)
                || (*command_success && is_empty_shell_search(canonical_tool_name, args, output))
                || (matches!(
                    canonical_tool_name,
                    vtcode_core::config::constants::tools::UNIFIED_EXEC
                        | vtcode_core::config::constants::tools::EXEC_COMMAND
                ) && !*command_success
                    && is_grep_style_no_match(canonical_tool_name, args, output))
        }
        ToolExecutionStatus::Failure { error } => error_is_missing_resource(&error.message),
        ToolExecutionStatus::Timeout { .. } | ToolExecutionStatus::Cancelled => false,
    }
}

/// Coarse inspection family for duplicate-listing detection. Unlike the exact
/// `low_signal_family_key` (full normalized command), this groups overlapping
/// scans such as three `find` invocations over the same tree with different
/// flags, so successful but redundant rescans of one target still count
/// toward diagnostics. The family is scoped by binary AND search root:
/// `ls src` / `find src …` / `ls -1 src` share a target and count together,
/// while scans of distinct trees (`ls src` / `ls crates` / `ls tests`) are
/// legitimate exploration and never group.
fn coarse_inspection_family_key(canonical_tool_name: &str, args: &serde_json::Value) -> Option<String> {
    use vtcode_core::config::constants::tools;
    // Only bare directory listings suffer from overlapping-but-distinct
    // invocations (e.g. three `find` calls over the same tree with different
    // flags) that the exact family key never groups. File reads (`cat`/`head`/
    // `tail` via shell included) and semantic search (`rg`/`grep`, `code_search`)
    // already carry precise family keys; grouping them coarsely would mislabel
    // diverse productive exploration (different files/queries) as looping.
    // In particular `rg`/`grep` must stay out: their first positional is the
    // search pattern, not the search root, so five distinct queries such as
    // `grep -n "enum Commands" ...`, `grep -rn "enum ExecSubcommand" ...`
    // (turn_1303/turn_1304: `exec::inspection::grep::enum ×5`) or five `rg`
    // searches for `pub` (turn_1291: `exec::inspection::rg::pub ×5`, e.g.
    // `rg -n 'pub enum Commands' ...`) all collapse into
    // one coarse family and get promoted to low-signal, tripping early
    // recovery on legitimate research. Distinct patterns/paths keep distinct
    // exact families and converge via the total low-signal guard instead.
    match canonical_tool_name {
        tools::UNIFIED_EXEC | tools::EXEC_COMMAND => {
            let command = vtcode_core::tools::command_args::command_text(args).ok()??;
            let first = command.split_whitespace().next().unwrap_or("");
            let base = first
                .rsplit('/')
                .next()
                .unwrap_or(first)
                .trim_matches(|ch| ch == '\'' || ch == '"')
                .to_ascii_lowercase();
            if matches!(base.as_str(), "find" | "ls" | "fd") {
                Some(format!("exec::inspection::{base}::{}", coarse_inspection_root(&command)))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Extract the search-root segment of a listing command: the first
/// non-flag argument, with surrounding quotes and trailing slashes stripped.
/// Deliberately heuristic — an option value (`ls --width 80 src` → root "80")
/// can be picked up and fragment a family, which only makes detection more
/// conservative. Commands with no positional argument (`ls -la`) scan the
/// working directory and map to ".". Only `ls`/`find`/`fd` reach this helper;
/// `rg`/`grep` are excluded above because their first positional is the
/// search pattern, not a path root.
fn coarse_inspection_root(command: &str) -> String {
    let root = command
        .split_whitespace()
        .skip(1)
        .find(|token| !token.starts_with('-'))
        .unwrap_or(".");
    let root = root.trim_matches(|ch| ch == '\'' || ch == '"').trim_end_matches('/');
    if root.is_empty() { "." } else { root }.to_string()
}

/// Upsert a tool result into `history`, keyed on `tool_call_id`.
///
/// This is a **bounded** upsert: the reverse scan stops as soon as it reaches
/// ANY Assistant message (regardless of its tool_calls). This is critical:
/// Assistant messages represent turn boundaries. Tool responses from before an
/// Assistant must never be overwritten by Tool responses from after it, even
/// when fabricated tool_call_ids collide across turns.
///
/// If a Tool message with a matching id is found *before* the nearest
/// Assistant boundary, it is a legitimate same-call update (e.g. an
/// auto-permission probe replaying a result) and gets overwritten in place.
/// If the boundary is hit first, the id has been reused across turns, so we
/// append instead of clobbering an unrelated, earlier Tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolResponseHistoryUpdate {
    Appended,
    Replaced { previous_text_len: usize },
}

pub(crate) fn push_tool_response<S>(
    history: &mut Vec<uni::Message>,
    tool_call_id: S,
    tool_name: Option<&str>,
    content: String,
) -> ToolResponseHistoryUpdate
where
    S: AsRef<str> + Into<String>,
{
    let tool_call_id_ref = tool_call_id.as_ref();
    let mut overwrite_index = None;
    for (index, message) in history.iter().enumerate().rev() {
        match message.role {
            uni::MessageRole::Tool => {
                if message.tool_call_id.as_deref() == Some(tool_call_id_ref) {
                    overwrite_index = Some(index);
                    break;
                }
            }
            // Stop at ANY Assistant message — it marks a turn boundary.
            // Tool responses from before this Assistant must not be overwritten.
            uni::MessageRole::Assistant => {
                break;
            }
            _ => {}
        }
    }

    if let Some(index) = overwrite_index {
        let previous_text_len = history[index].content.as_text().len();
        history[index].content = uni::MessageContent::Text(content);
        if let Some(tool_name) = tool_name {
            history[index].origin_tool = Some(tool_name.to_string());
        }
        return ToolResponseHistoryUpdate::Replaced { previous_text_len };
    }

    let tool_call_id = tool_call_id.into();
    history.push(match tool_name {
        Some(name) => uni::Message::tool_response_with_origin(tool_call_id, content, name.to_string()),
        None => uni::Message::tool_response(tool_call_id, content),
    });
    ToolResponseHistoryUpdate::Appended
}

/// Generate a tool signature key with predictable structure for loop tracking.
pub(crate) fn signature_key_for(name: &str, args: &serde_json::Value) -> String {
    // Keep keys compact on hot paths: hash bounded argument bytes instead of
    // allocating full JSON payloads for large tool arguments.
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut input_len = 0usize;
    let mutability_tag = if vtcode_core::tools::tool_intent::classify_tool_intent(name, args).mutating {
        "rw"
    } else {
        "ro"
    };

    if serde_json::to_writer(HashingWriter::new(&mut hash, &mut input_len), args).is_err() {
        for byte in b"{}" {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
            input_len = input_len.saturating_add(1);
        }
    }

    format!("{name}:{mutability_tag}:len{input_len}-fnv{hash:016x}")
}

/// Generate a read-normalized signature key for cross-turn dedup.
///
/// File-read tools (`file_operation` with `read` action, `read_file`,
/// `grep_file`, `list_files`) omit pagination and read-offset fields so that
/// re-reading the same target groups under one logical read. `code_search`
/// uses its normalised result-replay identity, which preserves the effective
/// `max_results`; its separate loop identity may group searches across limits.
///
/// For mutating tools the original `signature_key_for` is returned unchanged.
pub(crate) fn read_normalized_signature_key(name: &str, args: &serde_json::Value) -> String {
    if name == vtcode_core::config::constants::tools::CODE_SEARCH
        && let Some(identity) = vtcode_core::tools::normalised_code_search_identity(args)
    {
        return format!("{name}:ro:{identity}");
    }

    if !is_read_only_tool_args(name, args) {
        return signature_key_for(name, args);
    }

    let Some(mut obj) = args.as_object().cloned() else {
        return signature_key_for(name, args);
    };

    // Strip pagination / read-offset fields that don't change *what* is read.
    for key in read_extent::normalization_strip_keys() {
        obj.remove(key);
    }

    let normalized = serde_json::Value::Object(obj);
    signature_key_for(name, &normalized)
}

/// Returns `true` when `(name, args)` describe a read-only tool invocation.
fn is_read_only_tool_args(name: &str, args: &serde_json::Value) -> bool {
    use vtcode_core::config::constants::tools;
    match name {
        tools::READ_FILE | tools::GREP_FILE | tools::LIST_FILES => true,
        tools::CODE_SEARCH => true,
        tools::UNIFIED_SEARCH | "search_dispatch" => true,
        tools::UNIFIED_FILE | "file_operation" => {
            matches!(args.get("action").and_then(|v| v.as_str()), Some("read"))
        }
        _ => false,
    }
}

struct HashingWriter<'a> {
    hash: &'a mut u64,
    input_len: &'a mut usize,
}

impl<'a> HashingWriter<'a> {
    fn new(hash: &'a mut u64, input_len: &'a mut usize) -> Self {
        Self { hash, input_len }
    }
}

impl std::io::Write for HashingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for byte in buf {
            *self.hash ^= u64::from(*byte);
            *self.hash = self.hash.wrapping_mul(0x100000001b3);
            *self.input_len = self.input_len.saturating_add(1);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn resolve_max_tool_retries(
    _tool_name: &str,
    vt_cfg: Option<&vtcode_core::config::loader::VTCodeConfig>,
) -> usize {
    vt_cfg
        .map(|cfg| cfg.agent.harness.max_tool_retries as usize)
        .unwrap_or(vtcode_config::constants::defaults::DEFAULT_MAX_TOOL_RETRIES as usize)
}

fn path_targets_plan_artifact(path: &str) -> bool {
    let normalized = path.trim().replace('\\', "/");
    normalized == ".vtcode/plans"
        || normalized.starts_with(".vtcode/plans/")
        || normalized.contains("/.vtcode/plans/")
        || normalized == "/tmp/vtcode-plans"
        || normalized.starts_with("/tmp/vtcode-plans/")
        || normalized.contains("/tmp/vtcode-plans/")
}

fn path_is_docs_only(path: &str) -> bool {
    let normalized = path.trim().replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    let file_name = lower.rsplit('/').next().unwrap_or_default();
    // Well-known prose filenames count only with a docs extension or no
    // extension at all, so `README.py` and `docs/script.py` stay code edits.
    let stem = file_name.split('.').next().unwrap_or_default();
    if matches!(stem, "readme" | "changelog" | "license") {
        let ext = file_name.rsplit('.').next().unwrap_or_default();
        if ext == stem || matches!(ext, "md" | "mdx" | "txt" | "rst") {
            return true;
        }
    }
    matches!(file_name.rsplit('.').next(), Some("md" | "mdx" | "txt" | "rst"))
}

/// Docs-only writes are low-risk prose edits. They neither trip nor clear the
/// anti-blind gate: while pending they stay allowed (warn-only), and they never
/// increment `consecutive_mutations`. Mixed docs+code patches, empty path sets,
/// and exec-tool mutations fail closed as code edits.
pub(crate) fn is_docs_only_write(name: &str, args: &serde_json::Value) -> bool {
    use vtcode_core::config::constants::tools as tool_names;
    use vtcode_core::tools::names::canonical_tool_name;
    use vtcode_core::tools::tool_intent::file_operation_action;

    let canonical = canonical_tool_name(name);
    let is_file_write = matches!(
        canonical,
        tool_names::APPLY_PATCH
            | tool_names::WRITE_FILE
            | tool_names::EDIT_FILE
            | tool_names::CREATE_FILE
            | tool_names::SEARCH_REPLACE
            | tool_names::DELETE_FILE
            | tool_names::MOVE_FILE
            | tool_names::COPY_FILE
            | tool_names::UNIFIED_FILE
    );
    if !is_file_write {
        return false;
    }
    if canonical == tool_names::UNIFIED_FILE
        && file_operation_action(args)
            .map(|action| action.eq_ignore_ascii_case("read"))
            .unwrap_or(false)
    {
        return false;
    }
    let mut paths = vtcode_core::tools::apply_patch::mutation_target_paths(canonical, args);
    // `mutation_target_paths` SINGULAR_KEYS lacks camelCase `filePath`
    // (covered by `is_plan_artifact_write`); supplement it so the same
    // call is not docs-only via `path` but blocked via `filePath`.
    // Duplicates are harmless: every path must still be docs-only.
    if let Some(extra) = args.get("filePath").and_then(|value| value.as_str()) {
        let trimmed = extra.trim();
        if !trimmed.is_empty() {
            paths.push(PathBuf::from(trimmed));
        }
    }
    if paths.is_empty() {
        return false;
    }
    paths.iter().all(|path| path.to_str().is_some_and(path_is_docs_only))
}

pub(crate) fn is_plan_artifact_write(name: &str, args: &serde_json::Value) -> bool {
    use vtcode_core::config::constants::tools as tool_names;
    use vtcode_core::tools::names::canonical_tool_name;
    use vtcode_core::tools::tool_intent::file_operation_action;

    let canonical = canonical_tool_name(name);
    match canonical {
        tool_names::TASK_TRACKER => true,
        tool_names::UNIFIED_FILE => {
            if !file_operation_action(args)
                .map(|action| action.eq_ignore_ascii_case("read"))
                .unwrap_or(false)
            {
                [
                    "path",
                    "file_path",
                    "filepath",
                    "filePath",
                    "target_path",
                    "destination",
                    "destination_path",
                ]
                .iter()
                .filter_map(|key| args.get(*key).and_then(|value| value.as_str()))
                .any(path_targets_plan_artifact)
            } else {
                false
            }
        }
        tool_names::WRITE_FILE | tool_names::EDIT_FILE | tool_names::CREATE_FILE | tool_names::SEARCH_REPLACE => {
            ["path", "file_path", "filepath", "filePath"]
                .iter()
                .filter_map(|key| args.get(*key).and_then(|value| value.as_str()))
                .any(path_targets_plan_artifact)
        }
        _ => false,
    }
}

fn is_execution_tool(name: &str) -> bool {
    use vtcode_core::config::constants::tools as tool_names;

    matches!(
        name,
        tool_names::UNIFIED_EXEC
            | tool_names::EXEC_COMMAND
            | tool_names::EXEC_PTY_CMD
            | tool_names::RUN_PTY_CMD
            | tool_names::EXECUTE_CODE
            | tool_names::SHELL
    )
}

/// Return whether a tool call must wait for a successful verification step.
///
/// Reads, inspections, verification commands, task tracking, dedicated
/// plan-artifact writes, and docs-only prose writes remain available while the
/// checkpoint is pending.
/// A failed verifier grants a bounded fix-up window ([`FAILED_VERIFICATION_FIX_ALLOWANCE`])
/// so a broken build can be repaired, and piped verifier attempts
/// (e.g. `cargo check 2>&1 | grep error`) are admitted to run even though
/// they cannot clear the gate. A verifier piped only into `head`/`tail` runs
/// as a standalone verifier (see [`shell_args_as_executed`]).
pub(crate) fn mutation_blocked_until_verification(
    loop_tracker: &LoopTracker,
    name: &str,
    args: &serde_json::Value,
) -> bool {
    if !loop_tracker.verification_is_pending() || is_plan_artifact_write(name, args) || is_docs_only_write(name, args) {
        return false;
    }

    let canonical_name = canonical_tool_name(name);
    // Cleanup can only stop/release an owned exec session; it must remain
    // reachable when workspace edits await verification. Permissions still apply.
    if vtcode_core::tools::tool_intent::is_exec_session_cleanup_call(canonical_name, args) {
        return false;
    }
    if is_execution_tool(canonical_name) {
        // Classify the command the kernel will run: a verifier piped only
        // into `head`/`tail` executes standalone and is a verification.
        let executed = shell_args_as_executed(canonical_name, args);
        let activity = classify_shell_activity(canonical_name, &executed);
        // Other piped verifier attempts (`cargo check 2>&1 | grep error`)
        // still run so the model can see the failure; they never clear the
        // gate (see update_repetition_tracker). The admission predicate
        // requires every shell segment to be verification-or-readonly, so a
        // smuggled mutation such as `cargo check && rm -rf target` stays
        // blocked.
        if !matches!(activity, ShellActivity::Mutation) || shell_command_is_admitted_verification_attempt(&executed) {
            return false;
        }
        // Fix-up window: allow bounded repair edits after a failed verifier.
        return loop_tracker.fix_edits_remaining == 0;
    }

    if !vtcode_core::tools::tool_intent::classify_tool_intent(canonical_name, args).mutating {
        return false;
    }
    loop_tracker.fix_edits_remaining == 0
}

/// Updates the tool repetition tracker based on the execution outcome.
///
/// Count completed attempts for repetition detection, but only successful
/// mutations contribute to anti-blind-editing verification pressure.
///
/// Returns `true` when this outcome freshly granted (or refreshed) the
/// failed-verifier fix-up window. Callers must reset the assistant text-response
/// streak so the model gets one diagnostic explanation before the
/// pending-verification text cap re-applies; otherwise a failed build blocks
/// the turn before the agent can describe the failure and use its fix-up edits.
///
/// A `true` return also covers *lost* verifier results: while the gate is
/// pending, a verifier-level Failure/Timeout (or a session follow-up
/// failure — `write_stdin`, a non-run `unified_exec` action, or a PTY
/// session follow-up — reporting a dead exec/PTY session) grants the same
/// bounded window because the verifier never produced an observable
/// verdict. In that case the tracker queues
/// [`VERIFICATION_RESULT_LOST_DIRECTIVE`] for the handlers to surface.
///
/// A `true` return finally covers a *piped* verifier success while the gate
/// is pending (`cargo check 2>&1 | grep error`): the exit status belongs to
/// another command and cannot clear the gate, so the tracker queues
/// [`PIPED_VERIFICATION_DIRECTIVE`] instead of leaving the model to believe
/// the check verified the edits.
pub(crate) fn update_repetition_tracker(
    loop_tracker: &mut LoopTracker,
    outcome: &ToolPipelineOutcome,
    name: &str,
    args: &serde_json::Value,
) -> bool {
    if matches!(&outcome.status, ToolExecutionStatus::Cancelled) {
        return false;
    }

    let canonical_name = canonical_tool_name(name);
    let signature_key = signature_key_for(canonical_name, args);
    loop_tracker.record(signature_key.clone());
    let targets_pending_verifier = loop_tracker
        .pending_verifier_session_id
        .as_deref()
        .is_some_and(|session_id| vtcode_core::tools::command_args::session_id_text(args) == Some(session_id));
    // Session cleanup never represents a workspace edit or a verifier verdict.
    if vtcode_core::tools::tool_intent::is_exec_session_cleanup_call(canonical_name, args) {
        if matches!(&outcome.status, ToolExecutionStatus::Success { .. }) && targets_pending_verifier {
            loop_tracker.pending_verifier_session_id = None;
        }
        return false;
    }
    if is_session_follow_up(canonical_name, args)
        && targets_pending_verifier
        && let ToolExecutionStatus::Success { output, .. } = &outcome.status
        && let Some(exit_code) = output.get("exit_code").and_then(serde_json::Value::as_i64)
    {
        if exit_code == 0 {
            loop_tracker.mark_verification_complete();
        } else {
            loop_tracker.record_failed_verification();
        }
        loop_tracker.reset_navigation_window(true);
        return exit_code != 0;
    }
    let navigation_family =
        crate::agent::runloop::unified::turn::tool_outcomes::handlers::low_signal_family_key(canonical_name, args);
    let low_signal_family = navigation_family
        .clone()
        .filter(|_| is_low_signal_outcome(outcome, canonical_name, args));
    let navigation_signature_key = navigation_family
        .map(|family| format!("navigation::{family}"))
        .unwrap_or_else(|| signature_key.clone());
    // Successful but redundant scans (e.g. three overlapping `find` calls)
    // never match the exact family key. Track a coarse inspection family so
    // the third repeat surfaces in diagnostics and in the turn balancer's
    // listing-loop tripwire without changing admission behavior.
    let coarse_family = coarse_inspection_family_key(canonical_name, args);
    let coarse_repeat = coarse_family.as_ref().map(|family| {
        let entry = loop_tracker
            .coarse_inspection_attempts
            .entry(family.clone())
            .or_insert((0, Instant::now()));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = Instant::now();
        entry.0
    });
    let is_coarse_duplicate =
        coarse_repeat.is_some_and(|count| count >= 3) && matches!(&outcome.status, ToolExecutionStatus::Success { .. });
    let mut low_signal_family = low_signal_family;
    if low_signal_family.is_none() && is_coarse_duplicate {
        low_signal_family = coarse_family.clone();
    }
    let is_low_signal_navigation = low_signal_family.is_some();
    if let Some(low_signal_family) = low_signal_family.as_ref() {
        loop_tracker.record_low_signal(low_signal_family.clone());
    }

    // Lost verifier results via session follow-ups: a `write_stdin`,
    // `unified_exec` poll/wait/inspect/continue, or PTY session follow-up
    // failure that reports a missing session means the session (and its
    // pending verifier output) died before the result was captured, so the
    // follow-up never classifies as ShellActivity::Verification. While the
    // gate is pending, treat it like a failed verifier so the model gets a
    // bounded fix/diagnostic window instead of deadlocking behind a gate
    // that can no longer observe a successful verifier. When its identity is
    // known, an unrelated missing session must not discard the live verifier.
    if is_session_follow_up(canonical_name, args)
        && loop_tracker.verification_is_pending()
        && (loop_tracker.pending_verifier_session_id.is_none() || targets_pending_verifier)
        && let ToolExecutionStatus::Failure { error } = &outcome.status
        && (error.is_exec_session_not_found() || error_text_indicates_lost_session(&error.message))
    {
        loop_tracker.verification_result_lost_notice_pending = true;
        loop_tracker.record_failed_verification();
        loop_tracker.reset_navigation_window(low_signal_family.is_none());
        return true;
    }

    // Update NL2Repo-Bench metrics based on tool intent.
    //
    // IMPORTANT: Check execution tools FIRST. `classify_tool_intent` marks
    // `command_session(action=run)` as `mutating: true` because shell commands *can*
    // mutate state, but for the Edit-Test heuristic, any execution/verification
    // step (cargo check, cargo test, etc.) should RESET the mutation counter,
    // not increment it.
    if is_execution_tool(canonical_name) {
        // Classify the command the kernel ran, not the typed text: a verifier
        // piped only into `head`/`tail` executes standalone, so its truthful
        // exit status is the verifier's and it counts as verification.
        let executed = shell_args_as_executed(canonical_name, args);
        match classify_shell_activity(canonical_name, &executed) {
            ShellActivity::Inspection => {
                loop_tracker.consecutive_navigations = loop_tracker.consecutive_navigations.saturating_add(1);
                loop_tracker.nav_signatures.insert(navigation_signature_key);
                loop_tracker.record_navigation_signal(is_low_signal_navigation);
            }
            ShellActivity::Verification => {
                if let ToolExecutionStatus::Success { output, .. } = &outcome.status
                    && output.get("exit_code").and_then(serde_json::Value::as_i64).is_none()
                    && let Some(session_id) = output.get("session_id").and_then(serde_json::Value::as_str)
                {
                    loop_tracker.mark_verification_pending();
                    loop_tracker.fix_edits_remaining = 0;
                    loop_tracker.pending_verifier_session_id = Some(session_id.to_owned());
                    loop_tracker.reset_navigation_window(true);
                    return false;
                }
                if matches!(&outcome.status, ToolExecutionStatus::Success { output, .. } if output.get("exit_code").and_then(serde_json::Value::as_i64) == Some(0))
                {
                    loop_tracker.mark_verification_complete();
                } else if matches!(&outcome.status, ToolExecutionStatus::Success { command_success: false, .. }) {
                    // Only a verifier that actually ran and reported non-zero
                    // opens the fix-up window. Tool-level Failure/Timeout (never
                    // executed, e.g. argument errors) must not grant edits.
                    loop_tracker.record_failed_verification();
                    loop_tracker.reset_navigation_window(low_signal_family.is_none());
                    return true;
                } else if loop_tracker.verification_is_pending()
                    && matches!(
                        &outcome.status,
                        ToolExecutionStatus::Failure { error } | ToolExecutionStatus::Timeout { error }
                            if !matches!(error.error_type, vtcode_core::tools::registry::ToolErrorType::InvalidParameters
                                | vtcode_core::tools::registry::ToolErrorType::PermissionDenied
                                | vtcode_core::tools::registry::ToolErrorType::PolicyViolation
                                | vtcode_core::tools::registry::ToolErrorType::ToolNotFound)
                    )
                {
                    // While the gate is pending, a verifier-level
                    // Failure/Timeout almost always means the result was lost
                    // (e.g. the exec session ended before the verifier
                    // finished) rather than a genuine non-zero exit: the
                    // verifier never produced an observable verdict. A bounded
                    // fix window that still requires a successful standalone
                    // verifier to clear is strictly better than a permanent
                    // stall. Without the gate pending this stays a no-grant
                    // arg-error path.
                    loop_tracker.verification_result_lost_notice_pending = true;
                    loop_tracker.record_failed_verification();
                    loop_tracker.reset_navigation_window(low_signal_family.is_none());
                    return true;
                }
                loop_tracker.reset_navigation_window(low_signal_family.is_none());
            }
            ShellActivity::Mutation => {
                // Piped verifier attempts that the kernel cannot elide (e.g.
                // `cargo check 2>&1 | grep error`, or a `;` join) are admitted
                // to run but never clear the gate: the exit status belongs to
                // another command, not the verifier. Don't count them as
                // blind edits; a failed piped attempt still
                // opens the fix window so the agent can repair and re-run a
                // standalone verifier. Chained mutations smuggled behind a
                // verifier prefix are rejected by the admission predicate and
                // take the blind-edit path below.
                if shell_command_is_admitted_verification_attempt(&executed) {
                    let ran_and_failed =
                        matches!(&outcome.status, ToolExecutionStatus::Success { command_success: false, .. });
                    if ran_and_failed {
                        loop_tracker.record_failed_verification();
                        loop_tracker.reset_navigation_window(low_signal_family.is_none());
                        return true;
                    }
                    // A piped verifier's exit status belongs to another
                    // command, so a success cannot clear the gate. While the
                    // gate is pending that silence reads as "verified" to
                    // the model (checkpoint session-vtcode-20260912T083718Z:
                    // a piped verifier exited 0 and the turn still
                    // deadlocked). Queue the one-shot piped-verifier
                    // directive so the handlers surface it after the tool
                    // response lands.
                    if loop_tracker.verification_is_pending()
                        && matches!(&outcome.status, ToolExecutionStatus::Success { command_success: true, .. })
                    {
                        loop_tracker.piped_verification_notice_pending = true;
                        loop_tracker.reset_navigation_window(low_signal_family.is_none());
                        return true;
                    }
                    loop_tracker.reset_navigation_window(low_signal_family.is_none());
                } else {
                    if mutation_was_applied(outcome) {
                        loop_tracker.record_successful_mutation();
                    }
                    loop_tracker.reset_navigation_window(low_signal_family.is_none());
                }
            }
        }
    } else if is_plan_artifact_write(canonical_name, args) || is_docs_only_write(canonical_name, args) {
        // Plan artifact writes in dedicated plan storage and docs-only prose
        // writes are allowed while pending and should not trigger
        // anti-blind-editing verification pressure.
        // Low-signal repetition history is preserved: plan/docs writes are not
        // navigation, so they neither advance nor clear that window.
        loop_tracker.reset_navigation_window(false);
    } else {
        let intent = vtcode_core::tools::tool_intent::classify_tool_intent(canonical_name, args);
        if intent.mutating {
            if mutation_was_applied(outcome) {
                loop_tracker.record_successful_mutation();
            }
            loop_tracker.reset_navigation_window(low_signal_family.is_none());
        } else {
            // Read-only / navigation tool
            loop_tracker.consecutive_navigations += 1;
            loop_tracker.nav_signatures.insert(navigation_signature_key);
            loop_tracker.record_navigation_signal(is_low_signal_navigation);
        }
    }
    false
}

fn mutation_was_applied(outcome: &ToolPipelineOutcome) -> bool {
    match &outcome.status {
        ToolExecutionStatus::Success { output, command_success, modified_files, .. } => {
            if let Some(effective_change) = vtcode_core::tools::file_ops::diff_output_has_effective_change(output) {
                return effective_change;
            }
            *command_success || !modified_files.is_empty()
        }
        ToolExecutionStatus::Failure { .. } | ToolExecutionStatus::Timeout { .. } | ToolExecutionStatus::Cancelled => {
            false
        }
    }
}
pub(crate) fn serialize_output(output: &serde_json::Value) -> String {
    if let Some(s) = output.as_str() {
        s.to_string()
    } else {
        serde_json::to_string(output).unwrap_or_else(|_| "{}".to_string())
    }
}

pub(crate) fn check_is_argument_error(error_str: &str) -> bool {
    error_str.contains("Missing required")
        || error_str.contains("Invalid arguments")
        || error_str.contains("Tool argument validation failed")
        || error_str.contains("required path parameter")
        || error_str.contains("is required for '")
        || error_str.contains("is required for \"")
        || error_str.contains("'index' is required")
        || error_str.contains("'index_path' is required")
        || error_str.contains("'status' is required")
        || error_str.contains("expected ")
        || error_str.contains("Expected:")
}

#[cfg(test)]
mod tests;
