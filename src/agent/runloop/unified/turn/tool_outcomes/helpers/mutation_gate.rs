//! Mutation gate: docs-only/plan-artifact classification and verification blocking.

use super::*;

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
