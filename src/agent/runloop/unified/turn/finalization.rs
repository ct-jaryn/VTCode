#![allow(
    clippy::too_many_arguments,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]
use anyhow::Result;
use std::io;
use std::path::PathBuf;
use vtcode_core::core::agent::snapshots::SnapshotTurnDiagnostics;
use vtcode_core::hooks::{LifecycleHookEngine, SessionEndReason};
use vtcode_core::llm::provider as uni;
use vtcode_core::notifications::{set_global_notification_hook_engine, set_global_terminal_focused};
use vtcode_core::ui::set_tui_mode;
use vtcode_core::utils::ansi::{AnsiRenderer, MessageStyle};
use vtcode_core::utils::session_archive::{SessionArchive, SessionMessage};
use vtcode_core::utils::transcript;
use vtcode_ui::tui::app::{InlineHandle, InlineSession};

use crate::agent::runloop::unified::async_mcp_manager::AsyncMcpManager;
use crate::agent::runloop::unified::state::SessionStats;
use crate::agent::runloop::unified::workspace_links::{LinkedDirectory, remove_directory_symlink};

use super::utils::render_hook_messages;

pub(super) struct FinalizationOutput {
    pub archive_path: Option<PathBuf>,
}

/// Budget for the final session-archive write. The write is atomic
/// (temp + rename) and progress snapshots are persisted during the session,
/// so timing out only skips the final upgrade — it never corrupts the archive.
const ARCHIVE_FINALIZE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Maintenance budget for one session-end task: interrupt exits (Ctrl+C /
/// /exit) get the tight teardown cap instead of `normal`.
fn interrupt_budget(interrupt_exit: bool, normal: std::time::Duration) -> std::time::Duration {
    if interrupt_exit {
        std::time::Duration::from_millis(500)
    } else {
        normal
    }
}

/// Restore terminal to a clean state after session exit
/// This ensures that raw mode is disabled and the terminal is left in a usable state
/// even if the TUI didn't exit cleanly (e.g., due to Ctrl+C)
///
/// Raw mode is deliberately kept on here: the graceful exit path still has
/// teardown work left, and a cooked tty would echo any late input (the kitty
/// key-release report for the exiting Ctrl+C) onto the screen. The exit
/// postamble finishes the transition as its final step.
fn restore_terminal_on_exit() -> io::Result<()> {
    // Use the centralized TUI restoration logic from vtcode-core
    // This handles draining events, clearing the line, and proper restoration order
    vtcode_ui::tui::panic_hook::restore_tui_keep_raw_mode()
}

pub(super) async fn finalize_session(
    renderer: &mut AnsiRenderer,
    lifecycle_hooks: Option<&LifecycleHookEngine>,
    turn_id: &str,
    session_end_reason: SessionEndReason,
    session_archive: &mut Option<SessionArchive>,
    session_stats: &SessionStats,
    last_turn_diagnostics: Option<SnapshotTurnDiagnostics>,
    conversation_history: &[uni::Message],
    linked_directories: Vec<LinkedDirectory>,
    async_mcp_manager: Option<&AsyncMcpManager>,
    handle: &InlineHandle,
    session: &mut InlineSession,
) -> Result<FinalizationOutput> {
    let transcript_lines = transcript::snapshot();
    let mut archive_path: Option<PathBuf> = None;

    if let Some(archive) = session_archive.take() {
        let distinct_tools = session_stats.sorted_tools();
        let total_messages = conversation_history.len();
        let session_messages: Vec<SessionMessage> = conversation_history.iter().map(SessionMessage::from).collect();

        // The final write is atomic (temp + rename), so a timeout here can only
        // skip the final upgrade past the last progress snapshot — never corrupt
        // the archive. Blocking the shell return on a slow disk write is worse.
        let finalize_task = tokio::task::spawn_blocking(move || {
            archive.finalize_with_diagnostics(
                transcript_lines,
                total_messages,
                distinct_tools,
                session_messages,
                last_turn_diagnostics,
            )
        });
        match tokio::time::timeout(ARCHIVE_FINALIZE_TIMEOUT, finalize_task).await {
            Ok(Ok(Ok(path))) => {
                archive_path = Some(path.clone());
                if let Some(hooks) = lifecycle_hooks {
                    hooks.update_transcript_path(Some(path.clone())).await;
                }
                renderer.line(MessageStyle::Info, &format!("Session saved to {}", path.display()))?;
                renderer.line_if_not_empty(MessageStyle::Output)?;
            }
            Ok(Ok(Err(err))) => {
                renderer.line(MessageStyle::Error, &format!("Failed to save session: {err}"))?;
                renderer.line_if_not_empty(MessageStyle::Output)?;
            }
            Ok(Err(join_error)) => {
                renderer.line(MessageStyle::Error, &format!("Failed to save session: {join_error}"))?;
                renderer.line_if_not_empty(MessageStyle::Output)?;
            }
            Err(_elapsed) => {
                tracing::warn!(
                    "session archive finalize timed out after {:?}; keeping the last progress snapshot",
                    ARCHIVE_FINALIZE_TIMEOUT
                );
            }
        }
    }

    for linked in linked_directories {
        if let Err(err) = remove_directory_symlink(&linked.link_path).await {
            tracing::warn!("Failed to remove linked directory {}: {}", linked.link_path.display(), err);
        }
    }

    // A user-requested exit (Ctrl+C / /exit / Ctrl+D) must not park the shell
    // on maintenance work: session-end hooks and MCP shutdown are best-effort
    // here (the OS reaps MCP children at process exit), so bound them tightly
    // instead of the 3s/2s they get on a normal session end. They are
    // independent of each other, so their budgets overlap via `join!` instead
    // of adding up sequentially.
    let interrupt_exit = matches!(session_end_reason, SessionEndReason::Exit | SessionEndReason::Cancelled);

    let hooks_future = async {
        let Some(hooks) = lifecycle_hooks else {
            return (Vec::new(), None);
        };
        let hook_budget = interrupt_budget(interrupt_exit, std::time::Duration::from_secs(3));
        match tokio::time::timeout(hook_budget, hooks.run_session_end(turn_id, session_end_reason)).await {
            Ok(Ok(messages)) => (messages, None),
            Ok(Err(err)) => (Vec::new(), Some(err)),
            Err(_elapsed) => {
                tracing::warn!("Session end hooks timed out, skipping");
                (Vec::new(), None)
            }
        }
    };
    let mcp_future = async {
        let Some(mcp_manager) = async_mcp_manager else {
            return;
        };
        let mcp_budget = interrupt_budget(interrupt_exit, std::time::Duration::from_secs(2));
        match tokio::time::timeout(mcp_budget, mcp_manager.shutdown()).await {
            Ok(Err(e)) => {
                let error_msg = e.to_string();
                if error_msg.contains("EPIPE") || error_msg.contains("Broken pipe") || error_msg.contains("write EPIPE")
                {
                    tracing::debug!("MCP client shutdown encountered pipe errors (normal): {}", e);
                } else {
                    tracing::warn!("Failed to shutdown MCP client cleanly: {}", e);
                }
            }
            Err(_elapsed) => {
                tracing::warn!("MCP client shutdown timed out during finalization");
            }
            Ok(Ok(())) => {}
        }
    };
    let ((hook_messages, hook_error), ()) = tokio::join!(hooks_future, mcp_future);

    handle.shutdown();
    set_global_notification_hook_engine(None);
    set_global_terminal_focused(false);

    // The TUI task owns the terminal: wait for it to run its own canonical
    // teardown (final render on the alternate screen, leave, drain, disable
    // raw mode) before touching the terminal from the host side. Restoring
    // earlier flips the screen back while the TUI is still drawing, which
    // paints transcript frames onto the main CLI screen. The TUI was told to
    // shut down at the start of the session tail, so this join usually
    // returns immediately.
    if !session.wait_for_exit(std::time::Duration::from_millis(2000)).await {
        tracing::warn!("TUI task did not exit after shutdown; forcing terminal restore");
    }

    // Backstop restore in case the TUI task hung or never ran. This is a
    // no-op when the TUI already restored itself (restore_tui is idempotent).
    let _ = restore_terminal_on_exit();
    // Claim the cooked-mode transition here rather than waiting for the exit
    // postamble: the TUI is gone and input has been drained, so any remaining
    // teardown (health rendering, subagent cleanup) must not keep the tty in
    // raw mode looking frozen. The postamble's own call becomes a drain-only
    // no-op (claim-once).
    vtcode_ui::tui::panic_hook::finish_deferred_raw_mode_restore();

    transcript::clear_inline_handle();

    set_tui_mode(false);

    if let Some(err) = hook_error {
        renderer.line(MessageStyle::Error, &format!("Failed to run session end hooks: {err}"))?;
    } else {
        render_hook_messages(renderer, &hook_messages)?;
    }

    // Phase 4 Telemetry: Report Resilience Metrics
    let open_circuits = session_stats.circuit_breaker.get_open_circuits();
    if !open_circuits.is_empty() {
        renderer.line(MessageStyle::Warning, &format!("Open Circuit Breakers ({}):", open_circuits.len()))?;
        for tool in &open_circuits {
            renderer.line(MessageStyle::Warning, &format!("  - {tool}"))?;
        }
        renderer.line_if_not_empty(MessageStyle::Output)?;
    }

    let all_stats = session_stats.tool_health_tracker.get_all_tool_stats();
    let mut unhealthy_tools: Vec<_> = all_stats
        .iter()
        .filter(|(name, _)| !session_stats.tool_health_tracker.is_healthy(name))
        .collect();

    // Sort for stable output
    unhealthy_tools.sort_by(|a, b| a.0.cmp(&b.0));

    if !unhealthy_tools.is_empty() {
        renderer.line(MessageStyle::Warning, &format!("Unhealthy Tools ({}):", unhealthy_tools.len()))?;
        for (name, _) in unhealthy_tools {
            let (_, reason) = session_stats.tool_health_tracker.check_health(name);
            if let Some(r) = reason {
                renderer.line(MessageStyle::Warning, &format!("  - {name}: {r}"))?;
            }
        }
        renderer.line_if_not_empty(MessageStyle::Output)?;
    }

    Ok(FinalizationOutput { archive_path })
}
