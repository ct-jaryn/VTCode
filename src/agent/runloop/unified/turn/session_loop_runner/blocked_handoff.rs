use std::path::Path;

use vtcode_core::core::agent::blocked_handoff::{BlockedHandoffResume, write_blocked_handoff_with_resume};
use vtcode_core::core::agent::harness_artifacts::existing_harness_artifact_paths;
use vtcode_core::core::agent::snapshots::SnapshotTurnDiagnostics;
use vtcode_core::exec::events::HarnessEventKind;
use vtcode_core::utils::ansi::{AnsiRenderer, MessageStyle};
use vtcode_core::utils::session_archive::{
    SessionArchive, SessionProgressArgs, SessionProgressPersistenceStatus, VerifiedSessionArchiveIdentifier,
};

use crate::agent::runloop::unified::inline_events::harness::{HarnessEventEmitter, harness_event};

const NO_ARCHIVE_RESUME_EXPLANATION: &str = "Resume is unavailable because no session archive exists.";
const UNVERIFIED_RESUME_EXPLANATION: &str = "Resume is unavailable because the session archive could not be verified.";

/// Upper bound (in chars) for the block reason shown in transcript lines.
/// Provider errors can flood the transcript, so the renderer shows a bounded
/// prefix plus a pointer to the handoff file; the handoff markdown keeps the
/// full reason unchanged.
const TRANSCRIPT_BLOCK_REASON_LIMIT: usize = 600;

/// Upper bound (in chars) for the appended last-turn diagnostics footer in
/// the handoff markdown. The canonical `events.jsonl` remains the
/// full-fidelity source; the footer only carries the counts needed to
/// triage without opening the log.
const BLOCKED_DIAGNOSTICS_FOOTER_LIMIT: usize = 1200;

/// Plan-mode blocked-turn header: plan mode is read-only by design, so a
/// `Turn blocked` / `Mutation blocked` there is a policy stop, not a
/// transient failure. Retrying the same mutating tools re-blocks.
const PLAN_MODE_MUTATION_BLOCK_HEADER: &str = "Plan mode is read-only — edits block by design (not a failing check):";
const PLAN_MODE_TURN_BLOCKED_HEADER: &str = "You're in plan mode (read-only) — retrying the same tools will re-block:";

/// Returns true when a blocker summary describes a mutation/policy stop
/// rather than a generic turn-loop stop. Used to pick plan-mode copy:
/// mutation stops need the read-only explanation, generic stops need the
/// re-block warning. Only the two canonical markers are matched: the
/// verification-gate `"mutation blocked"` prefix and the planning-gate
/// `"tool denied by planning workflow"` context. Broader substrings like
/// `"mutating"` or `"read-only"` are intentionally excluded — they appear in
/// unrelated verifier output (e.g. `read-only file system`) and in fuse-trip
/// reasons, where the generic header is the correct choice.
pub(super) fn is_plan_mode_mutation_block(blocker_summary: &str) -> bool {
    let lowered = blocker_summary.to_ascii_lowercase();
    lowered.contains("mutation blocked") || lowered.contains("tool denied by planning workflow")
}

/// Transcript guidance lines for a blocked turn while planning is active.
/// Index 0 is the header, 1 stays planning, 2 implements. Kept as static
/// strings so transcript rendering stays bounded and unit-testable.
/// Build is the default destination (confirmation-aware); Auto only changes
/// confirmation policy, not authority or safety gates.
pub(super) fn plan_mode_switch_guidance_lines(is_mutation_block: bool) -> [&'static str; 3] {
    if is_mutation_block {
        [
            PLAN_MODE_MUTATION_BLOCK_HEADER,
            "  • Stay planning: type `continue` to keep researching toward `<proposed_plan>`",
            "  • Implement now: approve the plan or run `/mode build` (`/mode auto` for unattended; Build stays confirmation-aware)",
        ]
    } else {
        [
            PLAN_MODE_TURN_BLOCKED_HEADER,
            "  • Stay planning: type `continue` to resume research",
            "  • Implement now: approve the plan or run `/mode build` (`/mode auto` for unattended)",
        ]
    }
}

/// Build the `# Last-Turn Diagnostics` footer from the turn snapshot and the
/// session tool set. Returns an empty string when there is nothing
/// meaningful to report, so callers can append unconditionally.
pub(super) fn blocked_diagnostics_footer(
    diagnostics: Option<&SnapshotTurnDiagnostics>,
    distinct_tools: &[String],
) -> String {
    let Some(diagnostics) = diagnostics else {
        return String::new();
    };
    let mut lines = Vec::with_capacity(6);
    lines.push(format!("Elapsed: {}ms", diagnostics.elapsed_ms));
    if !distinct_tools.is_empty() {
        let mut tools = distinct_tools.to_vec();
        tools.sort();
        tools.dedup();
        let mut joined = tools.join(", ");
        if joined.chars().count() > 300 {
            joined = format!("{}… ({} tools)", joined.chars().take(300).collect::<String>(), tools.len());
        }
        lines.push(format!("Tools used this session ({}): {joined}", tools.len()));
    }
    lines.push(format!(
        "Tool calls: requested={} admitted={} failed={} denied={} preflight_failures={} reused={}",
        diagnostics.requested_tool_calls,
        diagnostics.admitted_tool_calls,
        diagnostics.failed_tool_calls,
        diagnostics.denied_tool_calls,
        diagnostics.preflight_failures,
        diagnostics.reused_results,
    ));
    if diagnostics.model_visible_tool_preview_budget_exhausted || diagnostics.suppressed_tool_previews > 0 {
        lines.push(format!(
            "Preview budget exhausted: {} (suppressed previews: {})",
            diagnostics.model_visible_tool_preview_budget_exhausted, diagnostics.suppressed_tool_previews,
        ));
    }
    let usage = &diagnostics.usage;
    if usage.input_tokens > 0 || usage.output_tokens > 0 {
        lines.push(format!(
            "Turn usage: prompt={} cached={} completion={}",
            usage.input_tokens, usage.cached_input_tokens, usage.output_tokens,
        ));
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut footer = format!("\n\n# Last-Turn Diagnostics\n\n{}", lines.join("\n"));
    if footer.chars().count() > BLOCKED_DIAGNOSTICS_FOOTER_LIMIT {
        footer = footer.chars().take(BLOCKED_DIAGNOSTICS_FOOTER_LIMIT).collect();
    }
    footer
}

/// Append the diagnostics footer to a blocker summary, keeping the result
/// bounded. The transcript renderer truncates separately.
pub(super) fn blocker_summary_with_diagnostics(
    reason: &str,
    diagnostics: Option<&SnapshotTurnDiagnostics>,
    distinct_tools: &[String],
) -> String {
    let footer = blocked_diagnostics_footer(diagnostics, distinct_tools);
    if footer.is_empty() {
        return reason.to_string();
    }
    let mut summary = String::with_capacity(reason.len() + footer.len());
    summary.push_str(reason);
    summary.push_str(&footer);
    summary
}

/// Bound the block reason for transcript rendering. When the summary exceeds
/// [`TRANSCRIPT_BLOCK_REASON_LIMIT`] chars it is truncated and suffixed with
/// an ellipsis plus the handoff path that holds the full text.
fn truncated_block_reason(summary: &str, full_reason_path: &str) -> String {
    let suffix = format!("… — full reason: {full_reason_path}");
    let suffix_len = suffix.chars().count();
    let summary_len = summary.chars().count();
    if summary_len + suffix_len <= TRANSCRIPT_BLOCK_REASON_LIMIT {
        return summary.to_string();
    }
    let keep = TRANSCRIPT_BLOCK_REASON_LIMIT.saturating_sub(suffix_len);
    let mut bounded: String = summary.chars().take(keep).collect();
    bounded.push_str(&suffix);
    bounded
}

#[derive(Debug)]
enum ResumeAvailability {
    Available(VerifiedSessionArchiveIdentifier),
    Unavailable(String),
}

impl ResumeAvailability {
    fn as_handoff_resume(&self) -> BlockedHandoffResume<'_> {
        match self {
            Self::Available(identifier) => BlockedHandoffResume::Available(identifier),
            Self::Unavailable(explanation) => BlockedHandoffResume::Unavailable(explanation),
        }
    }
}

pub(super) struct SessionCheckpointOutcome {
    history_checkpoint_succeeded: bool,
    history_persistence_disabled: bool,
    blocked_resume: Option<ResumeAvailability>,
}

impl SessionCheckpointOutcome {
    fn new(blocked_turn: bool) -> Self {
        Self {
            history_checkpoint_succeeded: false,
            history_persistence_disabled: false,
            blocked_resume: blocked_turn.then(|| ResumeAvailability::Unavailable(UNVERIFIED_RESUME_EXPLANATION.into())),
        }
    }

    pub(super) fn without_archive(blocked_turn: bool) -> Self {
        let mut outcome = Self::new(blocked_turn);
        if blocked_turn {
            outcome.blocked_resume = Some(ResumeAvailability::Unavailable(NO_ARCHIVE_RESUME_EXPLANATION.into()));
        }
        outcome
    }

    pub(super) fn history_checkpoint_succeeded(&self) -> bool {
        self.history_checkpoint_succeeded
    }

    pub(super) fn history_persistence_disabled(&self) -> bool {
        self.history_persistence_disabled
    }

    pub(super) fn blocked_handoff_resume(&self) -> BlockedHandoffResume<'_> {
        self.blocked_resume.as_ref().map_or(
            BlockedHandoffResume::Unavailable(UNVERIFIED_RESUME_EXPLANATION),
            ResumeAvailability::as_handoff_resume,
        )
    }
}

pub(super) async fn persist_session_checkpoint(
    archive: &SessionArchive,
    args: SessionProgressArgs,
    blocked_turn: bool,
) -> SessionCheckpointOutcome {
    let mut outcome = SessionCheckpointOutcome::new(blocked_turn);
    let checkpoint_status = if blocked_turn {
        archive.persist_progress_async_with_status_forced(args).await
    } else {
        archive.persist_progress_async_with_status(args).await
    };

    match checkpoint_status {
        Ok(SessionProgressPersistenceStatus::Persisted(path)) => {
            outcome.history_checkpoint_succeeded = true;
            if blocked_turn {
                outcome.blocked_resume = Some(match archive.verify_persisted_resume_identifier(&path).await {
                    Ok(Some(identifier)) => ResumeAvailability::Available(identifier),
                    Ok(None) => ResumeAvailability::Unavailable(UNVERIFIED_RESUME_EXPLANATION.into()),
                    Err(err) => {
                        tracing::warn!(error = %err, "Failed to verify persisted session archive for blocked handoff");
                        ResumeAvailability::Unavailable(format!(
                            "Resume is unavailable because the persisted session archive could not be resolved: {err}"
                        ))
                    }
                });
            }
        }
        Ok(SessionProgressPersistenceStatus::Throttled(path)) => {
            tracing::debug!(
                path = %path.display(),
                "Session progress checkpoint throttled; retaining in-flight steering intents"
            );
            if blocked_turn {
                outcome.blocked_resume = Some(ResumeAvailability::Unavailable(
                    "Resume is unavailable because the blocked-turn checkpoint was throttled.".to_owned(),
                ));
            }
        }
        Ok(SessionProgressPersistenceStatus::Disabled(path)) => {
            outcome.history_persistence_disabled = true;
            tracing::debug!(
                path = %path.display(),
                "Session progress checkpoint skipped because history persistence is disabled"
            );
            if blocked_turn {
                outcome.blocked_resume = Some(ResumeAvailability::Unavailable(
                    "Resume is unavailable because history persistence is disabled.".to_owned(),
                ));
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "Failed to persist session progress");
            if blocked_turn {
                outcome.blocked_resume = Some(ResumeAvailability::Unavailable(format!(
                    "Resume is unavailable because the blocked-turn checkpoint failed: {err}"
                )));
            }
        }
    }

    outcome
}

pub(super) fn write_blocked_handoff_after_checkpoint(
    workspace: &Path,
    session_id: &str,
    blocker_summary: &str,
    resume: BlockedHandoffResume<'_>,
    renderer: &mut AnsiRenderer,
    harness_emitter: Option<&HarnessEventEmitter>,
    handle: Option<&vtcode_ui::tui::app::InlineHandle>,
    planning_active: bool,
) {
    match write_blocked_handoff_with_resume(
        workspace,
        session_id,
        "blocked",
        blocker_summary,
        &existing_harness_artifact_paths(workspace),
        resume,
        planning_active,
    ) {
        Ok(artifacts) => {
            let full_reason_path = artifacts.current_path.display().to_string();
            let transcript_reason = truncated_block_reason(blocker_summary, &full_reason_path);
            let _ = renderer.line(MessageStyle::Warning, &format!("Turn blocked: {transcript_reason}"));
            // Verification blocks already name the exact verifier in the summary and
            // attempted harness auto-verification: lead with the actionable
            // verifier-first step instead of the generic `continue` nudge so
            // long-running work can resume without re-reading handoff files.
            // Lowercase match follows the helper convention for compound reasons.
            let is_verification_block = blocker_summary.to_ascii_lowercase().contains("verification is still pending");
            if is_verification_block {
                let _ = renderer.line(MessageStyle::Info, "What to do now (verification gate still pending):");
                let _ = renderer.line(
                    MessageStyle::Info,
                    "  • Run the verifier standalone (no pipes; cap with `max_output_tokens`), let it exit 0, then type 'continue' — harness auto-verification already tried.",
                );
            } else {
                let _ = renderer.line(MessageStyle::Info, "What you can do:");
                let _ = renderer.line(
                    MessageStyle::Info,
                    "  • In this session: Type 'continue' to resume, or describe alternative instructions",
                );
            }
            // Plan-mode QoL: a blocked turn while planning is active is a
            // read-only policy stop. `continue` keeps planning, but the user
            // may prefer to implement. Never auto-switch modes here — mode
            // switches are locked during a turn and require explicit user
            // choice on the next turn — so this is transcript guidance plus
            // a plan-aware input placeholder set by the caller, the
            // non-blocking equivalent of a HITL mode-switch popup.
            if planning_active {
                for line in plan_mode_switch_guidance_lines(is_plan_mode_mutation_block(blocker_summary)) {
                    let _ = renderer.line(MessageStyle::Info, line);
                }
            }
            match resume {
                BlockedHandoffResume::Available(id) => {
                    let _ = renderer
                        .line(MessageStyle::Info, &format!("  • From terminal: Run `vtcode --resume {}`", id.as_str()));
                }
                BlockedHandoffResume::Unavailable(_) => {}
            }
            let _ = renderer
                .line(MessageStyle::Info, &format!("  • Blocker details: {}", artifacts.current_path.display()));
            // The live pointer is cleared once the session recovers, which
            // orphans transcripts that only name it. The timestamped archive
            // survives clearing, so always print it alongside.
            let _ = renderer
                .line(MessageStyle::Info, &format!("  • Archived details: {}", artifacts.archive_path.display()));

            if let Some(handle) = handle {
                handle.set_activity_state(vtcode_commons::ui_protocol::ActivityState::Blocked);
            }

            if let Some(emitter) = harness_emitter {
                let _ = emitter.emit(harness_event(
                    HarnessEventKind::TurnBlocked,
                    Some(blocker_summary.to_string()),
                    None,
                    None,
                    None,
                ));
                for path in [&artifacts.current_path, &artifacts.archive_path] {
                    let path_text = path.display().to_string();
                    let _ = emitter.emit(harness_event(
                        HarnessEventKind::BlockedHandoffWritten,
                        Some("Blocked handoff written".to_string()),
                        Some(path_text),
                        None,
                        None,
                    ));
                }
            }
        }
        Err(err) => tracing::warn!(error = %err, "Failed to persist blocked handoff"),
    }
}

/// Persist blocked-handoff artifacts without renderer nudge lines or
/// blocked UI state. Used when tracker/plan auto-continue already printed
/// the single exhausted-path info line and the generic "Type continue"
/// stack must not be duplicated.
pub(super) fn persist_blocked_handoff_quiet(
    workspace: &Path,
    session_id: &str,
    blocker_summary: &str,
    planning_active: bool,
) {
    match write_blocked_handoff_with_resume(
        workspace,
        session_id,
        "blocked",
        blocker_summary,
        &existing_harness_artifact_paths(workspace),
        BlockedHandoffResume::Unavailable(NO_ARCHIVE_RESUME_EXPLANATION),
        planning_active,
    ) {
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "Failed to persist quiet blocked handoff"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NO_ARCHIVE_RESUME_EXPLANATION, TRANSCRIPT_BLOCK_REASON_LIMIT, blocker_summary_with_diagnostics,
        is_plan_mode_mutation_block, plan_mode_switch_guidance_lines, truncated_block_reason,
    };
    use vtcode_core::core::agent::snapshots::SnapshotTurnDiagnostics;

    #[test]
    fn quiet_handoff_resume_explanation_is_non_empty() {
        assert!(!NO_ARCHIVE_RESUME_EXPLANATION.trim().is_empty());
        assert!(NO_ARCHIVE_RESUME_EXPLANATION.contains("Resume is unavailable"));
    }

    #[test]
    fn short_block_reason_is_rendered_verbatim() {
        assert_eq!(
            truncated_block_reason("provider 429 rate limited", ".vtcode/tasks/current_blocked.md"),
            "provider 429 rate limited"
        );
    }

    #[test]
    fn long_block_reason_is_truncated_with_full_reason_pointer() {
        let path = ".vtcode/tasks/current_blocked.md";
        let summary = "x".repeat(2000);
        let truncated = truncated_block_reason(&summary, path);
        assert!(
            truncated.chars().count() <= TRANSCRIPT_BLOCK_REASON_LIMIT,
            "transcript reason must stay bounded: {} chars",
            truncated.chars().count()
        );
        assert!(truncated.ends_with(&format!("… — full reason: {path}")));
        assert!(truncated.starts_with("xxx"), "truncation must keep the reason prefix");
        assert!(truncated.contains("xxx…"), "the ellipsis must mark the elided middle");
    }

    #[test]
    fn truncation_respects_multibyte_char_boundaries() {
        let summary = "é".repeat(1500);
        let truncated = truncated_block_reason(&summary, "h.md");
        assert!(truncated.chars().count() <= TRANSCRIPT_BLOCK_REASON_LIMIT);
        assert!(truncated.ends_with("full reason: h.md"));
        assert!(truncated.contains('é'), "multi-byte chars must survive intact");
    }

    #[test]
    fn blocker_summary_without_diagnostics_is_verbatim() {
        let summary = blocker_summary_with_diagnostics("stalled", None, &[]);
        assert_eq!(summary, "stalled");
    }

    #[test]
    fn blocker_summary_appends_bounded_diagnostics_footer() {
        let diagnostics = SnapshotTurnDiagnostics {
            elapsed_ms: 12_345,
            requested_tool_calls: 32,
            admitted_tool_calls: 28,
            failed_tool_calls: 3,
            denied_tool_calls: 1,
            preflight_failures: 2,
            model_visible_tool_preview_budget_exhausted: true,
            suppressed_tool_previews: 5,
            ..Default::default()
        };
        let summary = blocker_summary_with_diagnostics(
            "repeated unverified responses",
            Some(&diagnostics),
            &["exec_command".to_string(), "code_search".to_string()],
        );
        assert!(summary.starts_with("repeated unverified responses\n\n# Last-Turn Diagnostics"));
        assert!(summary.contains("requested=32 admitted=28 failed=3 denied=1 preflight_failures=2"));
        assert!(summary.contains("Preview budget exhausted: true"));
        assert!(summary.contains("exec_command"));
    }

    #[test]
    fn mutation_block_detection_is_case_insensitive_and_asymmetric() {
        assert!(is_plan_mode_mutation_block(
            "Mutation blocked until verification: 2 mutating command(s) await a verifier."
        ));
        assert!(is_plan_mode_mutation_block("Tool 'apply_patch' execution failed: tool denied by planning workflow"));
        assert!(!is_plan_mode_mutation_block(
            "Turn blocked after repeated assistant responses reached the safety cap; the latest response was preserved."
        ));
        assert!(!is_plan_mode_mutation_block("provider 429 rate limited"));
        // Regression guard: verifier output mentioning a read-only filesystem
        // must not select the mutation header — the generic plan-mode header
        // is correct there.
        assert!(!is_plan_mode_mutation_block(
            "Turn blocked waiting for verification; auto-recovery turn scheduled. Last output tail:\nerror: read-only file system"
        ));
    }

    #[test]
    fn plan_mode_guidance_splits_mutation_from_generic_turn_block() {
        let mutation = plan_mode_switch_guidance_lines(true);
        let generic = plan_mode_switch_guidance_lines(false);
        assert!(mutation[0].contains("read-only"));
        assert!(mutation[1].contains("continue"));
        assert!(mutation[2].contains("/mode build"));
        assert!(mutation[2].contains("/mode auto"));
        assert!(generic[0].contains("read-only"));
        assert!(generic[2].contains("/mode build"));
        assert_ne!(mutation[0], generic[0], "mutation vs generic headers must differ");
    }

    #[test]
    fn plan_mode_guidance_names_build_as_default_with_auto_as_unattended() {
        let mutation = plan_mode_switch_guidance_lines(true);
        assert!(
            mutation[2].contains("Build stays confirmation-aware"),
            "must explain build vs auto policy difference"
        );
    }
}
