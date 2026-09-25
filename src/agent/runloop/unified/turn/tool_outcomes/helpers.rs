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
use crate::agent::runloop::unified::turn::tool_outcomes::{is_grep_style_no_match, output_field_is_empty};

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
/// Warning rendered when the harness executes the project verifier itself
/// after the model exhausted its directive retries. Distinct from
/// [`VERIFICATION_AUTO_RECOVERY_WARNING`] (a directive grant) so transcripts
/// show the harness took action rather than asking once more.
pub(crate) const HARNESS_AUTO_VERIFICATION_WARNING: &str =
    "[i] Harness auto-verification: running the project verifier now instead of blocking.";
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
pub(crate) fn tracker_continue_follow_up(incomplete: &[String]) -> String {
    let joined = incomplete.join(", ");
    format!(
        "The task tracker still has incomplete steps: {joined}. This follow-up is the harness resuming \
         the work, so no user reply is needed. The next step is to continue with the next incomplete step \
         using tools and update task_tracker as steps complete. A status-only recap does not advance the \
         tracker. The turn can end when the tracker is complete, or when a user decision or a \
         permission/policy block stops progress."
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
    if reason.contains("permission")
        || reason.contains("user input")
        || reason.contains("request_user_input")
        || reason.contains("safety fuse")
        || reason.contains("manual intervention")
        || reason.contains("verification is still pending")
        || reason.contains("unverified assistant responses")
        || reason.contains("anti-blind")
        || reason.contains("verification gate")
        || reason.contains("context exceeded")
        || reason.contains("compaction could not reduce")
        || reason.contains("unmatched tool result")
        || reason.contains("attempted more tool calls")
        || reason.contains("final tool-free synthesis pass")
        || reason.contains("stale recovery state")
        || reason.contains("interview")
        || reason.contains("awaiting approval")
        || reason.contains("approval-ready plan remains")
    {
        return false;
    }
    // Recoverable production reason shapes — keep aligned with
    // `completion::recoverable_status_recap_phrasing` plus outer-only
    // harness constants (text-cap / no-response / safety-cap wording).
    reason.contains("recovery fallback")
        || reason.contains("recovery could not confirm")
        || reason.contains("recovery exhausted")
        || reason.contains("recovery was exhausted")
        || reason.contains("reached the safety cap")
        || reason.contains("safety cap")
        || reason.contains("preview budget")
        || reason.contains("tool preview budget")
        || reason.contains("turn budget")
        || reason.contains("tool budget")
        || reason.contains("tool loop budget")
        || reason.contains("tool loop")
        || reason.contains("tool-call budget")
        || reason.contains("tool follow-up")
        || reason.contains("wall clock")
        || reason.contains("blocked due to repeated")
        || reason.contains("blocked after repeated")
        || reason.contains("without a harness-visible final assistant response")
        || reason.contains("max tool")
        || reason.contains("per-turn tool")
        || reason.contains("read cap")
        || reason.contains("work budget")
        || reason.contains("budget exhausted")
        || reason.contains("budget ran out")
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
    if incomplete_items.is_none_or(|items| items.is_empty()) {
        return false;
    }
    if turn_completed {
        return true;
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
    if lower.contains("request_user_input")
        || lower.contains("permission")
        || lower.contains("user input")
        || lower.contains("awaiting")
        || lower.contains("interview")
        || lower.contains("attempted more tool calls")
        || lower.contains("final tool-free synthesis pass")
        || lower.contains("verification is still pending")
        || lower.contains("compaction could not reduce")
        || lower.contains("unmatched tool result")
    {
        return false;
    }
    // Production recovery constants (including PLANNING_COMPLETED_TURN_FALLBACK_REASON)
    // are recoverable. Deny tokens like "planning turn ended" / "approval-ready
    // plan" must not shadow "recovery fallback".
    if lower.contains("recovery fallback")
        || lower.contains("recovery could not confirm")
        || lower.contains("recovery exhausted")
        || lower.contains("recovery was exhausted")
        || lower.contains("reached the safety cap")
        || lower.contains("preview budget")
        || lower.contains("tool preview budget")
        || lower.contains("turn budget")
        || lower.contains("tool budget")
        || lower.contains("tool loop budget")
        || lower.contains("wall clock")
        || lower.contains("tool-free recovery")
        || lower.contains("tool follow-up")
        || lower.contains("budget exhausted")
    {
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
mod tracker_continue_tests {
    use super::*;

    #[test]
    fn parse_incomplete_tracker_items_shapes() {
        assert!(parse_incomplete_tracker_items(&serde_json::json!({"status":"empty"})).is_none());
        let complete = serde_json::json!({
            "status":"ok",
            "checklist":{"items":[{"description":"a","status":"completed"}]}
        });
        assert!(parse_incomplete_tracker_items(&complete).is_none());
        let mixed = serde_json::json!({
            "status":"ok",
            "checklist":{"items":[
                {"index":1,"description":"analyze","status":"completed"},
                {"index":2,"description":"change","status":"in_progress"},
                {"index":3,"description":"verify","status":"pending"}
            ]}
        });
        let items = parse_incomplete_tracker_items(&mixed).expect("incomplete");
        assert_eq!(items, vec!["#2 change (in_progress)".to_string(), "#3 verify (pending)".to_string()]);
    }

    #[test]
    fn tracker_follow_up_lists_items_and_forbids_nudge() {
        let prompt =
            tracker_continue_follow_up(&["#2 change (pending)".to_string(), "#3 verify (blocked)".to_string()]);
        assert!(prompt.contains("#2 change (pending)"));
        assert!(prompt.contains("#3 verify (blocked)"));
        assert!(prompt.contains("no user reply is needed"));
        assert!(prompt.contains("A status-only recap does not advance the tracker"));
    }

    #[test]
    fn tracker_continue_directive_is_shared_calm_prose() {
        let incomplete = ["#2 change (pending)".to_string(), "#3 verify (pending)".to_string()];
        for label in [TRACKER_RESUME_DIRECTIVE_LABEL, TRACKER_AUTO_CONTINUE_DIRECTIVE_LABEL] {
            let directive = tracker_continue_directive(label, &incomplete);
            assert!(directive.starts_with(&format!("{label}: task_tracker still has incomplete steps:")));
            assert!(directive.contains("#2 change (pending), #3 verify (pending)"));
            assert!(directive.contains("no user reply is needed"));
            assert!(directive.contains("status-only recap does not advance the tracker"));
            assert!(!directive.contains("do not"), "directive states consequences, not prohibitions: {directive}");
        }
    }

    #[test]
    fn plan_mode_auto_continue_gate_respects_user_gates_and_budget() {
        // Ready-for-approval is a user gate.
        assert!(!should_queue_plan_mode_auto_continue(true, true, true, true, None, false, 8, 0));
        // Ordinary completed planning turns never auto-continue (interview risk).
        assert!(!should_queue_plan_mode_auto_continue(true, true, false, true, None, false, 8, 0));
        // Recoverable blocked planning continues.
        assert!(should_queue_plan_mode_auto_continue(
            true,
            true,
            false,
            false,
            Some("reached the safety cap"),
            false,
            8,
            0
        ));
        // Planning handoff / verification / blocked-without-reason stay off.
        // Production PLANNING_COMPLETED_TURN_FALLBACK_REASON is recoverable
        // (allow-list after true-handoff deny) so planning auto-continues.
        assert!(should_queue_plan_mode_auto_continue(
            true,
            true,
            false,
            false,
            Some(
                "Planning turn ended via recovery fallback without confirming an approval-ready plan; planning remains active."
            ),
            false,
            8,
            0
        ));
        // Compound permission+recovery stays denied.
        assert!(!should_queue_plan_mode_auto_continue(
            true,
            true,
            false,
            false,
            Some("recovery fallback; permission denied for exec_command"),
            false,
            8,
            0
        ));
        assert!(!should_queue_plan_mode_auto_continue(
            true,
            true,
            false,
            false,
            Some("pending verification"),
            true,
            8,
            0
        ));
        assert!(!should_queue_plan_mode_auto_continue(true, true, false, false, None, false, 8, 0));
        // Kill-switch / zero budget / inactive planning stay off.
        assert!(!should_queue_plan_mode_auto_continue(false, true, false, false, Some("turn budget"), false, 8, 0));
        assert!(!should_queue_plan_mode_auto_continue(true, true, false, false, Some("turn budget"), false, 0, 0));
        assert!(!should_queue_plan_mode_auto_continue(true, false, false, false, Some("turn budget"), false, 8, 0));
    }

    #[test]
    fn plan_mode_auto_continue_stops_after_consecutive_empty_fallbacks() {
        let reason = Some(
            "Planning turn ended via recovery fallback without confirming an approval-ready plan; planning remains active.",
        );
        assert!(should_queue_plan_mode_auto_continue(true, true, false, false, reason, false, 32, 0));
        assert!(should_queue_plan_mode_auto_continue(true, true, false, false, reason, false, 32, 1));
        assert!(!should_queue_plan_mode_auto_continue(
            true,
            true,
            false,
            false,
            reason,
            false,
            32,
            MAX_PLAN_EMPTY_FALLBACK_AUTO_CONTINUE
        ));
        assert!(!should_queue_plan_mode_auto_continue(true, true, false, false, reason, false, 32, 3));
    }

    #[test]
    fn detects_plan_empty_fallback_text() {
        let empty = "Planning remains active, but this turn ended without a final plan synthesis. The research gathered above is preserved, so the next turn can reuse it without re-reading files. Type `keep planning`.";
        assert!(is_plan_empty_fallback_text(empty));
        // The gate must recognize the production fallback text, not only the fixture.
        assert!(is_plan_empty_fallback_text(
            crate::agent::runloop::unified::turn::turn_loop::PLANNING_COMPLETED_FALLBACK_RESPONSE
        ));
        assert!(!is_plan_empty_fallback_text("Planning turn ended via recovery fallback without confirming plan."));
        assert!(!is_plan_empty_fallback_text(""));
    }

    #[test]
    fn plan_mode_recoverable_block_allow_list() {
        assert!(plan_mode_recoverable_block("turn budget exhausted"));
        assert!(plan_mode_recoverable_block("reached the safety cap"));
        assert!(plan_mode_recoverable_block("tool-free recovery after safety cap"));
        assert!(plan_mode_recoverable_block(
            "Turn ended with a recovery fallback; the requested work was not confirmed."
        ));
        // Production planning fallback must auto-queue (allow-list first).
        assert!(plan_mode_recoverable_block(
            "Planning turn ended via recovery fallback without confirming an approval-ready plan; planning remains active."
        ));
        assert!(plan_mode_recoverable_block(
            "Tool loop budget exhausted before a final response; planning remains active."
        ));
        // Interview / approval / permission handoffs stay denied.
        assert!(!plan_mode_recoverable_block("request_user_input pending"));
        assert!(!plan_mode_recoverable_block("permission required"));
        assert!(!plan_mode_recoverable_block(
            "Recovery mode requested a final tool-free synthesis pass, but the model attempted more tool calls."
        ));
        // Compound recovery+permission must deny (true handoff first).
        assert!(!plan_mode_recoverable_block("recovery fallback after permission denied for exec_command"));
        assert!(!plan_mode_recoverable_block("tool budget exhausted while awaiting user approval"));
    }

    #[test]
    fn plan_progress_line_shapes() {
        assert_eq!(plan_progress_line("Release", false, 0, 0), "• Plan Release — research/synthesis");
        assert_eq!(plan_progress_line("Release", false, 2, 4), "• Plan Release — open decisions: 2");
        assert_eq!(plan_progress_line("Release", true, 0, 4), "• Plan Release — ready for approval (4 steps)");
        assert_eq!(plan_progress_line("Release", true, 0, 0), "• Plan Release — ready for approval");
        assert_eq!(plan_progress_line("", true, 0, 0), "• Plan — ready for approval");
        assert_eq!(plan_progress_line("", false, 0, 0), "• Plan — research/synthesis");
        assert_eq!(plan_progress_line("", true, 0, 4), "• Plan — ready for approval (4 steps)");
        assert_eq!(plan_progress_line("", false, 1, 0), "• Plan — open decisions: 1");
    }

    #[test]
    fn plan_mode_continue_messages_keep_planning_read_only_without_a_user_nudge() {
        for prompt in [
            plan_mode_continue_follow_up(),
            plan_mode_auto_continue_directive(),
            plan_mode_resume_directive(),
        ] {
            assert!(prompt.contains("no user reply is needed"), "{prompt}");
            assert!(prompt.contains("read-only"), "{prompt}");
            assert!(prompt.contains("code changes wait for plan approval"), "{prompt}");
            assert!(prompt.contains("<proposed_plan>"), "{prompt}");
            let normalized = vtcode_core::planning::normalize_plan_intent(&prompt);
            assert!(vtcode_core::planning::matches_stay_intent(&normalized), "{prompt}");
            assert!(!vtcode_core::planning::contains_implementation_cue(&normalized), "{prompt}");
        }
        assert!(plan_mode_continue_follow_up().starts_with(PLAN_MODE_AUTO_CONTINUE_MARKER));
        assert!(plan_mode_auto_continue_directive().starts_with(PLAN_MODE_AUTO_CONTINUE_MARKER));
    }

    #[test]
    fn refusal_notices_never_auto_continue() {
        // A refusal explanation may quote text that matches recoverable
        // tokens ("recovery fallback", "tool budget"); the refusal still wins.
        let reason = format!(
            "{}: the request looked like a recovery fallback for a tool budget bypass. \
             The request was not retried; rephrase it or switch models.",
            refusal::REFUSAL_NOTICE_PREFIX
        );
        assert!(!tracker_auto_continue_is_recoverable_block(Some(&reason)));
        assert!(!plan_mode_recoverable_block(&reason));
        assert!(!should_queue_tracker_auto_continue(
            true,
            false,
            false,
            Some(&reason),
            false,
            Some(&["step".to_string()]),
            3,
            false,
            false,
        ));
        assert!(!should_queue_plan_mode_auto_continue(true, true, false, false, Some(&reason), false, 3, 0));
    }

    #[test]
    fn recoverable_block_classification_allow_list() {
        // Production RECOVERY_CONTRACT_VIOLATION_REASON — must NOT auto-queue
        // despite containing "tool-free".
        assert!(!tracker_auto_continue_is_recoverable_block(Some(
            "Recovery mode requested a final tool-free synthesis pass, but the model attempted more tool calls."
        )));
        // Blocked with unknown/missing reason is not auto-queued.
        assert!(!tracker_auto_continue_is_recoverable_block(None));
        // Production PENDING_VERIFICATION_BLOCK_REASON.
        assert!(!tracker_auto_continue_is_recoverable_block(Some(
            "Turn blocked after repeated unverified assistant responses; verification is still pending."
        )));
        // Production POST_TOOL_CONTEXT_COMPACTION_FAILED_REASON.
        assert!(!tracker_auto_continue_is_recoverable_block(Some(
            "The provider rejected the follow-up because the context exceeded its capacity, and the bounded recovery compaction could not reduce the request."
        )));
        // Recoverable production constants.
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Turn ended with a recovery fallback; the requested work was not confirmed. The current plan and task state were retained."
        )));
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Turn blocked after repeated assistant responses reached the safety cap; the latest response was preserved."
        )));
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Post-tool recovery could not confirm the requested work after one bounded tool-enabled retry. The completed tool outputs and resume handoff were retained; retry from the pending step."
        )));
        assert!(tracker_auto_continue_is_recoverable_block(Some("preview budget exhausted")));
        assert!(tracker_auto_continue_is_recoverable_block(Some("Turn blocked due to repeated failing behavior.")));
        // Session/production budget phrases from residual UX work.
        assert!(tracker_auto_continue_is_recoverable_block(Some("Task 7 blocked by the turn's preview budget")));
        assert!(tracker_auto_continue_is_recoverable_block(Some("tool budget ran out")));
        assert!(tracker_auto_continue_is_recoverable_block(Some("hit the per-file read cap")));
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Tool loop budget exhausted before a final response."
        )));
        // True handoffs stay terminal.
        assert!(!tracker_auto_continue_is_recoverable_block(Some(
            "Anti-blind checkpoint: verification is still pending"
        )));
        assert!(!tracker_auto_continue_is_recoverable_block(Some("verification gate remains open")));
        // Unknown / policy handoffs stay terminal.
        assert!(!tracker_auto_continue_is_recoverable_block(Some("some unknown block")));
        assert!(!tracker_auto_continue_is_recoverable_block(Some("exec_command is denied by permission policy")));
        assert!(!tracker_auto_continue_is_recoverable_block(Some(
            "I hit the tool-call safety fuse mid-verification"
        )));
        assert!(!tracker_auto_continue_is_recoverable_block(Some("request_user_input is required")));
        assert!(!tracker_auto_continue_is_recoverable_block(Some(
            "Turn blocked after repeated unverified assistant responses; verification is still pending."
        )));
        // Session/production recoverable vocabulary parity.
        assert!(tracker_auto_continue_is_recoverable_block(Some("Task 7 blocked by the turn's preview budget")));
        assert!(tracker_auto_continue_is_recoverable_block(Some("tool budget ran out")));
        assert!(tracker_auto_continue_is_recoverable_block(Some("per-file read cap")));
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Tool loop budget exhausted before a final response"
        )));
        assert!(tracker_auto_continue_is_recoverable_block(Some("work budget exhausted")));
    }

    #[test]
    fn outer_queue_gate_blocks_unknown_and_contract_violation() {
        let incomplete = ["#2 change (pending)".to_string()];
        // Completed + incomplete tracker → queue when not a handoff/question.
        assert!(should_queue_tracker_auto_continue(
            true,
            false,
            true,
            None,
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
        // Completed + genuine user question → do not queue past the ask.
        assert!(!should_queue_tracker_auto_continue(
            true,
            false,
            true,
            None,
            false,
            Some(&incomplete),
            8,
            false,
            true
        ));
        assert!(!should_queue_tracker_auto_continue(
            true,
            false,
            false,
            None,
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
        assert!(!should_queue_tracker_auto_continue(
            true,
            false,
            false,
            Some("Recovery mode requested a final tool-free synthesis pass, but the model attempted more tool calls."),
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
        assert!(should_queue_tracker_auto_continue(
            true,
            false,
            false,
            Some(
                "Turn ended with a recovery fallback; the requested work was not confirmed. The current plan and task state were retained."
            ),
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
    }

    #[test]
    fn outer_queue_gate_respects_planning_verification_and_budget() {
        let incomplete = ["#2 change (pending)".to_string()];
        assert!(should_queue_tracker_auto_continue(
            true,
            false,
            true,
            None,
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
        assert!(!should_queue_tracker_auto_continue(
            false,
            false,
            true,
            None,
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
        assert!(!should_queue_tracker_auto_continue(
            true,
            true,
            true,
            None,
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
        assert!(!should_queue_tracker_auto_continue(true, false, true, None, false, None, 8, false, false));
        assert!(!should_queue_tracker_auto_continue(
            true,
            false,
            true,
            None,
            false,
            Some(&incomplete),
            0,
            false,
            false
        ));
        assert!(!should_queue_tracker_auto_continue(
            true,
            false,
            true,
            None,
            false,
            Some(&incomplete),
            8,
            true,
            false,
            // safety-handoff final text
        ));
        assert!(!should_queue_tracker_auto_continue(
            true,
            false,
            false,
            Some("pending verification; type continue"),
            true,
            Some(&incomplete),
            8,
            false,
            false
        ));
        assert!(should_queue_tracker_auto_continue(
            true,
            false,
            false,
            Some("preview budget exhausted"),
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
        assert!(should_queue_tracker_auto_continue(
            true,
            false,
            false,
            Some(
                "Turn ended with a recovery fallback; the requested work was not confirmed. The current plan and task state were retained."
            ),
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
        assert!(should_queue_tracker_auto_continue(
            true,
            false,
            false,
            Some(
                "Turn blocked after repeated assistant responses reached the safety cap; the latest response was preserved."
            ),
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
        assert!(!should_queue_tracker_auto_continue(
            true,
            false,
            false,
            Some("permission denied"),
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
        assert!(!should_queue_tracker_auto_continue(
            true,
            false,
            false,
            Some("unknown block reason"),
            false,
            Some(&incomplete),
            8,
            false,
            false
        ));
    }

    #[test]
    fn resume_gate_honors_zero_cross_turn_budget() {
        let incomplete = ["#1 analyze (in_progress)".to_string()];
        assert!(should_queue_tracker_resume_continuation(true, 8, Some(&incomplete)));
        assert!(!should_queue_tracker_resume_continuation(true, 0, Some(&incomplete)));
        assert!(!should_queue_tracker_resume_continuation(false, 8, Some(&incomplete)));
        assert!(!should_queue_tracker_resume_continuation(true, 8, None));
    }

    #[test]
    fn tracker_config_defaults() {
        assert!(tracker_auto_continue_enabled(None));
        assert_eq!(tracker_cross_turn_turns(None), 32);
    }

    #[test]
    fn recoverable_block_includes_tool_budget_and_plan_fallback_shapes() {
        // Production TOOL_LOOP_LIMIT_RECOVERY_REASON.
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Tool loop budget exhausted before a final response. Tools are disabled for one bounded synthesis pass."
        )));
        // Tool-call budget / follow-up recovery constants.
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Tool follow-up failed. Tools disabled; respond with text using context and recent tool outputs."
        )));
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Turn ended without a harness-visible final assistant response, so successful completion could not be confirmed."
        )));
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Approved-plan execution stopped after recovery was exhausted. The approved plan and task checklist were retained."
        )));
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Per-turn tool limit reached (max: 32). Wait or adjust config."
        )));
        // Planning fallback contains "recovery fallback" (recoverable token);
        // outer tracker queue is separately gated by planning_active.
        assert!(tracker_auto_continue_is_recoverable_block(Some(
            "Planning turn ended via recovery fallback without confirming an approval-ready plan; planning remains active."
        )));
    }

    #[test]
    fn plan_mode_recoverable_allows_planning_fallback_constant() {
        // PLANNING_COMPLETED_TURN_FALLBACK_REASON must auto-queue (allow-list first).
        assert!(plan_mode_recoverable_block(
            "Planning turn ended via recovery fallback without confirming an approval-ready plan; planning remains active. The current plan and task state were retained."
        ));
        assert!(plan_mode_recoverable_block(
            "Turn ended with a recovery fallback; the requested work was not confirmed."
        ));
        // Interview / approval / permission handoffs stay denied.
        assert!(!plan_mode_recoverable_block("request_user_input is required for the planning interview"));
        assert!(!plan_mode_recoverable_block("permission denied for exec_command"));
        assert!(!plan_mode_recoverable_block(
            "Recovery mode requested a final tool-free synthesis pass, but the model attempted more tool calls."
        ));
        assert!(!plan_mode_recoverable_block("recovery fallback; interview waiting on user decision"));
    }

    #[test]
    fn session_stats_progress_resets_tracker_budget() {
        let mut stats = crate::agent::runloop::unified::state::SessionStats::default();
        assert!(stats.record_tracker_continuation_turn_with_limit(32));
        assert!(stats.record_tracker_continuation_turn_with_limit(32));
        assert_eq!(stats.tracker_continuation_turns(), 2);
        // No first observation yet — initial 0 → 0 is not progress.
        assert!(!stats.note_tracker_completed_count(0));
        assert_eq!(stats.tracker_continuation_turns(), 2);
        // Progress → reset episode budget.
        assert!(stats.note_tracker_completed_count(1));
        stats.reset_tracker_continuation_budget();
        assert_eq!(stats.tracker_continuation_turns(), 0);
        // Same count is not further progress.
        assert!(!stats.note_tracker_completed_count(1));
        // Tracker recreate with a lower completed count still resets the episode.
        assert!(stats.record_tracker_continuation_turn_with_limit(32));
        assert!(stats.note_tracker_completed_count(0));
        stats.reset_tracker_continuation_budget();
        assert_eq!(stats.tracker_continuation_turns(), 0);
    }

    #[test]
    fn tracker_probe_cache_clears_on_complete_and_keeps_on_unavailable() {
        let mut cache: Option<Vec<String>> = Some(vec!["#1 a (pending)".to_string()]);
        // Complete is an authoritative clear.
        let effective = apply_tracker_probe_to_cache(&mut cache, TrackerProbeOutcome::Complete);
        assert!(effective.is_none());
        assert!(cache.is_none());
        // Incomplete replaces the cache.
        let effective = apply_tracker_probe_to_cache(
            &mut cache,
            TrackerProbeOutcome::Incomplete(vec!["#2 b (in_progress)".to_string()]),
        );
        assert_eq!(effective, Some(["#2 b (in_progress)".to_string()].as_slice()));
        // Unavailable keeps the last incomplete set.
        let effective = apply_tracker_probe_to_cache(&mut cache, TrackerProbeOutcome::Unavailable);
        assert_eq!(effective, Some(["#2 b (in_progress)".to_string()].as_slice()));
        // Parse distinguishes complete vs incomplete vs unavailable shapes.
        let complete = serde_json::json!({
            "status": "ok",
            "checklist": {"items": [{"index": 1, "description": "a", "status": "completed"}]}
        });
        assert_eq!(tracker_probe_outcome(&complete), TrackerProbeOutcome::Complete);
        assert!(parse_incomplete_tracker_items(&complete).is_none());
        let incomplete = serde_json::json!({
            "status": "ok",
            "checklist": {"items": [{"index": 1, "description": "a", "status": "pending"}]}
        });
        assert!(matches!(tracker_probe_outcome(&incomplete), TrackerProbeOutcome::Incomplete(_)));
        assert_eq!(tracker_probe_outcome(&serde_json::json!({"status": "empty"})), TrackerProbeOutcome::Complete);
        assert_eq!(tracker_probe_outcome(&serde_json::json!({"no_status": true})), TrackerProbeOutcome::Unavailable);
        // Checklist without items is malformed → Unavailable (keep cache), not Complete.
        assert_eq!(
            tracker_probe_outcome(&serde_json::json!({"status": "ok", "checklist": {}})),
            TrackerProbeOutcome::Unavailable
        );
    }

    #[test]
    fn session_stats_apply_tracker_probe_clears_stale_incomplete() {
        let mut stats = crate::agent::runloop::unified::state::SessionStats::default();
        let after_incomplete =
            stats.apply_tracker_probe(TrackerProbeOutcome::Incomplete(vec!["#1 a (pending)".to_string()]));
        assert_eq!(after_incomplete, Some(["#1 a (pending)".to_string()].as_slice()));
        // Successful complete must clear — do not auto-continue after tracker finishes.
        assert!(stats.apply_tracker_probe(TrackerProbeOutcome::Complete).is_none());
        // Unavailable after a new incomplete keeps that incomplete set.
        stats.apply_tracker_probe(TrackerProbeOutcome::Incomplete(vec!["#2 b (pending)".to_string()]));
        assert_eq!(
            stats.apply_tracker_probe(TrackerProbeOutcome::Unavailable),
            Some(["#2 b (pending)".to_string()].as_slice())
        );
    }

    #[test]
    fn parse_tracker_completed_count_from_list_payload() {
        let payload = serde_json::json!({
            "status": "ok",
            "checklist": {
                "items": [
                    {"index": 1, "description": "a", "status": "completed"},
                    {"index": 2, "description": "b", "status": "in_progress"},
                    {"index": 3, "description": "c", "status": "pending"},
                ]
            }
        });
        assert_eq!(parse_tracker_completed_count(&payload), 1);
        assert_eq!(parse_incomplete_tracker_items(&payload).map(|items| items.len()), Some(2));
        let empty = serde_json::json!({"status": "empty"});
        assert_eq!(parse_tracker_completed_count(&empty), 0);
    }

    #[test]
    fn session_stats_resets_tracker_budget_with_verification_episode() {
        let mut stats = crate::agent::runloop::unified::state::SessionStats::default();
        assert!(stats.record_tracker_continuation_turn_with_limit(8));
        assert!(stats.record_tracker_continuation_turn_with_limit(8));
        assert_eq!(stats.tracker_continuation_turns(), 2);
        // Verification-episode reset must NOT wipe tracker/plan continuation budgets.
        stats.reset_verification_recovery_episode();
        assert_eq!(stats.tracker_continuation_turns(), 2);
        assert!(stats.record_plan_continuation_turn_with_limit(1));
        assert!(!stats.record_plan_continuation_turn_with_limit(1));
        stats.reset_tracker_continuation_budget();
        assert_eq!(stats.tracker_continuation_turns(), 0);
        assert_eq!(stats.plan_continuation_turns(), 1);
        stats.reset_plan_continuation_budget();
        assert_eq!(stats.plan_continuation_turns(), 0);
    }
}

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
        self.verification_is_pending() && !self.auto_verification_executed
    }

    /// Record that the harness executed the project verifier itself this
    /// turn. Unconditional: every outcome (success, failure, lost result)
    /// flows through the normal tracker paths, which clear or preserve the
    /// gate; the flag only prevents a second harness execution this turn.
    pub(crate) fn record_auto_verification_executed(&mut self) {
        self.auto_verification_executed = true;
    }

    fn mark_verification_complete(&mut self) {
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
    // that can no longer observe a successful verifier.
    if is_session_follow_up(canonical_name, args)
        && loop_tracker.verification_is_pending()
        && let ToolExecutionStatus::Failure { error } = &outcome.status
        && error_text_indicates_lost_session(&error.message)
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
                if matches!(&outcome.status, ToolExecutionStatus::Success { command_success: true, .. }) {
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
                        ToolExecutionStatus::Failure { .. } | ToolExecutionStatus::Timeout { .. }
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
mod tests {
    use serde_json::json;
    use vtcode_core::config::constants::tools;

    use super::*;

    #[test]
    fn push_tool_response_replaces_existing_tool_call_entry() {
        let mut history = vec![uni::Message::tool_response(
            "call_1".to_string(),
            "{\"output\":\"first\"}".to_string(),
        )];

        let update =
            push_tool_response(&mut history, "call_1".to_string(), None, "{\"output\":\"latest\"}".to_string());

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content.as_text_borrowed(), Some("{\"output\":\"latest\"}"));
        assert_eq!(update, ToolResponseHistoryUpdate::Replaced { previous_text_len: "{\"output\":\"first\"}".len() });
    }

    #[test]
    fn push_tool_response_sets_origin_tool_when_provided() {
        let mut history = Vec::new();

        let update = push_tool_response(
            &mut history,
            "call_1".to_string(),
            Some("read_file"),
            "{\"output\":\"first\"}".to_string(),
        );

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].origin_tool.as_deref(), Some("read_file"));
        assert_eq!(update, ToolResponseHistoryUpdate::Appended);
    }

    #[test]
    fn push_tool_response_refreshes_origin_tool_when_replacing_same_call() {
        let mut history = vec![uni::Message::tool_response("call_1".to_string(), "old".to_string())];

        let update = push_tool_response(&mut history, "call_1".to_string(), Some("exec_command"), "new".to_string());

        assert_eq!(update, ToolResponseHistoryUpdate::Replaced { previous_text_len: 3 });
        assert_eq!(history[0].origin_tool.as_deref(), Some("exec_command"));
    }

    #[test]
    fn push_tool_response_appends_when_id_reused_across_assistant_boundary() {
        // Fabricated ids can collide across turns (e.g. index-based fallbacks).
        // A later assistant message re-declaring the same id must not cause a
        // new result to clobber the earlier, unrelated Tool response.
        let mut history = vec![
            uni::Message::assistant_with_tools(
                "first".into(),
                vec![uni::ToolCall::function(
                    "call_1".into(),
                    "file_operation".into(),
                    "{}".into(),
                )],
            ),
            uni::Message::tool_response("call_1".to_string(), "{\"output\":\"first\"}".into()),
            uni::Message::assistant_with_tools(
                "second".into(),
                vec![uni::ToolCall::function(
                    "call_1".into(),
                    tools::CODE_SEARCH.into(),
                    "{}".into(),
                )],
            ),
        ];

        let update = push_tool_response(
            &mut history,
            "call_1".to_string(),
            Some(tools::CODE_SEARCH),
            "{\"output\":\"second\"}".to_string(),
        );

        let tool_messages: Vec<&uni::Message> = history
            .iter()
            .filter(|message| matches!(message.role, uni::MessageRole::Tool))
            .collect();
        assert_eq!(tool_messages.len(), 2, "must append, not overwrite");
        assert_eq!(
            tool_messages[0].content.as_text_borrowed(),
            Some("{\"output\":\"first\"}"),
            "earlier unrelated Tool result must remain intact"
        );
        assert_eq!(tool_messages[1].content.as_text_borrowed(), Some("{\"output\":\"second\"}"));
        assert_eq!(update, ToolResponseHistoryUpdate::Appended);
    }

    #[test]
    fn push_tool_response_appends_when_assistant_has_no_tool_calls() {
        // When an Assistant message has no tool_calls (e.g. commentary-only
        // message between tool calls), the boundary must STILL stop the scan.
        // Otherwise a later Tool response with a colliding fabricated id would
        // overwrite an earlier, unrelated Tool result.
        let mut history = vec![
            uni::Message::assistant_with_tools(
                String::new(),
                vec![uni::ToolCall::function(
                    "call_0".into(),
                    "file_operation".into(),
                    "{}".into(),
                )],
            ),
            uni::Message::tool_response("call_0".to_string(), "{\"output\":\"file content\"}".into()),
            // Commentary Assistant with no tool_calls — must act as boundary
            uni::Message::assistant("I need to retry.".into()),
            uni::Message::assistant_with_tools(
                String::new(),
                vec![uni::ToolCall::function(
                    "call_0".into(),
                    "apply_patch".into(),
                    "{}".into(),
                )],
            ),
        ];

        let update = push_tool_response(
            &mut history,
            "call_0".to_string(),
            Some("apply_patch"),
            "{\"output\":\"patch result\"}".to_string(),
        );

        let tool_messages: Vec<&uni::Message> = history
            .iter()
            .filter(|message| matches!(message.role, uni::MessageRole::Tool))
            .collect();
        assert_eq!(tool_messages.len(), 2, "must append, not overwrite the earlier file read");
        assert_eq!(
            tool_messages[0].content.as_text_borrowed(),
            Some("{\"output\":\"file content\"}"),
            "earlier file read result must remain intact"
        );
        assert_eq!(tool_messages[1].content.as_text_borrowed(), Some("{\"output\":\"patch result\"}"));
        assert_eq!(update, ToolResponseHistoryUpdate::Appended);
    }

    #[test]
    fn repetition_tracker_counts_failures() {
        let mut tracker = LoopTracker::new();
        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                "edit_file".to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "boom".to_string(),
            ),
        });

        update_repetition_tracker(&mut tracker, &outcome, "edit_file", &json!({"path":"src/main.rs"}));

        assert_eq!(tracker.max_count_filtered(|_| false), 1);
    }

    #[test]
    fn failed_file_mutations_do_not_trigger_verification_pressure() {
        let mut tracker = LoopTracker::new();
        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                "apply_patch".to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "invalid patch path".to_string(),
            ),
        });

        update_repetition_tracker(
            &mut tracker,
            &outcome,
            tools::APPLY_PATCH,
            &json!({"input":"*** Begin Patch\n*** Update File: /absolute/path\n*** End Patch"}),
        );

        assert_eq!(tracker.consecutive_mutations, 0);
    }

    #[test]
    fn no_op_write_does_not_trigger_verification_pressure() {
        let mut tracker = LoopTracker::new();
        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: json!({
                "success": true,
                "path": "README.md",
                "diff_preview": {
                    "content": "",
                    "truncated": false,
                    "omitted_line_count": 0,
                    "skipped": false,
                    "is_empty": true
                },
                "diff": [{
                    "path": "README.md",
                    "content": "",
                    "truncated": false,
                    "omitted_line_count": 0,
                    "skipped": false,
                    "is_empty": true
                }]
            }),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(
            &mut tracker,
            &outcome,
            tools::WRITE_FILE,
            &json!({"path":"README.md","content":"same\n","mode":"overwrite"}),
        );

        assert_eq!(tracker.consecutive_mutations, 0);
    }

    #[test]
    fn skipped_write_does_not_trigger_verification_pressure() {
        let mut tracker = LoopTracker::new();
        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: json!({
                "success": true,
                "skipped": true,
                "reason": "File already exists"
            }),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(
            &mut tracker,
            &outcome,
            tools::WRITE_FILE,
            &json!({"path":"README.md","content":"same\n","mode":"skip_if_exists"}),
        );

        assert_eq!(tracker.consecutive_mutations, 0);
    }

    #[test]
    fn verification_gate_blocks_mutations_but_allows_reads_checks_and_plan_artifacts() {
        let mut tracker = LoopTracker::new();
        tracker.verification_pending = true;

        assert!(mutation_blocked_until_verification(
            &tracker,
            tools::WRITE_FILE,
            &json!({"path":"src/lib.rs","content":"new"})
        ));
        // Docs-only prose stays allowed while pending and never trips the gate.
        assert!(!mutation_blocked_until_verification(
            &tracker,
            tools::WRITE_FILE,
            &json!({"path":"README.md","content":"new"})
        ));
        assert!(!mutation_blocked_until_verification(
            &tracker,
            tools::WRITE_FILE,
            &json!({"path":"docs/guide.md","content":"new"})
        ));
        // Mixed docs+code patches stay blocked (fail closed).
        assert!(mutation_blocked_until_verification(
            &tracker,
            tools::APPLY_PATCH,
            &json!({"patch":"*** Begin Patch\n*** Update File: README.md\n@@\n-old\n+new\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch\n"})
        ));
        assert!(mutation_blocked_until_verification(
            &tracker,
            tools::EXEC_COMMAND,
            &json!({"cmd":"sed -i '' 's/old/new/' README.md"})
        ));
        assert!(!mutation_blocked_until_verification(&tracker, tools::READ_FILE, &json!({"path":"README.md"})));
        assert!(!mutation_blocked_until_verification(
            &tracker,
            tools::EXEC_COMMAND,
            &json!({"cmd":"cargo check --locked"})
        ));
        assert!(!mutation_blocked_until_verification(
            &tracker,
            tools::WRITE_FILE,
            &json!({"path":".vtcode/plans/next.md","content":"plan"})
        ));
        assert!(!mutation_blocked_until_verification(&tracker, tools::TASK_TRACKER, &json!({"action":"update"})));
    }

    #[test]
    fn inspection_does_not_clear_mutations_waiting_for_verification() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;

        for command in ["git diff -- README.md", "git diff --check"] {
            update_repetition_tracker(&mut tracker, &success, tools::EXEC_COMMAND, &json!({"cmd":command}));
        }

        assert_eq!(tracker.consecutive_mutations, BLIND_EDITING_THRESHOLD);
    }

    #[test]
    fn failed_verification_does_not_clear_mutations_waiting_for_verification() {
        let mut tracker = LoopTracker::new();
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        tracker.verification_pending = true;
        let failed_check = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 1}),
            stdout: None,
            modified_files: vec![],
            command_success: false,
        });

        update_repetition_tracker(&mut tracker, &failed_check, tools::EXEC_COMMAND, &json!({"cmd":"cargo check"}));

        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.consecutive_mutations, BLIND_EDITING_THRESHOLD);
        // A failed verifier keeps the gate but opens a bounded fix-up window
        // so the broken build can be repaired instead of deadlocking.
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);
        assert!(!mutation_blocked_until_verification(&tracker, tools::EDIT_FILE, &json!({"path": "src/lib.rs"})));
    }

    #[test]
    fn failed_verification_fix_window_is_consumed_by_repair_edits() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let failed_check = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 1}),
            stdout: None,
            modified_files: vec![],
            command_success: false,
        });
        update_repetition_tracker(&mut tracker, &failed_check, tools::EXEC_COMMAND, &json!({"cmd":"cargo check"}));
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);

        let edit = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        for _ in 0..FAILED_VERIFICATION_FIX_ALLOWANCE {
            assert!(!mutation_blocked_until_verification(&tracker, tools::EDIT_FILE, &json!({"path": "src/lib.rs"})));
            update_repetition_tracker(&mut tracker, &edit, tools::EDIT_FILE, &json!({"path": "src/lib.rs"}));
            assert!(tracker.verification_is_pending());
        }
        // Window exhausted: further mutations block again until a standalone
        // verifier succeeds.
        assert_eq!(tracker.fix_edits_remaining, 0);
        assert!(mutation_blocked_until_verification(&tracker, tools::EDIT_FILE, &json!({"path": "src/lib.rs"})));
    }

    #[test]
    fn piped_verifier_is_admitted_but_does_not_clear_gate() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        // Filtering-piped verifiers must run (not block) so the model sees
        // output, but the exit status is the filter's — they never clear.
        assert!(!mutation_blocked_until_verification(
            &tracker,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo check --locked 2>&1 | grep error"})
        ));
        let piped_success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        update_repetition_tracker(
            &mut tracker,
            &piped_success,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo check --locked 2>&1 | grep error"}),
        );
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.consecutive_mutations, BLIND_EDITING_THRESHOLD);
    }

    #[test]
    fn truncation_only_piped_verifier_success_clears_gate() {
        // The kernel elides a pure `| head`/`| tail` tail and runs the
        // standalone verifier, so its exit 0 is the verifier's own. The
        // tracker must agree even when handed the typed (raw) arguments,
        // e.g. after a PreToolUse hook rewrite.
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        for command in [
            "cargo check --locked 2>&1 | tail -5",
            "cargo check --locked 2>&1 | head -c 4000",
        ] {
            let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
            tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
            assert!(
                !mutation_blocked_until_verification(&tracker, tools::EXEC_COMMAND, &json!({"cmd": command})),
                "{command}"
            );
            assert!(
                !update_repetition_tracker(&mut tracker, &success, tools::EXEC_COMMAND, &json!({"cmd": command})),
                "{command}"
            );
            assert!(!tracker.verification_is_pending(), "elided verifier success must clear: {command}");
            assert_eq!(tracker.consecutive_mutations, 0, "{command}");
            assert!(!tracker.take_piped_verification_notice(), "no piped notice for {command}");
        }
    }

    #[test]
    fn anti_blind_editing_directive_states_the_shared_shell_form_note() {
        assert!(ANTI_BLIND_EDITING_DIRECTIVE.ends_with(vtcode_core::tools::tool_intent::VERIFIER_SHELL_FORM_NOTE));
        assert!(!PIPED_VERIFICATION_DIRECTIVE.contains("`tail`/`head`"));
    }

    #[test]
    fn smuggled_mutation_behind_verifier_prefix_stays_blocked() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        for command in [
            "cargo check && rm -rf target",
            "cargo check; rm foo.txt",
            "cargo check --locked && cargo test && rm foo.txt",
        ] {
            assert!(
                mutation_blocked_until_verification(&tracker, tools::EXEC_COMMAND, &json!({"cmd": command})),
                "smuggled mutation must stay blocked: {command}"
            );
        }
    }

    #[test]
    fn pure_and_chained_verifiers_are_admitted_and_clear_gate() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        tracker.verification_result_lost_notice_pending = true;
        tracker.piped_verification_notice_pending = true;
        for command in [
            "cargo fmt --all -- --check && cargo check --locked",
            "cargo check --locked && cargo nextest run --locked -p vtcode-ui",
            "cargo check --locked && cargo clippy --locked -p vtcode-ui -- -D warnings",
        ] {
            assert!(
                !mutation_blocked_until_verification(&tracker, tools::EXEC_COMMAND, &json!({"cmd": command})),
                "pure && verifier chain must be admitted: {command}"
            );
        }

        let chained_success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &chained_success,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo fmt --all -- --check && cargo check --locked"}),
        ));
        assert!(!tracker.verification_is_pending());
        assert_eq!(tracker.consecutive_mutations, 0);
        assert!(!tracker.take_verification_result_lost_notice());
        assert!(!tracker.take_piped_verification_notice());
    }

    #[test]
    fn non_and_chained_verifiers_do_not_clear_gate() {
        for command in [
            "cargo check --locked; cargo nextest run --locked -p vtcode-ui",
            "cargo check --locked || cargo nextest run --locked -p vtcode-ui",
            "cargo check --locked | grep -v warning",
        ] {
            let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
            tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
            let chained_success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
                output: serde_json::json!({"exit_code": 0}),
                stdout: None,
                modified_files: vec![],
                command_success: true,
            });
            update_repetition_tracker(&mut tracker, &chained_success, tools::EXEC_COMMAND, &json!({"cmd": command}));
            assert!(tracker.verification_is_pending(), "`;`/`||`/`|` chains must not clear the gate: {command}");
        }
    }

    #[test]
    fn gate_trips_on_sixth_consecutive_code_mutation_not_fifth() {
        let mut tracker = LoopTracker::new();
        let edit = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        for _ in 0..(BLIND_EDITING_THRESHOLD - 1) {
            update_repetition_tracker(&mut tracker, &edit, tools::EDIT_FILE, &json!({"path": "src/lib.rs"}));
        }
        assert!(!tracker.verification_is_pending());
        assert!(!mutation_blocked_until_verification(&tracker, tools::EDIT_FILE, &json!({"path": "src/lib.rs"})));
        update_repetition_tracker(&mut tracker, &edit, tools::EDIT_FILE, &json!({"path": "src/lib.rs"}));
        assert!(tracker.verification_is_pending());
        assert!(mutation_blocked_until_verification(&tracker, tools::EDIT_FILE, &json!({"path": "src/lib.rs"})));
    }

    #[test]
    fn docs_only_writes_stay_allowed_and_do_not_increment_counter() {
        let mut tracker = LoopTracker::new();
        let docs_edit = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        for _ in 0..BLIND_EDITING_THRESHOLD {
            update_repetition_tracker(
                &mut tracker,
                &docs_edit,
                tools::WRITE_FILE,
                &json!({"path": "README.md", "content": "prose"}),
            );
        }
        assert_eq!(tracker.consecutive_mutations, 0);
        assert!(!tracker.verification_is_pending());

        let mut pending = LoopTracker::with_verification_snapshot((true, 0));
        pending.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        update_repetition_tracker(&mut pending, &docs_edit, tools::EDIT_FILE, &json!({"path": "docs/guide.md"}));
        assert!(pending.verification_is_pending());
        assert_eq!(pending.consecutive_mutations, BLIND_EDITING_THRESHOLD);
        assert!(!mutation_blocked_until_verification(&pending, tools::EDIT_FILE, &json!({"path": "docs/guide.md"})));
        assert!(mutation_blocked_until_verification(&pending, tools::EDIT_FILE, &json!({"path": "src/lib.rs"})));
    }

    #[test]
    fn expanded_verifiers_clear_gate_while_mutating_lookalikes_do_not() {
        for command in [
            "bun test",
            "deno lint",
            "make test",
            "just lint",
            "ruff check src/",
            "tsc --noEmit",
            "eslint src/",
            "python3 -m pytest",
            "uv run pytest",
        ] {
            let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
            tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
            assert!(
                !mutation_blocked_until_verification(&tracker, tools::EXEC_COMMAND, &json!({"cmd": command})),
                "verifier must be admitted: {command}"
            );
            let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
                output: serde_json::json!({"exit_code": 0}),
                stdout: None,
                modified_files: vec![],
                command_success: true,
            });
            update_repetition_tracker(&mut tracker, &success, tools::EXEC_COMMAND, &json!({"cmd": command}));
            assert!(!tracker.verification_is_pending(), "verifier must clear the gate: {command}");
        }
        for command in [
            "make clean",
            "make test clean",
            "tsc",
            "eslint --fix src/",
            "ruff format src/",
            "bun install",
        ] {
            let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
            tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
            assert!(
                mutation_blocked_until_verification(&tracker, tools::EXEC_COMMAND, &json!({"cmd": command})),
                "mutating lookalike must stay blocked: {command}"
            );
        }
    }

    #[test]
    fn docs_only_boundary_cases_fail_closed() {
        let pending = LoopTracker::with_verification_snapshot((true, 0));
        // Code under docs/ stays a code edit.
        for path in [
            "docs/script.py",
            "docs/app.ts",
            "README.py",
            "readme_script.py",
            "LICENSE-MIT",
        ] {
            assert!(
                mutation_blocked_until_verification(&pending, tools::EDIT_FILE, &json!({"path": path})),
                "code-looking path must stay blocked: {path}"
            );
            assert!(!is_docs_only_write(tools::EDIT_FILE, &json!({"path": path})), "{path}");
        }
        // Prose spellings stay allowed, including camelCase `filePath`
        // (supplemented: `mutation_target_paths` lacks that key).
        for args in [
            json!({"path": "README"}),
            json!({"path": "README.md"}),
            json!({"path": "CHANGELOG.rst"}),
            json!({"path": "LICENSE"}),
            json!({"path": "docs/guide.md"}),
            json!({"filePath": "docs/guide.md"}),
        ] {
            assert!(!mutation_blocked_until_verification(&pending, tools::EDIT_FILE, &args), "{args}");
            assert!(is_docs_only_write(tools::EDIT_FILE, &args), "{args}");
        }
        // Exec-tool mutations never qualify, even for prose paths.
        assert!(!is_docs_only_write(tools::EXEC_COMMAND, &json!({"cmd": "echo hi > README.md"})));
    }

    #[test]
    fn piped_verifier_success_while_pending_queues_notice_once() {
        // Regression guard for session-vtcode-20260912T083718Z: a piped
        // verifier success exited 0 while the gate was pending, cleared
        // nothing, and said nothing — the model believed it had verified and
        // the turn deadlocked on unverified text responses.
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let piped_success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        assert!(update_repetition_tracker(
            &mut tracker,
            &piped_success,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo check --locked -p vtcode 2>&1 | grep -E 'error|warning'"}),
        ));
        assert!(tracker.verification_is_pending(), "piped success must not clear the gate");
        assert!(tracker.take_piped_verification_notice(), "piped success must queue the notice");
        assert!(!tracker.take_piped_verification_notice(), "notice is one-shot");

        // A standalone pure-`&&` verifier success afterwards clears the gate
        // together with any queued piped notice.
        let chained_success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &chained_success,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo fmt --all -- --check && cargo check --locked"}),
        ));
        assert!(!tracker.verification_is_pending());
        assert!(!tracker.take_piped_verification_notice());
    }

    #[test]
    fn piped_verifier_success_without_pending_gate_stays_silent() {
        let mut tracker = LoopTracker::new();
        let piped_success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &piped_success,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo check --locked 2>&1 | grep error"}),
        ));
        assert!(!tracker.take_piped_verification_notice());
    }

    #[test]
    fn fmt_check_clears_gate_but_plain_fmt_does_not() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        assert!(!mutation_blocked_until_verification(
            &tracker,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo fmt --all -- --check"})
        ));

        let fmt_check_success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        update_repetition_tracker(
            &mut tracker,
            &fmt_check_success,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo fmt --all -- --check"}),
        );
        assert!(!tracker.verification_is_pending());

        // Plain `cargo fmt` rewrites files: it stays a mutation and never
        // clears the gate.
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        assert!(mutation_blocked_until_verification(&tracker, tools::EXEC_COMMAND, &json!({"cmd": "cargo fmt"})));
    }

    #[test]
    fn failed_verifier_reports_fix_window_for_text_streak_reset() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let failed_check = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 1}),
            stdout: None,
            modified_files: vec![],
            command_success: false,
        });
        assert!(update_repetition_tracker(
            &mut tracker,
            &failed_check,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo check --locked"}),
        ));
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);

        let successful_check = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &successful_check,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo check --locked"}),
        ));
    }

    #[test]
    fn lost_verification_tool_failure_while_pending_grants_fix_window() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let tool_failure = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::EXEC_COMMAND.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "check could not start".to_string(),
            ),
        });
        assert!(update_repetition_tracker(
            &mut tracker,
            &tool_failure,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo check --locked"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);
        assert!(
            !mutation_blocked_until_verification(&tracker, tools::EDIT_FILE, &json!({"path": "src/lib.rs"})),
            "the lost-result grant must open the bounded fix window"
        );
        assert!(tracker.take_verification_result_lost_notice(), "the lost-result directive must be queued");
        assert!(!tracker.take_verification_result_lost_notice(), "the notice is one-shot");
    }

    #[test]
    fn lost_verification_tool_timeout_while_pending_grants_fix_window() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let tool_timeout = ToolPipelineOutcome::from_status(ToolExecutionStatus::Timeout {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::EXEC_COMMAND.to_string(),
                vtcode_core::tools::registry::ToolErrorType::Timeout,
                "verification command timed out".to_string(),
            ),
        });
        assert!(update_repetition_tracker(
            &mut tracker,
            &tool_timeout,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo nextest run"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);
    }

    #[test]
    fn verification_tool_failure_without_pending_gate_grants_no_fix_window() {
        let mut tracker = LoopTracker::new();
        let tool_failure = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::EXEC_COMMAND.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "check could not start".to_string(),
            ),
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &tool_failure,
            tools::EXEC_COMMAND,
            &json!({"cmd": "cargo check --locked"}),
        ));
        assert!(!tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, 0);
        assert!(!tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn write_stdin_lost_exec_session_failure_while_pending_grants_fix_window() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let lost_session = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::WRITE_STDIN.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "exec session 'run-7' not found. Copy the exact `session_id` from the original run response"
                    .to_string(),
            ),
        });
        assert!(update_repetition_tracker(
            &mut tracker,
            &lost_session,
            tools::WRITE_STDIN,
            &json!({"session_id": "run-7", "chars": ""}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);
        assert!(!mutation_blocked_until_verification(&tracker, tools::EDIT_FILE, &json!({"path": "src/lib.rs"})));
        assert!(tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn write_stdin_unrelated_failure_while_pending_grants_no_fix_window() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let unrelated = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::WRITE_STDIN.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "session is not writable".to_string(),
            ),
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &unrelated,
            tools::WRITE_STDIN,
            &json!({"session_id": "run-7", "chars": "q"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, 0);
        assert!(!tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn unified_exec_wait_on_lost_session_while_pending_grants_fix_window() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let lost_session = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::UNIFIED_EXEC.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "exec session 'run-7' not found. Copy the exact `session_id` from the original run response"
                    .to_string(),
            ),
        });
        assert!(update_repetition_tracker(
            &mut tracker,
            &lost_session,
            tools::UNIFIED_EXEC,
            &json!({"action": "wait", "session_id": "run-7"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);
        assert!(!mutation_blocked_until_verification(&tracker, tools::EDIT_FILE, &json!({"path": "src/lib.rs"})));
        assert!(tracker.take_verification_result_lost_notice());
        assert!(!tracker.take_verification_result_lost_notice(), "the notice is one-shot");
    }

    #[test]
    fn unified_exec_poll_on_lost_session_while_pending_grants_fix_window() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let lost_session = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::UNIFIED_EXEC.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "exec session 'run-7' not found. Copy the exact `session_id` from the original run response"
                    .to_string(),
            ),
        });
        assert!(update_repetition_tracker(
            &mut tracker,
            &lost_session,
            tools::UNIFIED_EXEC,
            &json!({"action": "poll", "session_id": "run-7"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);
        assert!(tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn unified_exec_inferred_poll_on_lost_session_while_pending_grants_fix_window() {
        // No explicit `action`: a bare `session_id` infers a poll follow-up,
        // so the lost-session branch must still fire.
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let lost_session = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::UNIFIED_EXEC.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "exec session 'run-7' not found. Copy the exact `session_id` from the original run response"
                    .to_string(),
            ),
        });
        assert!(update_repetition_tracker(
            &mut tracker,
            &lost_session,
            tools::UNIFIED_EXEC,
            &json!({"session_id": "run-7"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);
        assert!(tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn unified_exec_wait_unrelated_failure_while_pending_grants_no_fix_window() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let unrelated = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::UNIFIED_EXEC.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "session_id is required for command session wait".to_string(),
            ),
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &unrelated,
            tools::UNIFIED_EXEC,
            &json!({"action": "wait"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, 0);
        assert!(!tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn unified_exec_wait_lost_session_without_pending_gate_grants_nothing() {
        let mut tracker = LoopTracker::new();
        let lost_session = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::UNIFIED_EXEC.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "exec session 'run-7' not found. Copy the exact `session_id` from the original run response"
                    .to_string(),
            ),
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &lost_session,
            tools::UNIFIED_EXEC,
            &json!({"action": "wait", "session_id": "run-7"}),
        ));
        assert!(!tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, 0);
        assert!(!tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn unified_exec_run_action_with_lost_session_text_takes_no_follow_up_path() {
        // A fresh `run` creates its session, so it is not a follow-up: the
        // error text alone must not open the lost-result window. (A run whose
        // command classifies as Verification still takes the generic
        // verifier-loss branch; `echo` classifies as Inspection, so nothing
        // is granted here.)
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let failure = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::UNIFIED_EXEC.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "exec session 'run-7' not found. Copy the exact `session_id` from the original run response"
                    .to_string(),
            ),
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &failure,
            tools::UNIFIED_EXEC,
            &json!({"action": "run", "command": "echo hi"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, 0);
        assert!(!tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn read_pty_session_poll_on_lost_pty_session_while_pending_grants_fix_window() {
        // A verifier waited on via a PTY session poll whose session died
        // reports "PTY session '<id>' not found" — the same lost-result shape
        // as the exec-session manager, and it needs the same bounded window.
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let lost_session = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::READ_PTY_SESSION.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "PTY session 'pty-3' not found".to_string(),
            ),
        });
        assert!(update_repetition_tracker(
            &mut tracker,
            &lost_session,
            tools::READ_PTY_SESSION,
            &json!({"session_id": "pty-3"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);
        assert!(!mutation_blocked_until_verification(&tracker, tools::EDIT_FILE, &json!({"path": "src/lib.rs"})));
        assert!(tracker.take_verification_result_lost_notice());
        assert!(!tracker.take_verification_result_lost_notice(), "the notice is one-shot");
    }

    #[test]
    fn send_pty_input_on_lost_pty_session_while_pending_grants_fix_window() {
        // `send_pty_input` is a session follow-up (carries `session_id`, never
        // classifies as Verification), so a dead session there grants too.
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let lost_session = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::SEND_PTY_INPUT.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "PTY session 'pty-3' not found".to_string(),
            ),
        });
        assert!(update_repetition_tracker(
            &mut tracker,
            &lost_session,
            tools::SEND_PTY_INPUT,
            &json!({"session_id": "pty-3", "input": "q"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);
        assert!(tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn create_pty_session_run_with_lost_session_text_takes_no_follow_up_path() {
        // A fresh PTY `run` creates its session, so it is not a follow-up:
        // the error text alone must not open the lost-result window.
        // (`cargo check` classifies as Verification, so use `echo` which
        // classifies as Inspection — nothing is granted here.)
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let failure = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::CREATE_PTY_SESSION.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "PTY session 'pty-3' not found".to_string(),
            ),
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &failure,
            tools::CREATE_PTY_SESSION,
            &json!({"command": "echo hi"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, 0);
        assert!(!tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn pty_follow_up_lost_session_without_pending_gate_grants_nothing() {
        let mut tracker = LoopTracker::new();
        let lost_session = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::READ_PTY_SESSION.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "PTY session 'pty-3' not found".to_string(),
            ),
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &lost_session,
            tools::READ_PTY_SESSION,
            &json!({"session_id": "pty-3"}),
        ));
        assert!(!tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, 0);
        assert!(!tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn pty_follow_up_unrelated_failure_while_pending_grants_no_fix_window() {
        // "no longer writable" is a live-session failure, not a lost session:
        // it carries no "not found", so the lost-result branch must not fire.
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        let unrelated = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::READ_PTY_SESSION.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "PTY session 'pty-3' is no longer writable".to_string(),
            ),
        });
        assert!(!update_repetition_tracker(
            &mut tracker,
            &unrelated,
            tools::READ_PTY_SESSION,
            &json!({"session_id": "pty-3"}),
        ));
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, 0);
        assert!(!tracker.take_verification_result_lost_notice());
    }

    #[test]
    fn verification_snapshot_bundle_round_trips_without_drift() {
        let tracker = LoopTracker::with_verification_snapshot((true, FAILED_VERIFICATION_FIX_ALLOWANCE));
        assert_eq!(tracker.verification_snapshot(), (true, FAILED_VERIFICATION_FIX_ALLOWANCE));
        let cleared = LoopTracker::with_verification_snapshot((false, FAILED_VERIFICATION_FIX_ALLOWANCE));
        assert_eq!(cleared.verification_snapshot(), (false, 0));
    }

    #[test]
    fn harness_verifier_override_must_be_standalone_or_pure_chain() {
        use super::resolve_harness_verifier_command;

        let dir = tempfile::TempDir::new().expect("workspace");
        let config_with = |override_command: &str| {
            let mut vt_cfg = vtcode_core::config::loader::VTCodeConfig::default();
            vt_cfg.agent.harness.verification.default_verifier_override = Some(override_command.to_string());
            vt_cfg
        };

        // Valid overrides win over detection (empty dir detects nothing).
        let vt_cfg = config_with("cargo nextest run -p mycrate");
        assert_eq!(
            resolve_harness_verifier_command(Some(&vt_cfg), dir.path()).as_deref(),
            Some("cargo nextest run -p mycrate")
        );
        // Pure-`&&` verifier chains are truthful and accepted.
        let vt_cfg = config_with("cargo fmt --all -- --check && cargo check --locked");
        assert!(resolve_harness_verifier_command(Some(&vt_cfg), dir.path()).is_some());
        // Piped, joined, and mutating overrides fall back to detection, which
        // finds nothing here — never executing attacker- or typo-shaped text.
        for bad in [
            "cargo check --locked | tail -5",
            "cargo check; cargo test",
            "cargo check || cargo test",
            "rm -rf /tmp/scratch",
            "   ",
        ] {
            let vt_cfg = config_with(bad);
            assert_eq!(resolve_harness_verifier_command(Some(&vt_cfg), dir.path()), None, "must not resolve: {bad:?}");
        }
        // Fallback works when detection finds a marker: the override loses.
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").expect("Cargo.toml");
        let vt_cfg = config_with("cargo check | tail -5");
        assert_eq!(
            resolve_harness_verifier_command(Some(&vt_cfg), dir.path()).as_deref(),
            Some("cargo check --locked")
        );
    }

    #[test]
    fn verification_auto_recovery_budget_is_bounded_and_cleared_on_success() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        for _ in 0..MAX_VERIFICATION_AUTO_RECOVERY_ATTEMPTS {
            assert!(tracker.record_verification_auto_recovery_with_limit(MAX_VERIFICATION_AUTO_RECOVERY_ATTEMPTS));
        }
        assert!(!tracker.record_verification_auto_recovery_with_limit(MAX_VERIFICATION_AUTO_RECOVERY_ATTEMPTS));
        assert_eq!(tracker.verification_auto_recovery_attempts(), MAX_VERIFICATION_AUTO_RECOVERY_ATTEMPTS);
        // A fresh turn starts with a fresh budget.
        let fresh = LoopTracker::with_verification_snapshot((true, 0));
        assert_eq!(fresh.verification_auto_recovery_attempts(), 0);

        // A successful standalone verifier clears the budget with the gate.
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        update_repetition_tracker(&mut tracker, &success, tools::EXEC_COMMAND, &json!({"cmd": "cargo check --locked"}));
        assert!(!tracker.verification_is_pending());
        assert_eq!(tracker.verification_auto_recovery_attempts(), 0);
    }

    #[test]
    fn auto_execute_verifier_is_one_shot_per_turn_until_success() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        tracker.consecutive_mutations = BLIND_EDITING_THRESHOLD;
        assert!(tracker.should_auto_execute_verifier());

        tracker.record_auto_verification_executed();
        assert!(!tracker.should_auto_execute_verifier());

        // A failed verifier keeps the gate but does not re-arm execution.
        let failed = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 1}),
            stdout: None,
            modified_files: vec![],
            command_success: false,
        });
        update_repetition_tracker(&mut tracker, &failed, tools::EXEC_COMMAND, &json!({"cmd": "cargo check --locked"}));
        assert!(tracker.verification_is_pending());
        assert!(!tracker.should_auto_execute_verifier());

        // A fresh turn re-arms; success clears the flag with the gate.
        let fresh = LoopTracker::with_verification_snapshot((true, 0));
        assert!(fresh.should_auto_execute_verifier());
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        update_repetition_tracker(&mut tracker, &success, tools::EXEC_COMMAND, &json!({"cmd": "cargo check --locked"}));
        assert!(!tracker.verification_is_pending());
        assert!(!tracker.auto_verification_executed);
    }

    #[test]
    fn logged_compound_inspections_do_not_trigger_anti_blind_pressure() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        for command in [
            "cat README.md && printf '\\n--- git status ---\\n' && git status --short",
            "wc -l README.md; rg -n '^#' README.md",
            "git diff --stat; find docs -maxdepth 2 -type f | sort | head -40",
        ] {
            update_repetition_tracker(&mut tracker, &success, tools::EXEC_COMMAND, &json!({"cmd":command}));
        }

        assert_eq!(tracker.consecutive_mutations, 0);
        assert!(!tracker.verification_is_pending());
        assert_eq!(tracker.consecutive_navigations, 3);
    }

    #[test]
    fn awk_range_inspections_do_not_trigger_anti_blind_pressure() {
        // Regression for session-vtcode-20260921T023834Z: six consecutive
        // read-only `awk` page reads tripped the blind-editing gate because
        // `awk` was missing from the read-only allow-list.
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        for _ in 0..BLIND_EDITING_THRESHOLD {
            update_repetition_tracker(
                &mut tracker,
                &success,
                tools::EXEC_COMMAND,
                &json!({"cmd": "awk 'NR>=297 && NR<=312' README.md"}),
            );
        }

        assert_eq!(tracker.consecutive_mutations, 0);
        assert!(!tracker.verification_is_pending());
        assert!(!mutation_blocked_until_verification(
            &tracker,
            tools::EXEC_COMMAND,
            &json!({"cmd": "awk 'NR>=297 && NR<=312' README.md"}),
        ));
    }

    #[test]
    fn awk_write_primitives_still_trigger_anti_blind_pressure() {
        // Asymmetric counterpart: real writes (including gawk `@` indirect
        // calls) must still count as mutations so the fix cannot
        // over-correct into a fail-open.
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        for _ in 0..BLIND_EDITING_THRESHOLD {
            update_repetition_tracker(
                &mut tracker,
                &success,
                tools::EXEC_COMMAND,
                &json!({"cmd": "awk -v f=system 'BEGIN{@f(\"id\")}' README.md"}),
            );
        }

        assert_eq!(tracker.consecutive_mutations, BLIND_EDITING_THRESHOLD);
        assert!(tracker.verification_is_pending());
        assert!(mutation_blocked_until_verification(
            &tracker,
            tools::EXEC_COMMAND,
            &json!({"cmd": "awk '{print > \"out.txt\"}' README.md"}),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn logged_compound_inspection_with_unix_stderr_suppression_does_not_trigger_pressure() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        let command = r###"git diff --stat; find docs -maxdepth 2 -type f | sort | head -40; rg -n "vtcode init|vtcode models|full-auto|run-debug|cargo install" docs/user-guide docs/installation docs/development 2>/dev/null | head -50"###;
        update_repetition_tracker(&mut tracker, &success, tools::EXEC_COMMAND, &json!({"cmd":command}));

        assert_eq!(tracker.consecutive_mutations, 0);
        assert_eq!(tracker.consecutive_navigations, 1);
    }

    #[test]
    fn only_a_completed_verification_clears_pending_mutation_pressure() {
        let mut tracker = LoopTracker::new();
        let edit = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        for _ in 0..BLIND_EDITING_THRESHOLD {
            update_repetition_tracker(&mut tracker, &edit, tools::EDIT_FILE, &json!({"path":"src/lib.rs"}));
        }
        assert!(tracker.verification_is_pending());

        let failed_check = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::EXEC_COMMAND.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ExecutionError,
                "check could not start".to_string(),
            ),
        });
        update_repetition_tracker(
            &mut tracker,
            &failed_check,
            tools::EXEC_COMMAND,
            &json!({"cmd":"cargo nextest run"}),
        );
        assert!(tracker.verification_is_pending());

        update_repetition_tracker(&mut tracker, &edit, tools::EXEC_COMMAND, &json!({"cmd":"cargo nextest run"}));
        assert!(!tracker.verification_is_pending());
        assert_eq!(tracker.consecutive_mutations, 0);
    }

    #[test]
    fn carried_verification_checkpoint_clears_after_successful_check() {
        let mut tracker = LoopTracker::with_verification_snapshot((true, 0));
        let successful_check = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(
            &mut tracker,
            &successful_check,
            tools::EXEC_COMMAND,
            &json!({"cmd":"cargo check --locked"}),
        );

        assert!(!tracker.verification_is_pending());
    }

    #[test]
    fn verification_snapshot_round_trips_through_session_state() {
        let tracker = LoopTracker::with_verification_snapshot((true, FAILED_VERIFICATION_FIX_ALLOWANCE));
        assert_eq!(tracker.verification_snapshot(), (true, FAILED_VERIFICATION_FIX_ALLOWANCE));
        assert_eq!(LoopTracker::new().verification_snapshot(), (false, 0));
    }

    #[test]
    fn repetition_tracker_ignores_cancellations() {
        let mut tracker = LoopTracker::new();
        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Cancelled);

        update_repetition_tracker(&mut tracker, &outcome, "edit_file", &json!({"path":"src/main.rs"}));

        assert_eq!(tracker.max_count_filtered(|_| false), 0);
    }

    #[test]
    fn reset_after_balancer_recovery_preserves_anti_blind_state() {
        let mut tracker = LoopTracker::new();
        tracker.record("code_search:{\"query\":\"Widget\"}".to_string());
        tracker.record("code_search:{\"query\":\"Widget\"}".to_string());
        tracker.consecutive_mutations = 2;
        tracker.verification_pending = true;
        tracker.fix_edits_remaining = FAILED_VERIFICATION_FIX_ALLOWANCE;
        tracker.verification_warning_emitted = true;
        tracker.verification_block_notice_emitted = true;
        tracker.verification_result_lost_notice_pending = true;
        tracker.piped_verification_notice_pending = true;
        tracker.consecutive_navigations = 4;
        tracker.consecutive_low_signal_navigations = 3;
        tracker.total_low_signal_navigations = 7;
        tracker.record_low_signal("code_search::Widget::src".to_string());
        tracker.navigation_loop_recoveries = 3;

        tracker.reset_after_balancer_recovery();

        assert_eq!(tracker.max_count_filtered(|_| false), 0);
        assert_eq!(tracker.max_low_signal_count(), 0);
        assert_eq!(tracker.consecutive_mutations, 2);
        assert!(tracker.verification_pending);
        assert!(tracker.verification_is_pending());
        assert_eq!(tracker.fix_edits_remaining, FAILED_VERIFICATION_FIX_ALLOWANCE);
        assert!(tracker.verification_warning_emitted);
        assert!(tracker.verification_block_notice_emitted);
        assert!(tracker.verification_result_lost_notice_pending);
        assert!(tracker.piped_verification_notice_pending);
        assert_eq!(tracker.consecutive_navigations, 0);
        assert_eq!(tracker.consecutive_low_signal_navigations, 0);
        assert_eq!(tracker.total_low_signal_navigations, 0);
        assert_eq!(tracker.navigation_loop_recoveries, 3);
    }

    #[test]
    fn balancer_recovery_cannot_postpone_the_mutation_threshold() {
        let mut tracker = LoopTracker::new();
        for _ in 0..(BLIND_EDITING_THRESHOLD - 1) {
            tracker.record_successful_mutation();
        }
        assert!(!tracker.verification_is_pending());

        tracker.reset_after_balancer_recovery();
        tracker.record_successful_mutation();

        assert_eq!(tracker.consecutive_mutations, BLIND_EDITING_THRESHOLD);
        assert!(tracker.verification_is_pending());
    }

    #[test]
    fn shell_activity_distinguishes_inspection_verification_and_mutation() {
        for command in [
            "rg -n 'LoopTracker' src",
            "find src -name '*.rs'",
            "cat Cargo.toml",
            "sed -n '1,80p' src/main.rs",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &json!({"cmd":command})),
                ShellActivity::Inspection,
                "{command}"
            );
        }

        for command in [
            "cargo check --locked",
            "cargo nextest run -p vtcode",
            "cargo clippy --all-targets",
            "cargo build --release",
            "./scripts/check-dev.sh --changed",
            "cargo check --locked > build.log",
            "cargo check &> build.log",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &json!({"cmd":command})),
                ShellActivity::Verification,
                "{command}"
            );
        }

        for command in [
            "cargo nextest run -p vtcode 2>&1 | head -c 4000",
            "cargo check | head -40",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &json!({"cmd":command})),
                ShellActivity::Mutation,
                "verification pipelines require reliable aggregate status: {command}"
            );
        }

        assert_eq!(
            classify_shell_activity(tools::EXEC_COMMAND, &json!({"cmd":"sed -i '' 's/a/b/' src/lib.rs"})),
            ShellActivity::Mutation
        );
        assert_eq!(
            classify_shell_activity(tools::EXEC_COMMAND, &json!({"cmd":"rm output && cargo check"})),
            ShellActivity::Mutation
        );
    }

    #[test]
    fn inspection_commands_increment_navigation_instead_of_resetting_it() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        for command in [
            "rg LoopTracker src",
            "find src -name '*.rs'",
            "cat Cargo.toml",
            "sed -n '1,20p' src/main.rs",
        ] {
            update_repetition_tracker(&mut tracker, &success, tools::EXEC_COMMAND, &json!({"cmd":command}));
        }

        assert_eq!(tracker.consecutive_navigations, 4);
    }

    #[test]
    fn navigation_tracking_ignores_nonsemantic_preview_controls() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: json!({"stdout":"useful source"}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        let command = "sed -n '278,420p' src/startup/mod.rs";

        update_repetition_tracker(
            &mut tracker,
            &success,
            tools::EXEC_COMMAND,
            &json!({"cmd": command, "command": command, "action": "run"}),
        );
        update_repetition_tracker(
            &mut tracker,
            &success,
            tools::EXEC_COMMAND,
            &json!({
                "cmd": command,
                "command": command,
                "max_output_tokens": 8000,
                "action": "run"
            }),
        );

        assert_eq!(tracker.consecutive_navigations, 2);
        assert_eq!(tracker.repeated_navigation_count(), 1);
        assert_eq!(tracker.consecutive_low_signal_navigations, 0);
    }

    #[test]
    fn productive_navigation_resets_only_consecutive_low_signal_count() {
        let mut tracker = LoopTracker::new();
        let miss = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: json!({"results":[]}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        let hit = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: json!({"results":[{"path":"src/lib.rs"}]}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        for query in ["missing-a", "missing-b"] {
            update_repetition_tracker(&mut tracker, &miss, tools::CODE_SEARCH, &json!({"query":query, "path":"src"}));
        }
        update_repetition_tracker(
            &mut tracker,
            &hit,
            tools::CODE_SEARCH,
            &json!({"query":"LoopTracker", "path":"src"}),
        );

        assert_eq!(tracker.consecutive_low_signal_navigations, 0);
        assert_eq!(tracker.total_low_signal_navigations, 2);
    }

    #[test]
    fn verification_resets_all_low_signal_navigation_counts() {
        let mut tracker = LoopTracker::new();
        tracker.consecutive_low_signal_navigations = 6;
        tracker.total_low_signal_navigations = 10;
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(
            &mut tracker,
            &success,
            tools::UNIFIED_EXEC,
            &json!({"action":"run", "command":"cargo check --locked"}),
        );

        assert_eq!(tracker.consecutive_low_signal_navigations, 0);
        assert_eq!(tracker.total_low_signal_navigations, 0);
    }

    #[test]
    fn consecutive_mutations_increments_on_edit() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        // edit_file is classified as mutating
        update_repetition_tracker(
            &mut tracker,
            &success,
            "edit_file",
            &json!({"path":"src/lib.rs","old_str":"a","new_str":"b"}),
        );
        assert_eq!(tracker.consecutive_mutations, 1);
        assert_eq!(tracker.consecutive_navigations, 0);

        update_repetition_tracker(&mut tracker, &success, "write_to_file", &json!({"path":"src/lib.rs","content":"x"}));
        assert_eq!(tracker.consecutive_mutations, 2);
    }

    #[test]
    fn execution_tool_resets_mutation_counter() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        // Two mutations
        update_repetition_tracker(
            &mut tracker,
            &success,
            "edit_file",
            &json!({"path":"a","old_str":"x","new_str":"y"}),
        );
        update_repetition_tracker(
            &mut tracker,
            &success,
            "edit_file",
            &json!({"path":"b","old_str":"x","new_str":"y"}),
        );
        assert_eq!(tracker.consecutive_mutations, 2);

        // Execution tool resets
        update_repetition_tracker(
            &mut tracker,
            &success,
            tools::UNIFIED_EXEC,
            &json!({"action":"run","command":"cargo check"}),
        );
        assert_eq!(tracker.consecutive_mutations, 0);
        assert_eq!(tracker.consecutive_navigations, 0);
    }

    #[test]
    fn reads_increment_navigation_counter() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(&mut tracker, &success, tools::READ_FILE, &json!({"path":"src/main.rs"}));
        assert_eq!(tracker.consecutive_navigations, 1);
        assert_eq!(tracker.consecutive_mutations, 0);

        update_repetition_tracker(&mut tracker, &success, tools::GREP_FILE, &json!({"pattern":"foo","path":"src/"}));
        assert_eq!(tracker.consecutive_navigations, 2);
    }

    #[test]
    fn mutation_resets_navigation_counter() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        // Several reads
        for _ in 0..5 {
            update_repetition_tracker(&mut tracker, &success, tools::READ_FILE, &json!({"path":"src/main.rs"}));
        }
        assert_eq!(tracker.consecutive_navigations, 5);

        // A mutation resets navigation counter
        update_repetition_tracker(
            &mut tracker,
            &success,
            "edit_file",
            &json!({"path":"src/lib.rs","old_str":"a","new_str":"b"}),
        );
        assert_eq!(tracker.consecutive_navigations, 0);
        assert_eq!(tracker.consecutive_mutations, 1);
    }

    #[test]
    fn task_tracker_does_not_increment_mutations_in_planning() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(
            &mut tracker,
            &success,
            tools::TASK_TRACKER,
            &json!({"action":"create","items":["step"]}),
        );
        assert_eq!(tracker.consecutive_mutations, 0);
        assert_eq!(tracker.consecutive_navigations, 0);
    }

    #[test]
    fn task_tracker_does_not_increment_mutations() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(
            &mut tracker,
            &success,
            tools::TASK_TRACKER,
            &json!({"action":"create","items":["step"]}),
        );
        assert_eq!(tracker.consecutive_mutations, 0);
        assert_eq!(tracker.consecutive_navigations, 0);
    }

    #[test]
    fn plan_file_write_does_not_increment_mutations() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(
            &mut tracker,
            &success,
            tools::UNIFIED_FILE,
            &json!({"action":"write","path":".vtcode/plans/my-plan.md","content":"text"}),
        );
        assert_eq!(tracker.consecutive_mutations, 0);
        assert_eq!(tracker.consecutive_navigations, 0);
    }

    #[test]
    fn non_plan_file_write_still_increments_mutations() {
        let mut tracker = LoopTracker::new();
        let success = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(
            &mut tracker,
            &success,
            tools::UNIFIED_FILE,
            &json!({"action":"write","path":"src/lib.rs","content":"text"}),
        );
        assert_eq!(tracker.consecutive_mutations, 1);
        assert_eq!(tracker.consecutive_navigations, 0);
    }

    #[test]
    fn argument_error_detection_includes_required_update_fields() {
        assert!(check_is_argument_error("Tool execution failed: 'index' is required for 'update' (1-indexed)"));
    }

    #[test]
    fn low_signal_tracker_groups_empty_search_results_by_family() {
        let mut tracker = LoopTracker::new();
        let miss = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"results":[]}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        // Different queries produce separate family keys, so each counts as its
        // own family while the agent explores one path.
        update_repetition_tracker(
            &mut tracker,
            &miss,
            tools::CODE_SEARCH,
            &json!({"query":"Widget", "path":"src", "result_types":["definition"]}),
        );
        update_repetition_tracker(
            &mut tracker,
            &miss,
            tools::CODE_SEARCH,
            &json!({"query":"Result", "path":"src", "result_types":["usage"]}),
        );
        update_repetition_tracker(
            &mut tracker,
            &miss,
            tools::CODE_SEARCH,
            &json!({"query":"Result<", "path":"src", "result_types":["text"]}),
        );

        assert_eq!(tracker.max_low_signal_count(), 1);
    }

    #[test]
    fn low_signal_tracker_groups_identical_searches_in_same_family() {
        let mut tracker = LoopTracker::new();
        let miss = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"results":[]}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        let args = json!({"query":"TODO","path":"src","file_types":["rust"]});
        update_repetition_tracker(&mut tracker, &miss, tools::CODE_SEARCH, &args);
        update_repetition_tracker(&mut tracker, &miss, tools::CODE_SEARCH, &args);
        update_repetition_tracker(&mut tracker, &miss, tools::CODE_SEARCH, &args);

        assert_eq!(tracker.max_low_signal_count(), 3);
    }

    #[test]
    fn low_signal_tracker_ignores_empty_search_results_with_recovery_guidance() {
        let mut tracker = LoopTracker::new();
        let guided = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({
                "results": [],
                "hint": "Try narrowing the path.",
                "is_recoverable": true,
                "next_action": "Retry with narrower filters."
            }),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(
            &mut tracker,
            &guided,
            tools::CODE_SEARCH,
            &json!({"query":"run", "path":"src/agent", "result_types":["definition"]}),
        );

        assert_eq!(tracker.max_low_signal_count(), 0);
    }

    #[test]
    fn low_signal_tracker_does_not_hide_structured_search_errors_as_empty_results() {
        let mut tracker = LoopTracker::new();
        let failure_like = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({
                "results": [],
                "error": "permission denied while searching the workspace"
            }),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        update_repetition_tracker(
            &mut tracker,
            &failure_like,
            tools::CODE_SEARCH,
            &json!({"query":"secret", "path":"src"}),
        );

        assert_eq!(tracker.max_low_signal_count(), 0);
        assert_eq!(tracker.consecutive_low_signal_navigations, 0);
    }

    #[test]
    fn low_signal_tracker_counts_missing_read_failures() {
        let mut tracker = LoopTracker::new();
        let miss = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::UNIFIED_FILE.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ResourceNotFound,
                "Resource not found: vtcode-tui/src/main.rs".to_string(),
            ),
        });

        // Two reads of the same path with different offsets are *different*
        // slices (paginated exploration), not a retry loop. The slice-aware
        // family key keeps them as distinct families, each with count 1.
        // Regression: previously both collapsed into one family with count 2,
        // which falsely tripped the family cap when the model paginated a
        // missing file (checkpoint turn_613 pattern).
        update_repetition_tracker(
            &mut tracker,
            &miss,
            tools::UNIFIED_FILE,
            &json!({"action":"read","path":"vtcode-tui/src/main.rs"}),
        );
        update_repetition_tracker(
            &mut tracker,
            &miss,
            tools::UNIFIED_FILE,
            &json!({"action":"read","path":"vtcode-tui/src/main.rs","offset":40}),
        );

        assert_eq!(
            tracker.max_low_signal_count(),
            1,
            "paginated reads (different offset) must be distinct families, not one family with count 2"
        );
    }

    #[test]
    fn low_signal_tracker_counts_identical_missing_read_failures() {
        // True retry loop: same path + same slice, repeated. The low-signal
        // count must accumulate so the turn balancer can stop the churn.
        let mut tracker = LoopTracker::new();
        let miss = ToolPipelineOutcome::from_status(ToolExecutionStatus::Failure {
            error: vtcode_core::tools::registry::ToolExecutionError::new(
                tools::UNIFIED_FILE.to_string(),
                vtcode_core::tools::registry::ToolErrorType::ResourceNotFound,
                "Resource not found: vtcode-tui/src/main.rs".to_string(),
            ),
        });

        let identical_args = json!({"action":"read","path":"vtcode-tui/src/main.rs"});
        update_repetition_tracker(&mut tracker, &miss, tools::UNIFIED_FILE, &identical_args);
        update_repetition_tracker(&mut tracker, &miss, tools::UNIFIED_FILE, &identical_args);

        assert_eq!(
            tracker.max_low_signal_count(),
            2,
            "identical retry reads must accumulate into one family with count 2"
        );
    }

    #[test]
    fn low_signal_tracker_counts_grep_style_shell_misses() {
        let mut tracker = LoopTracker::new();
        let miss = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({
                "command": "grep -n 'missing' vtcode-tui/src/main.rs",
                "exit_code": 1,
                "output": ""
            }),
            stdout: None,
            modified_files: vec![],
            command_success: false,
        });
        update_repetition_tracker(
            &mut tracker,
            &miss,
            tools::EXEC_COMMAND,
            &json!({"cmd":"grep -n 'missing' vtcode-tui/src/main.rs"}),
        );
        update_repetition_tracker(
            &mut tracker,
            &miss,
            tools::EXEC_COMMAND,
            &json!({"cmd":"grep -n \"missing\" vtcode-tui/src/main.rs"}),
        );

        assert_eq!(tracker.max_low_signal_count(), 2);
        assert_eq!(tracker.consecutive_low_signal_navigations, 2);
        assert_eq!(tracker.total_low_signal_navigations, 2);
    }

    #[test]
    fn low_signal_tracker_does_not_count_grep_style_errors_as_no_match() {
        let mut tracker = LoopTracker::new();
        let error = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({
                "command": "rg missing restricted",
                "exit_code": 2,
                "output": ""
            }),
            stdout: None,
            modified_files: vec![],
            command_success: false,
        });

        update_repetition_tracker(&mut tracker, &error, tools::EXEC_COMMAND, &json!({"cmd":"rg missing restricted"}));

        assert_eq!(tracker.max_low_signal_count(), 0);
        assert_eq!(tracker.consecutive_low_signal_navigations, 0);
        assert_eq!(tracker.total_low_signal_navigations, 0);
    }

    #[test]
    fn low_signal_tracker_does_not_hide_grep_errors_as_no_match() {
        let mut tracker = LoopTracker::new();
        let failure = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({
                "command": "rg missing restricted",
                "exit_code": 1,
                "stdout": "",
                "stderr": "permission denied",
            }),
            stdout: None,
            modified_files: vec![],
            command_success: false,
        });

        update_repetition_tracker(&mut tracker, &failure, tools::EXEC_COMMAND, &json!({"cmd":"rg missing restricted"}));

        assert_eq!(tracker.max_low_signal_count(), 0);
        assert_eq!(tracker.consecutive_low_signal_navigations, 0);
    }

    // --- read_normalized_signature_key tests ---

    #[test]
    fn read_normalized_signature_key_normalizes_file_operation_read_offset() {
        let args_a = json!({"action": "read", "path": "src/lib.rs", "offset": 0, "limit": 100});
        let args_b = json!({"action": "read", "path": "src/lib.rs", "offset": 50, "limit": 200});
        let key_a = read_normalized_signature_key("file_operation", &args_a);
        let key_b = read_normalized_signature_key("file_operation", &args_b);
        assert_eq!(key_a, key_b, "same file read with different offset/limit should produce the same normalized key");
    }

    #[test]
    fn read_normalized_signature_key_preserves_encoding() {
        let utf8 = json!({"action": "read", "path": "src/lib.rs", "encoding": "utf8"});
        let base64 = json!({"action": "read", "path": "src/lib.rs", "encoding": "base64"});

        assert_ne!(
            read_normalized_signature_key("file_operation", &utf8),
            read_normalized_signature_key("file_operation", &base64),
            "different encodings produce different tool output and must not reuse one another"
        );
    }

    #[test]
    fn read_normalized_signature_key_differentiates_different_paths() {
        let args_a = json!({"action": "read", "path": "src/lib.rs"});
        let args_b = json!({"action": "read", "path": "src/main.rs"});
        let key_a = read_normalized_signature_key("file_operation", &args_a);
        let key_b = read_normalized_signature_key("file_operation", &args_b);
        assert_ne!(key_a, key_b, "different paths must produce different keys");
    }

    #[test]
    fn read_normalized_signature_key_includes_code_search_limit_and_normalises_filter_order() {
        let args_a = json!({
            "query": "Widget",
            "path": "src",
            "file_types": ["rust", "typescript"],
            "result_types": ["text", "definition"],
            "max_results": 10
        });
        let args_b = json!({
            "query": "Widget",
            "path": "src",
            "file_types": ["typescript", "rs"],
            "result_types": ["definition", "text"],
            "max_results": 100
        });
        let key_a = read_normalized_signature_key(tools::CODE_SEARCH, &args_a);
        let key_b = read_normalized_signature_key(tools::CODE_SEARCH, &args_b);
        assert_ne!(key_a, key_b, "different effective limits must not share one code-search replay identity");

        let args_default = json!({
            "query": " Widget ",
            "path": "src",
            "file_types": ["rs", "typescript"],
            "result_types": ["definition", "text"]
        });
        let args_explicit_default = json!({
            "query": "Widget",
            "path": "src",
            "file_types": ["typescript", "rust"],
            "result_types": ["text", "definition"],
            "max_results": 20
        });
        assert_eq!(
            read_normalized_signature_key(tools::CODE_SEARCH, &args_default),
            read_normalized_signature_key(tools::CODE_SEARCH, &args_explicit_default),
            "omitted and explicit default limits must share replay identity"
        );
    }

    #[test]
    fn read_normalized_signature_key_preserves_mutation_for_write() {
        let args_a = json!({"path": "src/lib.rs", "content": "old"});
        let args_b = json!({"path": "src/lib.rs", "content": "new"});
        let key_a = read_normalized_signature_key("file_operation", &args_a);
        let key_b = read_normalized_signature_key("file_operation", &args_b);
        assert_ne!(key_a, key_b, "mutating writes must NOT be normalized away");
    }

    #[test]
    fn find_duplicate_in_history_matches_normalized_read() {
        use vtcode_core::llm::provider as uni;

        // find_duplicate_in_history uses read_normalized_signature_key, which
        // strips offset/limit for file reads. A later unrelated Assistant batch
        // must not obscure the earlier matching call and result pair.

        // Verify normalization: same file + different offset/limit → same key
        let key_a = read_normalized_signature_key(
            tools::UNIFIED_FILE,
            &json!({"action":"read","path":"src/lib.rs","offset":0,"limit":100}),
        );
        let key_b = read_normalized_signature_key(
            tools::UNIFIED_FILE,
            &json!({"action":"read","path":"src/lib.rs","offset":50,"limit":500}),
        );
        assert_eq!(key_a, key_b, "same file read with different offset/limit should normalize to the same key");

        // Verify: different file → different key
        let key_c = read_normalized_signature_key(
            tools::UNIFIED_FILE,
            &json!({"action":"read","path":"src/main.rs","offset":0,"limit":100}),
        );
        assert_ne!(key_a, key_c, "different files must produce different normalized keys");

        // Verify: code-search result limits remain distinct while filter ordering normalises away.
        let s_key_a = read_normalized_signature_key(
            tools::CODE_SEARCH,
            &json!({"query":"Widget","path":"src","file_types":["rust","typescript"],"result_types":["text","definition"],"max_results":10}),
        );
        let s_key_b = read_normalized_signature_key(
            tools::CODE_SEARCH,
            &json!({"query":"Widget","path":"src","file_types":["typescript","rs"],"result_types":["definition","text"],"max_results":100}),
        );
        assert_ne!(s_key_a, s_key_b, "different effective limits must not share one code-search replay identity");

        // Verify: write NOT normalized
        let w_key_a = read_normalized_signature_key(
            tools::UNIFIED_FILE,
            &json!({"action":"write","path":"src/lib.rs","content":"old"}),
        );
        let w_key_b = read_normalized_signature_key(
            tools::UNIFIED_FILE,
            &json!({"action":"write","path":"src/lib.rs","content":"new"}),
        );
        assert_ne!(w_key_a, w_key_b, "writes must not be normalized away");

        // Verify: find_duplicate_in_history still works for EXACT match
        let mut history: Vec<uni::Message> = Vec::new();
        history.push(uni::Message::assistant_with_tools(
            "read".into(),
            vec![uni::ToolCall::function(
                "tc_exact".into(),
                tools::UNIFIED_FILE.into(),
                serde_json::to_string(&json!({"action":"read","path":"src/lib.rs","offset":0,"limit":100})).unwrap(),
            )],
        ));
        history.push(uni::Message {
            role: uni::MessageRole::Tool,
            content: uni::MessageContent::text("exact content".into()),
            tool_call_id: Some("tc_exact".into()),
            ..Default::default()
        });
        // Second pair (different file) so the scan finds A₀'s Tool after A₁:
        history.push(uni::Message::assistant_with_tools(
            "read other".into(),
            vec![uni::ToolCall::function(
                "tc_other".into(),
                tools::UNIFIED_FILE.into(),
                serde_json::to_string(&json!({"action":"read","path":"src/main.rs"})).unwrap(),
            )],
        ));
        history.push(uni::Message {
            role: uni::MessageRole::Tool,
            content: uni::MessageContent::text("other content".into()),
            tool_call_id: Some("tc_other".into()),
            ..Default::default()
        });

        let result = find_duplicate_in_history(
            &history,
            tools::UNIFIED_FILE,
            &json!({"action":"read","path":"src/lib.rs","offset":0,"limit":50}),
            Path::new("."),
        );
        assert_eq!(result.as_deref(), Some("exact content"));
    }

    #[test]
    fn find_duplicate_in_history_respects_normalised_code_search_limit() {
        let original_args = json!({
            "query": "Widget",
            "path": "src",
            "file_types": ["rust", "typescript"],
            "result_types": ["text", "definition"],
            "max_results": 10
        });
        let history = vec![
            uni::Message::assistant_with_tools(
                "search".into(),
                vec![uni::ToolCall::function(
                    "tc_search".into(),
                    tools::CODE_SEARCH.into(),
                    serde_json::to_string(&original_args).unwrap(),
                )],
            ),
            uni::Message {
                role: uni::MessageRole::Tool,
                content: uni::MessageContent::text("{\"results\":[]}".into()),
                tool_call_id: Some("tc_search".into()),
                ..Default::default()
            },
        ];

        let different_limit = find_duplicate_in_history(
            &history,
            tools::CODE_SEARCH,
            &json!({
                "query": "Widget",
                "path": "src",
                "file_types": ["typescript", "rs"],
                "result_types": ["definition", "text"],
                "max_results": 100
            }),
            Path::new("."),
        );

        assert_eq!(different_limit, None);

        let equivalent_default_history = vec![
            uni::Message::assistant_with_tools(
                "search".into(),
                vec![uni::ToolCall::function(
                    "tc_default".into(),
                    tools::CODE_SEARCH.into(),
                    serde_json::to_string(&json!({
                        "query": "Widget",
                        "path": "src",
                        "max_results": 20
                    }))
                    .unwrap(),
                )],
            ),
            uni::Message {
                role: uni::MessageRole::Tool,
                content: uni::MessageContent::text("{\"results\":[1]}".into()),
                tool_call_id: Some("tc_default".into()),
                ..Default::default()
            },
        ];
        let reused = find_duplicate_in_history(
            &equivalent_default_history,
            tools::CODE_SEARCH,
            &json!({"query": " Widget ", "path": "src"}),
            Path::new("."),
        );
        assert_eq!(reused.as_deref(), Some("{\"results\":[1]}"));
    }

    #[test]
    fn working_history_code_search_replay_stops_at_in_scope_mutation() {
        let search_args = json!({"query": "Widget", "path": "src"});
        let search_call = uni::Message::assistant_with_tools(
            "search".into(),
            vec![uni::ToolCall::function(
                "search_call".into(),
                tools::CODE_SEARCH.into(),
                serde_json::to_string(&search_args).unwrap(),
            )],
        );
        let search_result = uni::Message {
            role: uni::MessageRole::Tool,
            content: uni::MessageContent::text("{\"results\":[\"cached\"]}".into()),
            tool_call_id: Some("search_call".into()),
            ..Default::default()
        };
        let mutation = |path: &str, result: serde_json::Value| {
            let patch = format!("*** Begin Patch\n*** Update File: {path}\n@@\n-Widget\n+Gadget\n*** End Patch\n");
            vec![
                uni::Message::assistant_with_tools(
                    "edit".into(),
                    vec![uni::ToolCall::function(
                        "edit_call".into(),
                        tools::APPLY_PATCH.into(),
                        serde_json::to_string(&json!({"patch": patch})).unwrap(),
                    )],
                ),
                uni::Message::tool_response("edit_call".into(), result.to_string()),
            ]
        };

        let mut in_scope_history = vec![search_call.clone(), search_result.clone()];
        in_scope_history.extend(mutation("src/widget.rs", json!({"success": true})));
        assert!(
            find_duplicate_in_history(&in_scope_history, tools::CODE_SEARCH, &search_args, Path::new("."),).is_none(),
            "editing src/widget.rs after searching src must force a fresh search"
        );

        let mut status_success_history = vec![search_call.clone(), search_result.clone()];
        status_success_history
            .extend(mutation("src/widget.rs", json!({"status": "success", "output": "patch applied"})));
        assert!(
            find_duplicate_in_history(&status_success_history, tools::CODE_SEARCH, &search_args, Path::new("."),)
                .is_none(),
            "the established successful status shape must invalidate replay"
        );

        let mut unrelated_history = vec![search_call.clone(), search_result.clone()];
        unrelated_history.extend(mutation("tests/widget.rs", json!({"success": true})));
        assert_eq!(
            find_duplicate_in_history(&unrelated_history, tools::CODE_SEARCH, &search_args, Path::new("."),).as_deref(),
            Some("{\"results\":[\"cached\"]}"),
            "an unrelated edit may reuse the prior scoped search"
        );

        for failure in [
            json!({"success": false, "error": "patch rejected"}),
            json!({"error": {"message": "execution denied by policy"}}),
            json!({"failure_kind": "timeout"}),
            json!({"status": "failed"}),
            json!({"status": "denied"}),
            json!({"success": null}),
            json!({"output": "patch output without an outcome"}),
            json!(["non-object mutation output"]),
        ] {
            let mut failed_history = vec![search_call.clone(), search_result.clone()];
            failed_history.extend(mutation("src/widget.rs", failure));
            assert_eq!(
                find_duplicate_in_history(&failed_history, tools::CODE_SEARCH, &search_args, Path::new("."),)
                    .as_deref(),
                Some("{\"results\":[\"cached\"]}"),
                "a mutation without explicit positive success evidence must preserve reuse"
            );
        }

        let mut unexecuted_history = vec![search_call, search_result];
        let unexecuted_mutation = mutation("src/widget.rs", json!({"success": true}));
        unexecuted_history.push(unexecuted_mutation[0].clone());
        assert_eq!(
            find_duplicate_in_history(&unexecuted_history, tools::CODE_SEARCH, &search_args, Path::new("."),)
                .as_deref(),
            Some("{\"results\":[\"cached\"]}"),
            "an unexecuted mutation call must preserve reuse"
        );
    }

    #[test]
    fn mutation_tool_response_success_rejects_malformed_and_conflicting_shapes() {
        let response = |content: &str| uni::Message::tool_response("edit_call".into(), content.into());

        assert!(tool_response_is_success(&response(r#"{"success":true}"#)));
        assert!(tool_response_is_success(&response(r#"{"status":"success","output":"patch applied"}"#,)));

        for content in [
            "not json",
            "null",
            r#"{"success":null,"status":"success"}"#,
            r#"{"success":true,"status":"failed"}"#,
            r#"{"success":true,"failure_kind":"timeout"}"#,
            r#"{"success":true,"error":"execution denied"}"#,
        ] {
            assert!(
                !tool_response_is_success(&response(content)),
                "mutation outcome must not count as successful: {content}"
            );
        }
    }

    #[test]
    fn duplicate_history_reuse_rejects_failed_results() {
        let args = json!({"query": "needle", "path": "src"});
        let call = || {
            uni::Message::assistant_with_tools(
                "search".into(),
                vec![uni::ToolCall::function(
                    "search_call".into(),
                    tools::CODE_SEARCH.into(),
                    serde_json::to_string(&args).unwrap(),
                )],
            )
        };

        for failure in [
            r#"{"success":false,"output":"partial"}"#,
            r#"{"status":"timeout","output":"partial"}"#,
            r#"{"error":"permission denied"}"#,
            "Error: command failed",
            "timed out while reading",
            "failed to execute command",
            "denied by policy",
            "blocked until verification",
            "not executed",
        ] {
            let history = vec![
                call(),
                uni::Message::tool_response("search_call".into(), failure.into()),
            ];
            assert!(
                find_duplicate_in_history(&history, tools::CODE_SEARCH, &args, Path::new(".")).is_none(),
                "failed result must not be replayed: {failure}"
            );
        }

        for success in [r#"{"results":[]}"#, "[]", "plain successful output"] {
            let history = vec![
                call(),
                uni::Message::tool_response("search_call".into(), success.into()),
            ];
            assert_eq!(
                find_duplicate_in_history(&history, tools::CODE_SEARCH, &args, Path::new(".")).as_deref(),
                Some(success)
            );
        }
    }

    #[test]
    fn working_history_code_search_replay_rejects_reused_patch_call_id() {
        let search_args = json!({"query": "Widget", "path": "src"});
        let shared_call_id = "call_0";
        let search_call = uni::Message::assistant_with_tools(
            "search".into(),
            vec![uni::ToolCall::function(
                shared_call_id.into(),
                tools::CODE_SEARCH.into(),
                serde_json::to_string(&search_args).unwrap(),
            )],
        );
        let search_result =
            uni::Message::tool_response(shared_call_id.into(), "{\"results\":[\"genuine search output\"]}".into());
        let patch = "*** Begin Patch\n*** Update File: src/widget.rs\n@@\n-Widget\n+Gadget\n*** End Patch\n";
        let patch_call = uni::Message::assistant_with_tools(
            "edit".into(),
            vec![uni::ToolCall::function(
                shared_call_id.into(),
                tools::APPLY_PATCH.into(),
                serde_json::to_string(&json!({"patch": patch})).unwrap(),
            )],
        );

        let mut successful_history = vec![
            search_call.clone(),
            search_result.clone(),
            patch_call.clone(),
            uni::Message::tool_response(
                shared_call_id.into(),
                json!({"success": true, "output": "patch output"}).to_string(),
            ),
        ];
        assert!(
            find_duplicate_in_history(&successful_history, tools::CODE_SEARCH, &search_args, Path::new("."),).is_none(),
            "a successful in-scope patch must invalidate the genuine earlier search result"
        );

        successful_history.pop();
        successful_history.push(uni::Message::tool_response(
            shared_call_id.into(),
            json!({"success": false, "error": "patch rejected", "output": "patch output"}).to_string(),
        ));
        assert_eq!(
            find_duplicate_in_history(&successful_history, tools::CODE_SEARCH, &search_args, Path::new("."),)
                .as_deref(),
            Some("{\"results\":[\"genuine search output\"]}"),
            "a failed patch must preserve the earlier search without returning patch output"
        );
    }

    #[test]
    fn read_extent_covers_query_rejects_larger_limit() {
        // Cached limit=200 must NOT cover query limit=220
        assert!(!read_extent::extent_covers(
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200}),
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":220}),
        ));

        // Cached limit=200 covers query limit=200 (same)
        assert!(read_extent::extent_covers(
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200}),
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200}),
        ));

        // Cached limit=200 covers query limit=100 (subset)
        assert!(read_extent::extent_covers(
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200}),
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":100}),
        ));

        // Different offset must not match
        assert!(!read_extent::extent_covers(
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200}),
            &json!({"action":"read","path":"AGENTS.md","offset":50,"limit":200}),
        ));
    }

    #[test]
    fn read_extent_covers_query_rejects_different_raw_mode() {
        // Non-raw cached must NOT cover raw=true query
        assert!(!read_extent::extent_covers(
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200}),
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200,"raw":true}),
        ));

        // Raw cached covers raw query
        assert!(read_extent::extent_covers(
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200,"raw":true}),
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200,"raw":true}),
        ));

        // Raw cached must NOT cover non-raw query
        assert!(!read_extent::extent_covers(
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200,"raw":true}),
            &json!({"action":"read","path":"AGENTS.md","offset":0,"limit":200}),
        ));
    }

    #[test]
    fn read_extent_covers_query_handles_missing_limit() {
        // Both missing limit → matches (same default read)
        assert!(read_extent::extent_covers(
            &json!({"action":"read","path":"AGENTS.md"}),
            &json!({"action":"read","path":"AGENTS.md"}),
        ));

        // Cached has limit, query doesn't → mismatch
        assert!(!read_extent::extent_covers(
            &json!({"action":"read","path":"AGENTS.md","limit":200}),
            &json!({"action":"read","path":"AGENTS.md"}),
        ));

        // Cached has no limit, query does → mismatch
        assert!(!read_extent::extent_covers(
            &json!({"action":"read","path":"AGENTS.md","limit":200}),
            &json!({"action":"read","path":"AGENTS.md"}),
        ));
    }

    fn successful_exec_output() -> ToolPipelineOutcome {
        ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({}),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        })
    }

    #[test]
    fn coarse_listing_count_groups_same_root_rescans() {
        let mut tracker = LoopTracker::new();
        assert_eq!(tracker.max_coarse_listing_count(), 0);
        // Same root across flag and quote variations: one coarse family.
        for command in ["ls src", "ls \"src\"", "ls -1 src/"] {
            update_repetition_tracker(
                &mut tracker,
                &successful_exec_output(),
                tools::EXEC_COMMAND,
                &json!({"cmd":command}),
            );
        }
        assert_eq!(tracker.max_coarse_listing_count(), 3);
        assert_eq!(tracker.dominant_churn(), Some(("exec::inspection::ls::src".to_string(), 3)));
    }

    #[test]
    fn coarse_listing_count_separates_distinct_roots() {
        let mut tracker = LoopTracker::new();
        // Distinct trees are legitimate exploration, not churn.
        for command in ["ls src", "ls crates", "ls tests"] {
            update_repetition_tracker(
                &mut tracker,
                &successful_exec_output(),
                tools::EXEC_COMMAND,
                &json!({"cmd":command}),
            );
        }
        assert_eq!(tracker.max_coarse_listing_count(), 1);
    }

    #[test]
    fn coarse_listing_count_keeps_binaries_in_separate_families() {
        let mut tracker = LoopTracker::new();
        for command in ["ls src", "find crates -name lib.rs", "fd main src"] {
            update_repetition_tracker(
                &mut tracker,
                &successful_exec_output(),
                tools::EXEC_COMMAND,
                &json!({"cmd":command}),
            );
        }
        assert_eq!(tracker.max_coarse_listing_count(), 1);
    }

    #[test]
    fn coarse_listing_count_ignores_grep_style_searches() {
        let mut tracker = LoopTracker::new();
        for command in ["rg foo src", "rg bar crates", "grep -r baz src"] {
            update_repetition_tracker(
                &mut tracker,
                &successful_exec_output(),
                tools::EXEC_COMMAND,
                &json!({"cmd":command}),
            );
        }
        assert_eq!(tracker.max_coarse_listing_count(), 0);
        // `rg`/`grep` must not enter the coarse ledger at all, so they can
        // never be promoted to low-signal or named as dominant churn.
        assert_eq!(tracker.max_low_signal_count(), 0);
        assert_eq!(tracker.dominant_churn(), None);
        assert_eq!(tracker.low_signal_tool_calls, 0);
    }

    #[test]
    fn same_pattern_grep_searches_do_not_promote_to_low_signal() {
        // Regression for turn_1303/turn_1304 (`exec::inspection::grep::enum ×5`)
        // and turn_1291 (`exec::inspection::rg::pub ×5`): five distinct
        // successful searches sharing one pattern (`enum` / `pub`) across
        // different files/flags are legitimate research, not churn. They must
        // not be promoted into the low-signal ledger and must not trip early
        // recovery on their own.
        let mut tracker = LoopTracker::new();
        for command in [
            "grep -n \"enum Commands\" -A 80 src/cli/mod.rs",
            "grep -n \"enum Command\\|pub enum\" src/cli/mod.rs",
            "grep -rn \"enum Commands\" crates/codegen/vtcode-core/src",
            "grep -rn \"enum ExecSubcommand\" -A 30 crates/codegen/vtcode-core/src/cli/args/",
            "grep -rn \"enum ScheduleSubcommand\" crates/codegen/vtcode-core/src/cli/args/",
        ] {
            update_repetition_tracker(
                &mut tracker,
                &successful_exec_output(),
                tools::EXEC_COMMAND,
                &json!({"cmd":command}),
            );
        }
        assert_eq!(tracker.max_coarse_listing_count(), 0);
        assert_eq!(tracker.max_low_signal_count(), 0);
        assert_eq!(tracker.dominant_churn(), None);

        let mut tracker = LoopTracker::new();
        for command in [
            "rg -n 'pub enum Commands' src/ -A 40",
            "rg -n 'pub enum Commands' crates/codegen/vtcode-core/src/cli/args/mod.rs -A 50",
            "rg -n 'pub enum Commands' crates/codegen/vtcode-core/src/cli/args/mod.rs -A 600",
            "rg -n 'pub enum Provider|Gemini|OpenAI' crates/codegen/vtcode-llm/src",
            "rg -n 'pub enum SecretCommand|Add|List' crates/codegen/vtcode-core/src/cli/args/secret.rs",
        ] {
            update_repetition_tracker(
                &mut tracker,
                &successful_exec_output(),
                tools::EXEC_COMMAND,
                &json!({"cmd":command}),
            );
        }
        assert_eq!(tracker.max_coarse_listing_count(), 0);
        assert_eq!(tracker.max_low_signal_count(), 0);
        assert_eq!(tracker.dominant_churn(), None);
    }

    #[test]
    fn coarse_inspection_root_extracts_first_positional_token() {
        assert_eq!(coarse_inspection_root("ls src/"), "src");
        assert_eq!(coarse_inspection_root("ls \"src/\""), "src");
        assert_eq!(coarse_inspection_root("find src-tauri/ -maxdepth 1"), "src-tauri");
        assert_eq!(coarse_inspection_root("ls -la"), ".");
        assert_eq!(coarse_inspection_root("ls"), ".");
        // Heuristic: an option value can be picked up as the root, which only
        // fragments families and keeps detection conservative.
        assert_eq!(coarse_inspection_root("ls --width 80 src"), "80");
    }

    #[test]
    fn promoted_listing_repeat_counts_toward_low_signal_telemetry() {
        // Three same-root successful listings: the third is promoted into the
        // low-signal ledger, so telemetry records exactly one low-signal call
        // even though every listing succeeded (the old
        // `low_signal_tool_calls:0 on 3×find` diagnostics gap).
        let mut tracker = LoopTracker::new();
        for command in ["ls src", "ls -1 src", "ls src/"] {
            update_repetition_tracker(
                &mut tracker,
                &successful_exec_output(),
                tools::EXEC_COMMAND,
                &json!({"cmd":command}),
            );
        }
        assert_eq!(tracker.low_signal_tool_calls, 1);
        assert_eq!(tracker.total_low_signal_navigations, 1);
        assert_eq!(tracker.consecutive_low_signal_navigations, 1);
        assert_eq!(tracker.max_coarse_listing_count(), 3);
    }
}
