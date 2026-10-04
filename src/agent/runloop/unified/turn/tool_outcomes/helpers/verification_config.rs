//! Verification-gate configuration accessors and gate constants.

use super::*;

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

pub(crate) fn resolve_max_tool_retries(
    _tool_name: &str,
    vt_cfg: Option<&vtcode_core::config::loader::VTCodeConfig>,
) -> usize {
    vt_cfg
        .map(|cfg| cfg.agent.harness.max_tool_retries as usize)
        .unwrap_or(vtcode_config::constants::defaults::DEFAULT_MAX_TOOL_RETRIES as usize)
}
