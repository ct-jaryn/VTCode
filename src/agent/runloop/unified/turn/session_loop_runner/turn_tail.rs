//! Shared turn-persistence tail: metrics, session checkpoint, and memory
//! envelope refresh.
//!
//! The interactive turn loop ends every real turn with this tail. Approved-plan
//! selection failures used to `continue` and skip it, dropping the just-finished
//! turn's checkpoint. Every path that abandons an iteration after history may
//! have changed must call `complete_turn_persistence_tail` first.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::RwLock;
use vtcode_core::config::loader::VTCodeConfig;
use vtcode_core::core::agent::runtime::AgentRuntime;
use vtcode_core::core::agent::snapshots::SnapshotTurnDiagnostics;
use vtcode_core::skills::Skill;
use vtcode_core::utils::session_archive::{SessionArchive, SessionMessage, SessionProgressArgs};

use super::super::RECENT_MESSAGE_LIMIT;
use super::blocked_handoff::{SessionCheckpointOutcome, persist_session_checkpoint};
use super::metrics::{TurnExecutionMetrics, emit_turn_execution_metrics};
use crate::agent::runloop::unified::state::SessionStats;
use crate::agent::runloop::unified::turn::compaction::refresh_session_memory_envelope_async;

/// Inputs for one pass of the turn-persistence tail.
pub(super) struct TurnPersistenceTail<'a> {
    pub outcome: &'static str,
    pub history_snapshot_bytes: usize,
    pub timeout_secs: u64,
    pub elapsed_ms: u128,
    pub blocked_turn: bool,
    /// Diagnostics for the turn that just finished. `None` when the iteration
    /// never reached request execution (selection failure at startup).
    pub turn_diagnostics: Option<SnapshotTurnDiagnostics>,
    pub runtime: &'a mut AgentRuntime,
    pub session_archive: &'a mut Option<SessionArchive>,
    pub next_checkpoint_turn: usize,
    pub session_stats: &'a SessionStats,
    pub loaded_skills: &'a Arc<RwLock<hashbrown::HashMap<String, Skill>>>,
    pub workspace: &'a Path,
    pub session_id: &'a str,
    pub vt_cfg: Option<&'a VTCodeConfig>,
}

/// Emit turn metrics, persist the session checkpoint, and refresh the memory
/// envelope for whatever history is already in `runtime`.
pub(super) async fn complete_turn_persistence_tail(tail: TurnPersistenceTail<'_>) -> SessionCheckpointOutcome {
    emit_turn_execution_metrics(TurnExecutionMetrics {
        attempts_made: 1,
        retry_count: 0,
        history_snapshot_bytes: tail.history_snapshot_bytes,
        timeout_secs: tail.timeout_secs,
        elapsed_ms: tail.elapsed_ms,
        outcome: tail.outcome,
    });

    let mut checkpoint_outcome = SessionCheckpointOutcome::without_archive(tail.blocked_turn);
    if let Some(archive) = tail.session_archive.as_ref() {
        let messages: Vec<SessionMessage> = tail.runtime.state.messages.iter().map(SessionMessage::from).collect();
        let mut recent_messages: Vec<SessionMessage> = tail
            .runtime
            .state
            .messages
            .iter()
            .rev()
            .take(RECENT_MESSAGE_LIMIT)
            .map(SessionMessage::from)
            .collect();
        recent_messages.reverse();

        let progress_turn = tail.next_checkpoint_turn.saturating_sub(1).max(1);
        let distinct_tools = tail.session_stats.sorted_tools();
        let skill_names: Vec<String> = tail.loaded_skills.read().await.keys().cloned().collect();
        let checkpoint_args = SessionProgressArgs {
            total_messages: tail.runtime.state.messages.len(),
            distinct_tools,
            messages,
            recent_messages,
            turn_number: progress_turn,
            token_usage: None,
            max_context_tokens: None,
            loaded_skills: Some(skill_names),
            turn_diagnostics: tail.turn_diagnostics,
        };
        checkpoint_outcome = persist_session_checkpoint(archive, checkpoint_args, tail.blocked_turn).await;
    }

    let steering_update = {
        let (_, steering) = tail.runtime.split_mut();
        if checkpoint_outcome.history_checkpoint_succeeded() {
            steering.acknowledge_durable_follow_up_intents();
        } else if tail.session_archive.is_none() || checkpoint_outcome.history_persistence_disabled() {
            steering.release_in_flight_follow_up_intents_without_persistence();
        }
        vtcode_core::compaction::memory_envelope::SessionMemoryEnvelopeUpdate {
            pending_intents: Some(steering.pending_follow_up_intents_snapshot()),
            applied_intent_ids: steering.applied_follow_up_intent_ids().iter().cloned().collect(),
            ..Default::default()
        }
    };
    if let Err(err) = refresh_session_memory_envelope_async(
        tail.workspace,
        tail.session_id,
        tail.vt_cfg,
        &tail.runtime.state.messages,
        tail.session_stats,
        Some(&steering_update),
    )
    .await
    {
        tracing::warn!(
            error = %err,
            session_id = %tail.session_id,
            "Failed to refresh session memory envelope after turn"
        );
    }

    checkpoint_outcome
}
