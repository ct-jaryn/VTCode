//! Session bootstrap and exit helpers: primary-agent persistence, plan-selection
//! failure tails, thread-completion resolution, and archived-prompt loading.

use super::*;

/// Stable opening shared with `is_internal_harness_follow_up`, which keys the
/// quiet path off this constant instead of a duplicated literal.
pub(crate) const BACKGROUND_COMPLETION_CONTINUATION_PROMPT_PREFIX: &str =
    "Review the authoritative background subprocess completion notice";

/// Budget for best-effort background teardown on the session-exit path.
///
/// Every step here runs after the terminal has been restored, so an unbounded
/// wait directly delays the shell prompt. Both the exec/PTY backstop and the
/// subagent controller shutdown share this bound; the OS reaps any remainder
/// at process exit, so skipping the wait is always preferable to parking.
pub(super) const EXIT_BACKGROUND_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn background_completion_continuation_prompt() -> String {
    format!(
        "{BACKGROUND_COMPLETION_CONTINUATION_PROMPT_PREFIX} and continue the user's request. \
         Do not poll or wait for those completed tasks."
    )
}

pub(super) fn persist_primary_agent(
    session_archive: &mut Option<session_archive::SessionArchive>,
    active_primary_agent: &vtcode_core::primary_agent::ActivePrimaryAgentState,
) {
    if let Some(archive) = session_archive.as_mut() {
        archive.set_primary_agent(active_primary_agent.active().name());
    }
}

/// Persist the turn tail when approved-plan agent selection fails.
///
/// Selection failure must stay recoverable (no hard session abort). The
/// iteration may already have produced history (plan-approval handoff after a
/// finished turn), so metrics/checkpoint run before `continue` instead of being
/// skipped until the next successful turn.
#[allow(
    clippy::too_many_arguments,
    reason = "turn-tail context is a flat bag of loop locals"
)]
pub(super) async fn record_plan_selection_failure_tail(
    runtime: &mut AgentRuntime,
    session_archive: &mut Option<session_archive::SessionArchive>,
    session_stats: &SessionStats,
    loaded_skills: &std::sync::Arc<tokio::sync::RwLock<hashbrown::HashMap<String, vtcode_core::skills::types::Skill>>>,
    next_checkpoint_turn: usize,
    workspace: &std::path::Path,
    session_id: &str,
    vt_cfg: Option<&VTCodeConfig>,
    timeout_secs: u64,
) {
    complete_turn_persistence_tail(TurnPersistenceTail {
        outcome: "aborted",
        history_snapshot_bytes: 0,
        timeout_secs,
        elapsed_ms: 0,
        blocked_turn: false,
        turn_diagnostics: None,
        runtime,
        session_archive,
        next_checkpoint_turn,
        session_stats,
        loaded_skills,
        workspace,
        session_id,
        vt_cfg,
    })
    .await;
}

/// Startup planning entry: select the plan primary agent and refresh the
/// header so prompt/tools match Plan mode (not just the ActivityState).
pub(super) async fn apply_startup_plan_agent_selection(
    active_primary_agent: &mut vtcode_core::primary_agent::ActivePrimaryAgentState,
    tool_registry: &vtcode_core::tools::registry::ToolRegistry,
    config: &vtcode_core::config::types::AgentConfig,
    handle: &vtcode_ui::tui::app::InlineHandle,
) {
    use crate::agent::runloop::unified::planning_workflow_state::{PLAN_PRIMARY_AGENT_NAME, apply_plan_agent_header};
    use crate::agent::runloop::unified::turn::primary_agent_runtime::{
        builtin_primary_agent_specs, load_primary_agent_specs,
    };

    if active_primary_agent
        .active()
        .identity
        .name
        .eq_ignore_ascii_case(PLAN_PRIMARY_AGENT_NAME)
    {
        apply_plan_agent_header(handle);
        return;
    }
    let specs = match load_primary_agent_specs(tool_registry, &config.workspace).await {
        Ok(specs) if !specs.is_empty() => specs,
        _ => builtin_primary_agent_specs(),
    };
    match active_primary_agent.select_from_specs(&specs, PLAN_PRIMARY_AGENT_NAME) {
        Ok(active) => {
            let display = active.display_name.clone();
            let color = active.color.clone().filter(|c| !c.trim().is_empty());
            handle.set_primary_agent(Some(display), color);
            tracing::info!(
                target: "vtcode.planning_workflow",
                switch_path = "startup_plan_entry",
                "Selected plan primary agent at startup planning entry"
            );
        }
        Err(err) => {
            tracing::warn!(
                target: "vtcode.planning_workflow",
                switch_path = "startup_plan_entry",
                error = %err,
                "Startup planning entry could not select plan primary agent; header will still show Plan"
            );
            apply_plan_agent_header(handle);
        }
    }
}

pub(crate) fn resolve_thread_completion_status(
    session_end_reason: &SessionEndReason,
    budget_limit_reached: bool,
    last_approved_plan_summary_status: Option<ExecutionSummaryStatus>,
    last_turn_result: Option<&RunLoopTurnLoopResult>,
    last_turn_response_was_fallback: bool,
) -> (&'static str, ThreadCompletionSubtype) {
    if budget_limit_reached {
        return session_end_reason.thread_completion_status(true);
    }

    if matches!(session_end_reason, SessionEndReason::Completed | SessionEndReason::NewSession) {
        return match (last_approved_plan_summary_status, last_turn_result) {
            (Some(ExecutionSummaryStatus::Blocked), _)
            | (_, Some(RunLoopTurnLoopResult::Blocked { .. }))
            | (_, Some(RunLoopTurnLoopResult::Aborted)) => ("blocked", ThreadCompletionSubtype::ErrorDuringExecution),
            (Some(ExecutionSummaryStatus::Failed), _) => ("failed", ThreadCompletionSubtype::ErrorDuringExecution),
            (_, Some(RunLoopTurnLoopResult::Completed { .. })) | (_, Some(RunLoopTurnLoopResult::Cancelled))
                if last_turn_response_was_fallback =>
            {
                ("failed", ThreadCompletionSubtype::ErrorDuringExecution)
            }
            (_, Some(RunLoopTurnLoopResult::Cancelled)) => ("cancelled", ThreadCompletionSubtype::Cancelled),
            (_, Some(RunLoopTurnLoopResult::Exit)) => ("exit", ThreadCompletionSubtype::Cancelled),
            _ => session_end_reason.thread_completion_status(budget_limit_reached),
        };
    }

    if matches!(session_end_reason, SessionEndReason::Exit)
        && !last_turn_response_was_fallback
        && !matches!(
            last_approved_plan_summary_status,
            Some(ExecutionSummaryStatus::Blocked | ExecutionSummaryStatus::Failed)
        )
        && matches!(last_turn_result, Some(RunLoopTurnLoopResult::Completed { .. }))
    {
        return ("exit", ThreadCompletionSubtype::Success);
    }

    session_end_reason.thread_completion_status(budget_limit_reached)
}

/// Load user prompts from recent session archives (last 24 hours) and inject
/// them into the history picker so Ctrl+R can search across sessions.
pub(super) async fn load_archived_prompts_for_history(handle: &vtcode_ui::tui::app::InlineHandle) {
    let listings = match session_archive::list_recent_sessions(50).await {
        Ok(listings) => listings,
        Err(_) => return,
    };

    let mut entries = Vec::new();
    for listing in &listings {
        let session_label = listing.identifier();
        for msg in &listing.snapshot.messages {
            if msg.role != MessageRole::User {
                continue;
            }
            let content = msg.content.as_text();
            let trimmed = content.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Use first line as the prompt preview
            let preview = trimmed.lines().next().unwrap_or(trimmed).chars().take(200).collect::<String>();
            // NOTE: `created_at` uses the session start time because per-message
            // timestamps are not stored in the archive format. The time label is
            // therefore an approximation of when the conversation happened.
            entries.push(ArchivedPromptEntry {
                content: preview,
                created_at: listing.snapshot.started_at,
                session_label: session_label.clone(),
            });
        }
    }

    entries.truncate(20);

    if !entries.is_empty() {
        handle.set_archived_history(entries);
    }
}
