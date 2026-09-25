use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hashbrown::{HashMap, HashSet};
use tokio::sync::RwLock;
use vtcode_config::core::permissions::AgentPermissionsConfig;
use vtcode_core::acp::ToolPermissionCache;
use vtcode_core::config::PermissionsConfig;
use vtcode_core::config::loader::VTCodeConfig;
use vtcode_core::config::types::AgentConfig as CoreAgentConfig;
use vtcode_core::core::decision_tracker::DecisionTracker;
use vtcode_core::core::trajectory::TrajectoryLogger;
use vtcode_core::llm::provider as uni;
use vtcode_core::tools::{ApprovalRecorder, ToolRegistry, ToolResultCache};
use vtcode_core::utils::ansi::AnsiRenderer;
use vtcode_ui::tui::app::{InlineHandle, InlineSession};

use crate::agent::runloop::mcp_events::McpPanelState;
use crate::agent::runloop::unified::inline_events::harness::HarnessEventEmitter;
use crate::agent::runloop::unified::planning_workflow_state::PlanningWorkflowSessionState;
use crate::agent::runloop::unified::state::SessionStats;
use crate::agent::runloop::unified::tool_call_safety::ToolCallSafetyValidator;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnRunId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnPhase {
    Preparing,
    Requesting,
    ExecutingTools,
    Finalizing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnExecutionPhase {
    Preparing,
    Requesting,
    ExecutingTools,
    Finalizing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryPhase {
    Inactive,
    Pending,
    InPass,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryMode {
    ToolEnabledRetry,
    ToolFreeSynthesis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolBudgetWarning {
    pub used: usize,
    pub max: usize,
    pub remaining: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolBudgetExhaustion {
    pub used: usize,
    pub max: usize,
    pub remaining: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolBudgetExhaustionNotice {
    pub exhaustion: ToolBudgetExhaustion,
    pub first_notice: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolWallClockExhaustion {
    pub max_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolWallClockExhaustionNotice {
    pub exhaustion: ToolWallClockExhaustion,
    pub first_notice: bool,
}

pub(crate) const TOOL_BUDGET_WARNING_THRESHOLD: f64 = 0.75;

/// Maximum aggregate tool-result preview bytes copied into the provider-facing
/// history for one turn. Complete output remains in the internal spool and
/// current-session tool-output viewer; this only bounds the diagnostic surface
/// seen by a model during a recovery-heavy turn.
/// Preview credit granted per admitted spool-page read. Paged spool reads are
/// already size-bounded per result and capped sequentially per turn by the
/// spool-chunk guard; without credit the aggregate budget blinds mid-file
/// paging (a single large file can exhaust it), defeating the designed
/// spool-then-page workflow in every mode.
pub(crate) const SPOOL_PAGE_PREVIEW_CREDIT_BYTES: usize = 16 * 1024;
/// Cap on banked spool-page credit per turn so paging cannot grow the prompt
/// without bound. Six sequential spool pages trip the spool-chunk guard into
/// recovery, so this covers a full paging run with headroom for its recovery
/// payload.
const MAX_SPOOL_PAGE_PREVIEW_CREDIT_BYTES: usize = 96 * 1024;
/// Model-facing guidance emitted after a user increases the per-turn tool
/// budget. Keep this shared by the normal and out-of-band provider paths so a
/// grant has the same continuation semantics regardless of transport.
pub(crate) const SESSION_LIMIT_GRANT_DIRECTIVE: &str = "Session tool-call limit increased by the user. Continue this same turn with the current agent, retry the pending call, and reuse existing tool outputs instead of repeating exploration.";
/// Session tool-call increase applied without prompting while full-auto
/// auto-grant is active. Matches the overlay default so automatic and manual
/// grants converge on the same budget.
pub(crate) const SESSION_LIMIT_AUTO_GRANT_INCREMENT: usize = 100;

/// Whether tool-loop and session tool-call limit increases may be granted
/// without an interactive prompt. Requires all three: the turn runs under
/// full-auto policy, `[automation.full_auto]` is enabled, and the opt-out
/// `auto_grant_tool_limits` flag is still on. Live config reloads can disable
/// `enabled` mid-session while the runtime bool stays true, so both sides
/// of the gate are checked.
#[inline]
pub(crate) fn full_auto_loop_grants_enabled(full_auto: bool, vt_cfg: Option<&VTCodeConfig>) -> bool {
    full_auto
        && vt_cfg.is_some_and(|cfg| cfg.automation.full_auto.enabled && cfg.automation.full_auto.auto_grant_tool_limits)
}
const MODEL_VISIBLE_TOOL_METADATA_BUDGET_BYTES: usize = 16 * 1024;
const TOOL_PREVIEW_METADATA_MAX_DEPTH: usize = 8;

const TOOL_PREVIEW_METADATA_STRING_LIMIT: usize = 512;
/// Do not materialize an arbitrarily large suppressed payload merely to
/// recover optional metadata. Normal tool responses are compacted before this
/// point; oversized or malformed payloads keep only the generic byte count.
const TOOL_PREVIEW_METADATA_PARSE_LIMIT_BYTES: usize = 128 * 1024;

impl ToolBudgetWarning {
    pub(crate) fn system_message(self) -> String {
        format!(
            "Tool-call budget warning: {}/{} used; {} remaining for this turn. Use targeted extraction/batching before additional tool calls.",
            self.used, self.max, self.remaining
        )
    }

    pub(crate) fn log_threshold_reached(self, path: &'static str) {
        tracing::info!(used = self.used, max = self.max, remaining = self.remaining, "{path}");
    }
}

/// Shared tail of the tool-call and wall-clock budget exhaustion directives.
const BUDGET_EXHAUSTED_SYNTHESIS_NOTE: &str = "Tools are disabled for the rest of this turn, so further tool calls are \
skipped. Synthesize your final answer now from the tool outputs already gathered in this conversation.";

impl ToolBudgetExhaustion {
    pub(crate) fn policy_violation_message(self) -> String {
        format!("Policy violation: exceeded max tool calls per turn ({})", self.max)
    }

    /// Compact stub returned for the 2nd+ rejected calls in the same batch so
    /// the full policy message isn't repeated N times and context stays clean.
    pub(crate) fn skipped_call_message(self) -> String {
        "Tool-call budget exhausted for this turn; call skipped.".to_string()
    }

    /// System directive pushed once (after all tool responses in the batch)
    /// telling the model that tools are disabled for the rest of the turn and
    /// it must synthesize a final answer from already-gathered outputs.
    /// Mirrors `ToolWallClockExhaustion::synthesis_directive_message`.
    pub(crate) fn synthesis_directive_message(self) -> String {
        debug_assert!(self.max > 0, "disabled tool-call caps must not emit exhaustion");
        format!(
            "Tool-call budget exhausted for this turn ({}/{}). {BUDGET_EXHAUSTED_SYNTHESIS_NOTE}",
            self.used, self.max
        )
    }
}

/// Budget kinds that can fail a turn closed. Recorded in trajectory telemetry
/// so post-hoc diagnosis can tell which ceiling fired; the model-facing
/// recovery directives already cover the live behavior.
pub(crate) mod budget_kind {
    /// Per-turn tool-call count (`max_tool_calls_per_turn`).
    pub(crate) const TOOL_CALLS: &str = "tool_calls";
    /// Per-turn tool-loop iterations (`max_tool_loops` hard cap).
    pub(crate) const TOOL_LOOP: &str = "tool_loop";
    /// Session-wide tool-call count (safety gateway + auto-grant headroom).
    pub(crate) const SESSION_CALLS: &str = "session_calls";
    /// Per-turn aggregate tool wall-clock time (`max_tool_wall_clock_secs`).
    pub(crate) const WALL_CLOCK: &str = "wall_clock_secs";
}

/// Snapshot of a failed-closed turn budget for trajectory telemetry.
/// Mirrors the `tool_catalog_cache_metrics` record shape.
#[derive(Copy, Clone)]
pub(crate) struct BudgetExhaustedMetrics {
    pub budget: &'static str,
    pub used: usize,
    pub max: usize,
    pub step_count: Option<usize>,
    pub planning_active: bool,
    pub tool_calls: usize,
}

/// Log one JSONL `budget_exhausted` record. Best effort like all trajectory
/// logging: a disabled logger drops the record. Unknown `kind` values are
/// ignored by trajectory consumers, so this adds no schema burden.
pub(crate) fn emit_budget_exhausted_metric(traj: &TrajectoryLogger, metrics: BudgetExhaustedMetrics) {
    traj.log(&budget_exhausted_record(metrics));
}

/// Serializable `budget_exhausted` record behind
/// [`emit_budget_exhausted_metric`], kept separate so tests can assert exact
/// field values without filesystem I/O (the async line writer behind
/// `traj.log` is only flushable inside `vtcode-core` unit tests).
#[derive(serde::Serialize)]
pub(crate) struct BudgetExhaustedRecord {
    kind: &'static str,
    budget: &'static str,
    used: usize,
    max: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_count: Option<usize>,
    planning_active: bool,
    tool_calls: usize,
    ts: i64,
}

/// Pure record constructor behind [`emit_budget_exhausted_metric`].
pub(crate) fn budget_exhausted_record(metrics: BudgetExhaustedMetrics) -> BudgetExhaustedRecord {
    BudgetExhaustedRecord {
        kind: "budget_exhausted",
        budget: metrics.budget,
        used: metrics.used,
        max: metrics.max,
        step_count: metrics.step_count,
        planning_active: metrics.planning_active,
        tool_calls: metrics.tool_calls,
        ts: chrono::Utc::now().timestamp(),
    }
}

impl ToolWallClockExhaustion {
    pub(crate) fn policy_violation_message(self) -> String {
        format!("Policy violation: exceeded tool wall clock budget ({}s)", self.max_secs)
    }

    /// Compact stub returned for the 2nd+ rejected calls in the same batch so
    /// the full policy message isn't repeated N times and context stays clean.
    pub(crate) fn skipped_call_message(self) -> String {
        "Tool wall-clock budget exhausted for this turn; call skipped.".to_string()
    }

    /// System directive pushed once (after all tool responses in the batch)
    /// telling the model that tools are disabled for the rest of the turn and
    /// it must synthesize a final answer from already-gathered outputs. This is
    /// the in-turn synthesis nudge that the raw per-call policy errors lack.
    pub(crate) fn synthesis_directive_message(self) -> String {
        format!(
            "Tool wall-clock budget exhausted for this turn ({}s). {BUDGET_EXHAUSTED_SYNTHESIS_NOTE}",
            self.max_secs
        )
    }
}

impl From<TurnPhase> for TurnExecutionPhase {
    fn from(value: TurnPhase) -> Self {
        match value {
            TurnPhase::Preparing => Self::Preparing,
            TurnPhase::Requesting => Self::Requesting,
            TurnPhase::ExecutingTools => Self::ExecutingTools,
            TurnPhase::Finalizing => Self::Finalizing,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnExecutionSnapshot {
    pub run_id: String,
    pub turn_id: String,
    pub phase: TurnExecutionPhase,
    pub max_tool_calls: usize,
    pub max_tool_wall_clock_secs: u64,
    pub max_tool_retries: u32,
}

/// Tracks action patterns across turn boundaries to detect loops that span
/// multiple turns.  Constructed once per session and carried forward across
/// turns, unlike `HarnessTurnState` which is fresh each turn.
///
/// Each turn produces a "fingerprint" — a hash of the sorted set of tool
/// signatures used in that turn.  If the same fingerprint appears 2+ times
/// in the sliding window, a cross-turn loop is detected.
///
/// Also tracks consecutive turns with no execution progress to detect
/// "stuck" states where the agent only reads without making progress.
pub(crate) struct CrossTurnTracker {
    /// Rolling window of per-turn action fingerprints.
    turn_fingerprints: VecDeque<u64>,
    /// Maximum window size for cross-turn loop detection.
    window_size: usize,
    /// Consecutive turns with no workspace mutation or command execution.
    zero_mutation_turns: usize,
    /// Failure key of the previous turn's shell execution, if it failed.
    last_failed_shell_key: Option<String>,
    /// Consecutive turns repeating the same failed shell key.
    consecutive_same_failed_shell: usize,
}

/// Number of consecutive zero-mutation turns before a HARD STOP fires.
const STUCK_ZERO_MUTATION_THRESHOLD: usize = 3;

/// Consecutive turns repeating an identical shell failure before a warning
/// fires. Three turns (two repeats) confirm the pattern while tolerating a
/// single fix-then-reverify cycle.
const IDENTICAL_SHELL_FAILURE_TURNS_THRESHOLD: usize = 3;

impl CrossTurnTracker {
    pub(crate) fn new() -> Self {
        Self {
            turn_fingerprints: VecDeque::with_capacity(8),
            window_size: 8,
            zero_mutation_turns: 0,
            last_failed_shell_key: None,
            consecutive_same_failed_shell: 0,
        }
    }

    /// Seal the current turn: compute a fingerprint from the provided tool
    /// signatures, check for cross-turn loops and stuck states.
    ///
    /// - `read_only_signatures`: signatures of read-only tool calls this turn.
    /// - `written_files`: paths of files written this turn.
    /// - `shell_command`: last shell command signature, if any.
    /// - `failed_shell_key`: `signature::err::error-signature` of this turn's
    ///   failed shell execution, if any. Identical keys across consecutive
    ///   turns mean the same command fails with an unchanged error — retries
    ///   after a genuine fix change the error or succeed, so they never
    ///   extend the streak.
    /// - `planning_active`: whether the planning workflow is currently active.
    ///
    /// Returns a warning string if a loop or stuck pattern is detected.
    #[allow(
        dead_code,
        reason = "Compatibility wrapper retained for callers using the original tracker API."
    )]
    pub(crate) fn seal_turn(
        &mut self,
        read_only_signatures: &[String],
        written_files: &HashSet<String>,
        shell_command: Option<&str>,
        planning_active: bool,
    ) -> Option<String> {
        self.seal_turn_with_progress(read_only_signatures, written_files, shell_command, None, false, planning_active)
    }

    /// Seal a turn while accounting for productive provider-native tool work
    /// that is not represented by a normal tool-result message.
    pub(crate) fn seal_turn_with_progress(
        &mut self,
        read_only_signatures: &[String],
        written_files: &HashSet<String>,
        shell_command: Option<&str>,
        failed_shell_key: Option<&str>,
        out_of_band_tool_progress: bool,
        planning_active: bool,
    ) -> Option<String> {
        let mut signatures: Vec<String> = read_only_signatures.to_vec();
        for path in written_files {
            signatures.push(format!("write::{path}"));
        }
        if let Some(cmd) = shell_command {
            signatures.push(cmd.to_string());
        }

        let had_execution_progress = !written_files.is_empty() || shell_command.is_some() || out_of_band_tool_progress;

        // Compute fingerprint from sorted signatures so order doesn't matter.
        let fingerprint = if signatures.is_empty() {
            0
        } else {
            let mut sorted: Vec<&str> = signatures.iter().map(String::as_str).collect();
            sorted.sort_unstable();
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for sig in &sorted {
                sig.hash(&mut hasher);
            }
            hasher.finish()
        };

        // Check cross-turn loop before pushing this turn's fingerprint.
        let loop_warning = if fingerprint != 0 && self.turn_fingerprints.contains(&fingerprint) {
            Some(
                "Cross-turn loop detected: the same set of tool actions has repeated across \
                 consecutive turns. Break the pattern by trying a different approach or \
                 synthesizing a final answer from existing context."
                    .to_string(),
            )
        } else {
            None
        };

        if fingerprint != 0 {
            if self.turn_fingerprints.len() >= self.window_size {
                self.turn_fingerprints.pop_front();
            }
            self.turn_fingerprints.push_back(fingerprint);
        }

        // Track zero-mutation turns for stuck detection.
        if had_execution_progress {
            self.zero_mutation_turns = 0;
        } else if !signatures.is_empty() && !planning_active {
            self.zero_mutation_turns = self.zero_mutation_turns.saturating_add(1);
        }

        // Track identical shell failures across turns. Unlike the
        // fingerprint set above, this fires regardless of surrounding
        // variation (different reads between retries must not mask the
        // loop), and only when the error itself is unchanged.
        let identical_failure_warning = match failed_shell_key {
            Some(key) if self.last_failed_shell_key.as_deref() == Some(key) => {
                self.consecutive_same_failed_shell = self.consecutive_same_failed_shell.saturating_add(1);
                (self.consecutive_same_failed_shell >= IDENTICAL_SHELL_FAILURE_TURNS_THRESHOLD).then(|| {
                    format!(
                        "Identical shell failure in {} consecutive turns ({key}). The command fails with an unchanged error, so rerunning it cannot make progress. \
                         Inspect the underlying state the error names (file existence, manifest validity, toolchain availability), fix the root cause, then verify once. \
                         If the error already changed, ignore this warning and continue.",
                        self.consecutive_same_failed_shell,
                    )
                })
            }
            Some(key) => {
                self.last_failed_shell_key = Some(key.to_string());
                self.consecutive_same_failed_shell = 1;
                None
            }
            None => {
                self.last_failed_shell_key = None;
                self.consecutive_same_failed_shell = 0;
                None
            }
        };

        // Return loop warning first (higher priority), then stuck warning.
        if loop_warning.is_some() {
            return loop_warning;
        }

        if identical_failure_warning.is_some() {
            return identical_failure_warning;
        }

        if !planning_active && self.zero_mutation_turns >= STUCK_ZERO_MUTATION_THRESHOLD {
            return Some(format!(
                "No progress detected for {} consecutive turns (all read-only tool calls, \
                 no file mutations or command executions). Synthesize a final answer from \
                 existing context or ask the user for guidance.",
                self.zero_mutation_turns
            ));
        }

        None
    }

    /// Check if the tracker has detected a stuck pattern (for diagnostics).
    #[allow(dead_code, reason = "Intentional compatibility, platform, or test-only suppression.")]
    pub(crate) fn zero_mutation_turns(&self) -> usize {
        self.zero_mutation_turns
    }
}

/// Telemetry captured when the blocked-tool-call fuse trips. Consumed by
/// `finalize_turn` to populate `TurnBlockedEvent` (`last_tool`, the enforced
/// caps, and the streak/total at trip time) instead of `None`/zeros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockedToolRecoveryTelemetry {
    pub(crate) last_tool: String,
    pub(crate) consecutive_cap: usize,
    pub(crate) total_cap: usize,
    pub(crate) blocked_streak: usize,
    pub(crate) blocked_total: usize,
}

/// A tool call whose harness item the LLM runtime started while streaming,
/// keyed by provider call id. Entries are removed when the call is dispatched
/// (executed or rejected); leftovers at turn end never reached the pipeline
/// and are closed by teardown so the session log has no dangling items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamedToolCallItem {
    pub(crate) item_id: String,
    pub(crate) tool_name: String,
}

pub(crate) struct HarnessTurnState {
    pub run_id: TurnRunId,
    pub turn_id: TurnId,
    pub phase: TurnPhase,
    pub turn_started_at: Instant,
    /// Time spent waiting for explicit user input or an already-running
    /// command is excluded from the ordinary per-turn harness wall-clock budget.
    wait_started_at: Option<Instant>,
    excluded_wait_duration: Duration,
    pub tool_calls: usize,
    requested_tool_calls: u32,
    admitted_tool_calls: u32,
    failed_tool_calls: u32,
    denied_tool_calls: u32,
    preflight_failures: u32,
    reused_results: u32,
    spooled_results: u32,
    raw_spooled_bytes: u64,
    model_visible_output_bytes: u64,
    model_visible_tool_preview_bytes: usize,
    model_visible_tiny_tool_preview_bytes: usize,
    model_visible_tool_metadata_bytes: usize,
    model_visible_tool_preview_budget_exhausted: bool,
    suppressed_tool_previews: u32,
    /// Tool calls whose provider-visible preview was suppressed by either
    /// budget layer. Shared identity tracking keeps diagnostics idempotent
    /// when an in-progress response is replaced by its terminal result.
    suppressed_tool_call_ids: HashSet<String>,
    /// Remaining bytes of spool-page preview credit. Admitted spool-page reads
    /// grant credit so the designed paged-reading workflow stays model-visible
    /// even after the aggregate preview budget is exhausted. Capped per turn;
    /// reset every turn with the rest of the harness state.
    spool_page_preview_credit_bytes: usize,
    recovery_activations: u32,
    pub blocked_tool_calls: usize,
    pub consecutive_blocked_tool_calls: usize,
    /// Parallel preview-gate rejections are one failed decision, not separate
    /// permission denials. Allow one response to choose spool paging or finish.
    preview_gate_rejected_this_batch: bool,
    consecutive_preview_gate_batches: u8,
    /// Counts consecutive malformed/schema-invalid tool calls independently
    /// from policy denials. A valid admitted call resets this streak.
    pub consecutive_preflight_failures: usize,
    /// Counts consecutive assistant *text-only* responses in this turn.
    /// Admitted tool execution resets the streak so productive progress is
    /// not mistaken for a tool-free regeneration loop. Reset every turn.
    pub consecutive_assistant_text_responses: u32,
    /// Copilot/runtime tool progress that occurs inside a provider request and
    /// therefore has no ordinary `MessageRole::Tool` entry in history.
    out_of_band_tool_progress: bool,
    /// Whether a non-empty final assistant response was rendered for this turn.
    /// This is separate from conversation history because recovery code can
    /// append a message without sending it through the user-facing renderer.
    final_response_rendered: bool,
    /// Whether the final assistant response reached the harness event stream.
    /// The stream is optional in interactive-only runs, where the state is
    /// treated as emitted once the response was rendered.
    final_response_event_emitted: bool,
    /// Whether the streaming bridge emitted assistant output for this turn.
    /// This is kept separate from the final-response flag because a streamed
    /// commentary preamble can precede tool calls and is not itself a final
    /// answer.
    streamed_response_event_emitted: bool,
    /// Whether the final response was produced by deterministic recovery
    /// fallback rather than by a successful model synthesis.
    final_response_was_fallback: bool,
    /// Whether the provider refused this turn (`FinishReason::Refusal`). A
    /// refused turn is rolled back out of model-visible history and never
    /// auto-continued, so the flag travels to the session loop.
    turn_refused: bool,
    pub consecutive_spool_chunk_reads: usize,
    pub consecutive_same_shell_command_runs: usize,
    pub last_shell_command_signature: Option<String>,
    /// Last shell command that passed the full admission gate. The repetition
    /// guard records `last_shell_command_signature` earlier, so it must not be
    /// used as evidence of execution progress by the cross-turn tracker.
    pub last_admitted_shell_command_signature: Option<String>,
    /// Failure key (`signature::err::error-signature`) of the last failed
    /// shell execution this turn. Feeds cross-turn identical-failure loop
    /// detection; reset every turn with the rest of the harness state.
    last_failed_shell_key: Option<String>,
    pub consecutive_same_file_read_family_calls: usize,
    last_file_read_family_signature: Option<String>,
    /// Per-file-path read count, independent of slice (offset/limit/raw).
    /// Catches paginated reads of the same file that the slice-aware family
    /// key lets through. Reset every turn.
    file_read_path_counts: HashMap<String, usize>,
    pub(crate) seen_successful_readonly_signatures: HashSet<String>,
    streamed_tool_call_item_ids: HashMap<String, StreamedToolCallItem>,
    pub stop_hook_active: bool,
    pub seen_task_tracker_create_signatures: HashSet<String>,
    pub recently_written_files: HashSet<String>,
    pub tool_budget_warning_emitted: bool,
    pub tool_budget_exhausted_emitted: bool,
    /// Whether the first-notice wall-clock-exhaustion policy message has been
    /// emitted this turn. Mirrors `tool_budget_exhausted_emitted` so the full
    /// policy-violation message is sent once and subsequent rejected calls in
    /// the same batch get a compact stub instead of repeating it.
    pub wall_clock_exhausted_emitted: bool,
    /// Set when the current tool call was rejected specifically because the
    /// per-turn tool-call budget was exhausted. The Copilot adapter consumes
    /// this marker so budget rejections are not counted as permission denials.
    tool_budget_rejection_pending: bool,
    /// Set when the first wall-clock rejection fires; consumed after the tool
    /// batch by the handler to push a single "synthesize now" system directive
    /// *after* all tool responses (never interleaved between them).
    pub wall_clock_directive_pending: bool,
    /// Set when the first tool-call-budget rejection fires; consumed after the
    /// tool batch to push a single "synthesize now" system directive, mirroring
    /// `wall_clock_directive_pending`. Without this, tool-call budget
    /// exhaustion hard-broke the turn as `Blocked` with no synthesis pass, so
    /// plan mode never produced a plan (checkpoint turn_647 follow-up).
    pub tool_budget_directive_pending: bool,
    /// Set when a user grants additional session tool-call headroom. The
    /// corresponding model-facing directive is flushed after the current tool
    /// batch so it never splits an assistant tool-call/result sequence.
    session_limit_grant_directive_pending: bool,
    session_limit_granted: bool,
    /// Model-facing prompt-injection warning queued while a tool batch is
    /// executing. It is flushed after every batch result so it cannot split
    /// an assistant tool-call/result sequence on the provider wire.
    pending_auto_permission_probe_warning: Option<String>,
    pub recovery_reason: Option<String>,
    recovery_phase: RecoveryPhase,
    recovery_mode: Option<RecoveryMode>,
    recovery_retry_count: u8,
    /// Set for the one post-tool provider-recovery pass that must compact the
    /// older prefix before the next tool-enabled request.
    post_tool_compaction_pending: bool,
    /// Set when the provider identified the failed follow-up as a context
    /// capacity error. A failed/no-op recovery compaction must then produce a
    /// blocked handoff instead of retrying the same oversized request.
    post_tool_context_capacity_failure: bool,
    /// Set when the required recovery compaction failed or had no reducible
    /// prefix after a context-capacity rejection.
    post_tool_context_compaction_failed: bool,
    /// Explicit one-shot guard for the tool-enabled retry. This is separate
    /// from the tool-free recovery cycle counter because the two modes have
    /// different budgets and completion semantics.
    post_tool_tool_enabled_retry_used: bool,
    /// Counts how many times the post-tool follow-up failure path has
    /// scheduled a tool-free recovery pass within a single turn. Bounded by
    /// `MAX_POST_TOOL_RECOVERY_CYCLES` in the turn loop as a defense-in-depth
    /// backstop against any regression that re-triggers recovery cyclically.
    /// Resets naturally per turn because each turn constructs a fresh
    /// `HarnessTurnState`.
    post_tool_recovery_cycles: u8,
    /// Best-effort prose salvaged from a recovery synthesis response that was
    /// rejected for containing tool-call markup. Used as the final answer when
    /// all recovery retries are exhausted, instead of the canned fallback
    /// string, so gathered context is not discarded entirely.
    recovery_rejected_synthesis: Option<String>,
    /// Marks the fresh turn created by an approved plan handoff. This keeps
    /// recovery tool-enabled and lets the turn loop discard stale
    /// "tools are disabled" status responses from the planning turn.
    approved_plan_execution: bool,
    approved_plan_recovery_retries: u8,
    /// Set when the planning interview tool is permanently unavailable. The
    /// tool batch consumes this flag after all tool responses have been
    /// appended so the recovery directive is not interleaved with a batch.
    interview_denial_recovery_pending: bool,
    /// Set when the preflight validation circuit breaker trips. The tool
    /// batch consumes this flag after all tool responses (including drained
    /// skipped-call responses) have been appended, then arms a tool-free
    /// recovery pass so the model can synthesize a plain-text response
    /// instead of the turn hard-blocking and silently dropping an approved
    /// plan build.
    preflight_circuit_recovery_pending: bool,
    /// Set when the blocked-tool fuse trips. The current tool response batch
    /// drains this flag after all responses are appended, then arms one
    /// tool-free synthesis pass instead of terminating the turn immediately.
    blocked_tool_recovery_pending: bool,
    blocked_tool_recovery_reason: Option<String>,
    /// Blocked-call telemetry captured when the fuse armed or hard-broke the
    /// turn. Consumed once by `finalize_turn` for `TurnBlockedEvent` fields.
    blocked_tool_recovery_telemetry: Option<BlockedToolRecoveryTelemetry>,
    pub max_tool_calls: usize,
    pub max_tool_wall_clock: Duration,
    pub max_tool_retries: u32,
    /// Tracks consecutive relaxed continuation decisions. If this exceeds
    /// `MAX_CONSECUTIVE_RELAXED_CONTINUATIONS`, the turn ends to prevent
    /// infinite loops where the model keeps producing continuation-worthy
    /// text without making actual progress.
    pub consecutive_relaxed_continuations: u32,
    /// Last successfully observed incomplete `task_tracker` steps. Used as a
    /// fallback when the live probe fails so auto-continue is not dropped.
    incomplete_tracker_items_cache: Option<Vec<String>>,
}

impl HarnessTurnState {
    #[allow(
        clippy::too_many_arguments,
        reason = "Intentional compatibility, platform, or test-only suppression."
    )]
    pub(crate) fn new(
        run_id: TurnRunId,
        turn_id: TurnId,
        max_tool_calls: usize,
        max_tool_wall_clock_secs: u64,
        max_tool_retries: u32,
    ) -> Self {
        Self {
            run_id,
            turn_id,
            phase: TurnPhase::Preparing,
            turn_started_at: Instant::now(),
            wait_started_at: None,
            excluded_wait_duration: Duration::ZERO,
            tool_calls: 0,
            requested_tool_calls: 0,
            admitted_tool_calls: 0,
            failed_tool_calls: 0,
            denied_tool_calls: 0,
            preflight_failures: 0,
            reused_results: 0,
            spooled_results: 0,
            raw_spooled_bytes: 0,
            model_visible_output_bytes: 0,
            model_visible_tool_preview_bytes: 0,
            model_visible_tiny_tool_preview_bytes: 0,
            model_visible_tool_metadata_bytes: 0,
            model_visible_tool_preview_budget_exhausted: false,
            suppressed_tool_previews: 0,
            suppressed_tool_call_ids: HashSet::new(),
            spool_page_preview_credit_bytes: 0,
            recovery_activations: 0,
            blocked_tool_calls: 0,
            consecutive_blocked_tool_calls: 0,
            preview_gate_rejected_this_batch: false,
            consecutive_preview_gate_batches: 0,
            consecutive_preflight_failures: 0,
            consecutive_assistant_text_responses: 0,
            out_of_band_tool_progress: false,
            final_response_rendered: false,
            final_response_event_emitted: false,
            streamed_response_event_emitted: false,
            final_response_was_fallback: false,
            turn_refused: false,
            consecutive_spool_chunk_reads: 0,
            consecutive_same_shell_command_runs: 0,
            last_shell_command_signature: None,
            last_admitted_shell_command_signature: None,
            last_failed_shell_key: None,
            consecutive_same_file_read_family_calls: 0,
            last_file_read_family_signature: None,
            file_read_path_counts: HashMap::new(),
            seen_successful_readonly_signatures: HashSet::new(),
            streamed_tool_call_item_ids: HashMap::new(),
            stop_hook_active: false,
            seen_task_tracker_create_signatures: HashSet::new(),
            recently_written_files: HashSet::new(),
            tool_budget_warning_emitted: false,
            tool_budget_exhausted_emitted: false,
            wall_clock_exhausted_emitted: false,
            tool_budget_rejection_pending: false,
            wall_clock_directive_pending: false,
            tool_budget_directive_pending: false,
            session_limit_grant_directive_pending: false,
            session_limit_granted: false,
            pending_auto_permission_probe_warning: None,
            recovery_reason: None,
            recovery_phase: RecoveryPhase::Inactive,
            recovery_mode: None,
            recovery_retry_count: 0,
            post_tool_compaction_pending: false,
            post_tool_context_capacity_failure: false,
            post_tool_context_compaction_failed: false,
            post_tool_tool_enabled_retry_used: false,
            post_tool_recovery_cycles: 0,
            recovery_rejected_synthesis: None,
            approved_plan_execution: false,
            approved_plan_recovery_retries: 0,
            interview_denial_recovery_pending: false,
            preflight_circuit_recovery_pending: false,
            blocked_tool_recovery_pending: false,
            blocked_tool_recovery_reason: None,
            blocked_tool_recovery_telemetry: None,
            max_tool_calls,
            max_tool_wall_clock: Duration::from_secs(max_tool_wall_clock_secs),
            max_tool_retries,
            consecutive_relaxed_continuations: 0,
            incomplete_tracker_items_cache: None,
        }
    }

    pub(crate) fn apply_tracker_probe(
        &mut self,
        probe: crate::agent::runloop::unified::turn::tool_outcomes::helpers::TrackerProbeOutcome,
    ) -> Option<&[String]> {
        crate::agent::runloop::unified::turn::tool_outcomes::helpers::apply_tracker_probe_to_cache(
            &mut self.incomplete_tracker_items_cache,
            probe,
        )
    }

    pub(crate) fn has_tool_call_budget(&self) -> bool {
        self.max_tool_calls > 0
    }

    pub(crate) fn tool_budget_exhausted(&self) -> bool {
        self.has_tool_call_budget() && self.tool_calls >= self.max_tool_calls
    }

    pub(crate) fn tool_budget_exhaustion(&self) -> Option<ToolBudgetExhaustion> {
        self.tool_budget_exhausted().then_some(ToolBudgetExhaustion {
            used: self.tool_calls,
            max: self.max_tool_calls,
            remaining: self.remaining_tool_calls(),
        })
    }

    pub(crate) fn wall_clock_exhausted(&self) -> bool {
        self.effective_wall_clock_elapsed() >= self.max_tool_wall_clock
    }

    fn effective_wall_clock_elapsed(&self) -> Duration {
        let elapsed = self.turn_started_at.elapsed();
        let active_wait = self.wait_started_at.map(|started| started.elapsed()).unwrap_or(Duration::ZERO);
        elapsed.saturating_sub(self.excluded_wait_duration.saturating_add(active_wait))
    }

    /// Pause ordinary turn wall-clock accounting around an explicit external
    /// wait. This does not alter tool ceilings or cancellation behavior.
    pub(crate) fn begin_budget_excluded_wait(&mut self) {
        if self.wait_started_at.is_none() {
            self.wait_started_at = Some(Instant::now());
        }
    }

    pub(crate) fn end_budget_excluded_wait(&mut self) {
        if let Some(started) = self.wait_started_at.take() {
            self.excluded_wait_duration = self.excluded_wait_duration.saturating_add(started.elapsed());
        }
    }

    pub(crate) fn wall_clock_budget_exhaustion(&self) -> Option<ToolWallClockExhaustion> {
        self.wall_clock_exhausted()
            .then_some(ToolWallClockExhaustion { max_secs: self.max_tool_wall_clock.as_secs() })
    }

    pub(crate) fn record_tool_call(&mut self) {
        self.tool_calls = self.tool_calls.saturating_add(1);
    }

    pub(crate) fn record_requested_tool_calls(&mut self, count: usize) {
        self.requested_tool_calls = self
            .requested_tool_calls
            .saturating_add(u32::try_from(count).unwrap_or(u32::MAX));
    }

    pub(crate) fn record_admitted_tool_call(&mut self) {
        self.admitted_tool_calls = self.admitted_tool_calls.saturating_add(1);
    }

    pub(crate) fn admitted_tool_call_count(&self) -> u32 {
        self.admitted_tool_calls
    }

    pub(crate) fn record_failed_tool_call(&mut self) {
        self.failed_tool_calls = self.failed_tool_calls.saturating_add(1);
    }

    pub(crate) fn record_denied_tool_call(&mut self) {
        self.denied_tool_calls = self.denied_tool_calls.saturating_add(1);
    }

    pub(crate) fn record_tool_budget_rejection(&mut self) {
        self.tool_budget_rejection_pending = true;
    }

    pub(crate) fn take_tool_budget_rejection(&mut self) -> bool {
        std::mem::take(&mut self.tool_budget_rejection_pending)
    }

    pub(crate) fn record_tool_output_metrics(
        &mut self,
        reused: bool,
        spooled: bool,
        raw_spooled_bytes: u64,
        model_visible_output_bytes: usize,
    ) {
        if reused {
            self.reused_results = self.reused_results.saturating_add(1);
        }
        if spooled {
            self.spooled_results = self.spooled_results.saturating_add(1);
        }
        self.raw_spooled_bytes = self.raw_spooled_bytes.saturating_add(raw_spooled_bytes);
        self.record_model_visible_output_append(model_visible_output_bytes);
    }

    pub(crate) fn record_reused_result(&mut self) {
        self.reused_results = self.reused_results.saturating_add(1);
    }

    pub(crate) fn record_model_visible_output_append(&mut self, model_visible_output_bytes: usize) {
        self.model_visible_output_bytes = self
            .model_visible_output_bytes
            .saturating_add(u64::try_from(model_visible_output_bytes).unwrap_or(u64::MAX));
    }

    /// Test-only execution-budget shorthand so exec-mode tests avoid
    /// repeating the `64 KiB` denominator on every call.
    #[cfg(test)]
    pub(crate) fn bound_model_visible_tool_preview(&mut self, tool_name: Option<&str>, content: String) -> String {
        self.bound_model_visible_tool_preview_with_budget(
            tool_name,
            content,
            vtcode_config::constants::output_limits::TURN_PREVIEW_BUDGET_BYTES,
        )
    }

    /// Test-only wrapper for exercising the local limiter without a registry
    /// tool-call identifier.
    ///
    /// Tool output processing already applies a per-result preview limit, but
    /// a turn can still accumulate many independent previews (or repeatedly
    /// inspect a spool file). Once the aggregate budget is exhausted, retain
    /// only bounded metadata so recovery cannot amplify one diagnostic into a
    /// recursively growing prompt.
    ///
    /// Callers pass the effective turn budget (`turn_preview_budget_bytes`)
    /// so planning (`96 KiB`) and execution (`64 KiB`) share one accounting
    /// path instead of duplicated ledgers.
    #[cfg(test)]
    pub(crate) fn bound_model_visible_tool_preview_with_budget(
        &mut self,
        tool_name: Option<&str>,
        content: String,
        budget_bytes: usize,
    ) -> String {
        self.bound_model_visible_tool_preview_inner(None, tool_name, content, budget_bytes)
    }

    /// Call-aware provider-history boundary. Registry markers are observed
    /// before body-size bypasses, while locally generated suppression shares
    /// the same call-id ledger so an interim response and its replacement are
    /// counted once.
    pub(crate) fn bound_model_visible_tool_preview_for_call_with_budget(
        &mut self,
        tool_call_id: &str,
        tool_name: Option<&str>,
        content: String,
        budget_bytes: usize,
    ) -> String {
        if self.observe_upstream_preview_budget_exhaustion(tool_call_id, &content, budget_bytes) {
            return content;
        }
        self.bound_model_visible_tool_preview_inner(Some(tool_call_id), tool_name, content, budget_bytes)
    }

    fn bound_model_visible_tool_preview_inner(
        &mut self,
        tool_call_id: Option<&str>,
        tool_name: Option<&str>,
        content: String,
        budget_bytes: usize,
    ) -> String {
        if content.is_empty() || !tool_preview_has_visible_body(&content) {
            return content;
        }

        // Spool paging bypass: an admitted spool-page read banks credit that
        // keeps exactly that page model-visible. Only fully covered responses
        // are exempted; anything larger falls through to the normal budget
        // path below so oversized pages cannot silently bypass the bound.
        if content.len() <= self.spool_page_preview_credit_bytes {
            self.spool_page_preview_credit_bytes -= content.len();
            return content;
        }

        // Verifier bypass: payloads at or under `TINY_PREVIEW_BYPASS_BYTES`
        // use a separate finite per-turn reserve instead of the regular
        // preview budget. This keeps short checks available after that budget
        // is exhausted. Charge full serialized content so metadata overhead
        // cannot bypass either bound.
        if content.len() <= vtcode_config::constants::output_limits::TINY_PREVIEW_BYPASS_BYTES {
            let tiny_budget = vtcode_config::constants::output_limits::TURN_TINY_PREVIEW_BUDGET_BYTES;
            let remaining = tiny_budget.saturating_sub(self.model_visible_tiny_tool_preview_bytes);
            if content.len() <= remaining {
                self.model_visible_tiny_tool_preview_bytes =
                    self.model_visible_tiny_tool_preview_bytes.saturating_add(content.len());
                return content;
            }
            self.model_visible_tiny_tool_preview_bytes = tiny_budget;
            return self.suppress_model_visible_tool_preview(tool_call_id, tool_name, content);
        }

        let budget = budget_bytes.max(1);
        let remaining = budget.saturating_sub(self.model_visible_tool_preview_bytes);
        if !self.model_visible_tool_preview_budget_exhausted && content.len() <= remaining {
            self.model_visible_tool_preview_bytes = self.model_visible_tool_preview_bytes.saturating_add(content.len());
            if self.model_visible_tool_preview_bytes >= budget {
                self.model_visible_tool_preview_budget_exhausted = true;
            }
            return content;
        }

        self.model_visible_tool_preview_bytes = budget;
        self.suppress_model_visible_tool_preview(tool_call_id, tool_name, content)
    }

    fn suppress_model_visible_tool_preview(
        &mut self,
        tool_call_id: Option<&str>,
        tool_name: Option<&str>,
        content: String,
    ) -> String {
        self.model_visible_tool_preview_budget_exhausted = true;
        self.record_suppressed_tool_preview(tool_call_id);
        let metadata_remaining =
            MODEL_VISIBLE_TOOL_METADATA_BUDGET_BYTES.saturating_sub(self.model_visible_tool_metadata_bytes);
        if metadata_remaining == 0 {
            self.model_visible_tool_metadata_bytes = MODEL_VISIBLE_TOOL_METADATA_BUDGET_BYTES;
            return generic_tool_preview_metadata(content.len());
        }
        let metadata = bounded_tool_preview_metadata(tool_name, &content);
        if metadata.len() <= metadata_remaining {
            self.model_visible_tool_metadata_bytes =
                self.model_visible_tool_metadata_bytes.saturating_add(metadata.len());
            metadata
        } else {
            self.model_visible_tool_metadata_bytes = MODEL_VISIBLE_TOOL_METADATA_BUDGET_BYTES;
            // Bounded metadata overflowed the remaining budget: fall back to
            // the minimal stub to preserve the aggregate bound. The bounded
            // path already preserves spool_path/byte_count/completion_state
            // for all fits-budget cases.
            generic_tool_preview_metadata(content.len())
        }
    }

    fn record_suppressed_tool_preview(&mut self, tool_call_id: Option<&str>) {
        let should_count = match tool_call_id {
            Some(tool_call_id) => self.suppressed_tool_call_ids.insert(tool_call_id.to_string()),
            None => true,
        };
        if should_count {
            self.suppressed_tool_previews = self.suppressed_tool_previews.saturating_add(1);
        }
    }

    /// Merge the registry's authoritative aggregate-budget transition into
    /// the runloop state before provider-history body checks can return early.
    ///
    /// Registry exhaustion strips payload bodies and leaves the
    /// `preview_budget_exhausted` control marker. Treating the resulting JSON
    /// as "no visible body" before observing that marker left the inspection
    /// gate, recovery balancer, checkpoints, and ATIF diagnostics unaware of
    /// the exhaustion (turn 1163). The transition is monotonic and suppression
    /// accounting is keyed by tool-call id so response replacement is
    /// idempotent.
    pub(crate) fn observe_upstream_preview_budget_exhaustion(
        &mut self,
        tool_call_id: &str,
        content: &str,
        budget_bytes: usize,
    ) -> bool {
        // Registry responses are already bounded. Refuse to parse an
        // oversized marker candidate here so arbitrary local/MCP output cannot
        // force a second unbounded JSON parse before the local limiter runs.
        let exhausted = content.len() <= TOOL_PREVIEW_METADATA_PARSE_LIMIT_BYTES
            && content.contains("\"preview_budget_exhausted\"")
            && serde_json::from_str::<serde_json::Value>(content)
                .ok()
                .and_then(|value| value.get("preview_budget_exhausted").and_then(serde_json::Value::as_bool))
                == Some(true);
        if !exhausted {
            return false;
        }

        self.model_visible_tool_preview_bytes = self.model_visible_tool_preview_bytes.max(budget_bytes.max(1));
        self.model_visible_tool_preview_budget_exhausted = true;
        self.record_suppressed_tool_preview(Some(tool_call_id));
        true
    }

    /// Whether the per-turn model-visible tool preview budget is exhausted.
    /// Once exhausted, every further tool response is stored as a metadata
    /// stub without body content, so additional research calls cannot surface
    /// new evidence to the model. The turn balancer uses this to converge
    /// planning turns toward synthesis instead of blind retries.
    pub(crate) fn model_visible_preview_budget_exhausted(&self) -> bool {
        self.model_visible_tool_preview_budget_exhausted
    }

    /// Bank preview credit for an admitted spool-page read. Only fully covered
    /// responses are exempted, so credit never creates partial-visibility
    /// states; unused credit simply expires with the turn.
    pub(crate) fn grant_spool_page_preview_credit(&mut self, bytes: usize) {
        self.spool_page_preview_credit_bytes =
            MAX_SPOOL_PAGE_PREVIEW_CREDIT_BYTES.min(self.spool_page_preview_credit_bytes.saturating_add(bytes));
    }

    pub(crate) fn replace_model_visible_output_bytes(&mut self, previous_len: usize, new_len: usize) {
        let previous_len = u64::try_from(previous_len).unwrap_or(u64::MAX);
        self.model_visible_output_bytes = self.model_visible_output_bytes.saturating_sub(previous_len);
        self.record_model_visible_output_append(new_len);
    }

    pub(crate) fn snapshot_turn_diagnostics(
        &self,
        usage: vtcode_core::exec::events::Usage,
        low_signal_tool_calls: u32,
    ) -> vtcode_core::core::agent::snapshots::SnapshotTurnDiagnostics {
        vtcode_core::core::agent::snapshots::SnapshotTurnDiagnostics {
            usage,
            requested_tool_calls: self.requested_tool_calls,
            admitted_tool_calls: self.admitted_tool_calls,
            unadmitted_tool_calls: self.requested_tool_calls.saturating_sub(self.admitted_tool_calls),
            failed_tool_calls: self.failed_tool_calls,
            denied_tool_calls: self.denied_tool_calls,
            preflight_failures: self.preflight_failures,
            reused_results: self.reused_results,
            spooled_results: self.spooled_results,
            raw_spooled_bytes: self.raw_spooled_bytes,
            model_visible_output_bytes: self.model_visible_output_bytes,
            suppressed_tool_previews: self.suppressed_tool_previews,
            model_visible_tool_preview_budget_exhausted: self.model_visible_tool_preview_budget_exhausted,
            low_signal_tool_calls,
            recovery_activations: self.recovery_activations,
            ..Default::default()
        }
    }

    pub(crate) fn record_tool_call_with_warning(&mut self, threshold: f64) -> Option<ToolBudgetWarning> {
        self.record_tool_call();
        if !self.should_emit_tool_budget_warning(threshold) {
            return None;
        }

        let warning = ToolBudgetWarning {
            used: self.tool_calls,
            max: self.max_tool_calls,
            remaining: self.remaining_tool_calls(),
        };
        self.mark_tool_budget_warning_emitted();
        Some(warning)
    }

    pub(crate) fn record_tool_call_with_default_warning(&mut self) -> Option<ToolBudgetWarning> {
        self.record_tool_call_with_warning(TOOL_BUDGET_WARNING_THRESHOLD)
    }

    /// Record that the agent emitted a text-only response in this turn.
    /// This state is authoritative and survives history compaction, including
    /// inline tool boundaries that are not represented in `working_history`.
    pub(crate) fn record_assistant_text_response(&mut self) -> u32 {
        self.consecutive_assistant_text_responses = self.consecutive_assistant_text_responses.saturating_add(1);
        self.consecutive_assistant_text_responses
    }

    /// Break the text-only response streak after a tool call passes admission.
    /// Blocked and malformed attempts are not progress and retain the streak;
    /// their dedicated safeguards remain responsible for those failure loops.
    pub(crate) fn reset_assistant_text_response_streak(&mut self) {
        self.consecutive_assistant_text_responses = 0;
    }

    /// Record productive tool execution that is not represented by a normal
    /// tool-result message, such as an inline Copilot runtime call.
    pub(crate) fn record_out_of_band_tool_progress(&mut self) {
        self.out_of_band_tool_progress = true;
        self.reset_assistant_text_response_streak();
    }

    pub(crate) fn record_out_of_band_tool_call(&mut self) {
        self.record_requested_tool_calls(1);
        self.record_admitted_tool_call();
        self.record_out_of_band_tool_progress();
    }

    pub(crate) fn has_out_of_band_tool_progress(&self) -> bool {
        self.out_of_band_tool_progress
    }

    pub(crate) fn record_tool_budget_exhaustion_notice(&mut self) -> Option<ToolBudgetExhaustionNotice> {
        let exhaustion = self.tool_budget_exhaustion()?;
        let first_notice = !self.tool_budget_exhausted_emitted;
        if first_notice {
            self.mark_tool_budget_exhausted_emitted();
            self.tool_budget_directive_pending = true;
        }
        Some(ToolBudgetExhaustionNotice { exhaustion, first_notice })
    }

    /// Consume the pending tool-call-budget synthesis-directive flag. Returns
    /// `true` exactly once per turn (after the batch where exhaustion first
    /// fired). Mirrors `take_wall_clock_directive_pending`.
    pub(crate) fn take_tool_budget_directive_pending(&mut self) -> bool {
        std::mem::take(&mut self.tool_budget_directive_pending)
    }

    /// Record a user-approved session limit increase for the current turn.
    pub(crate) fn record_session_limit_grant(&mut self) {
        self.session_limit_granted = true;
        self.session_limit_grant_directive_pending = true;
    }

    pub(crate) fn has_session_limit_grant(&self) -> bool {
        self.session_limit_granted
    }

    /// Consume the pending model-facing session-limit guidance after the tool
    /// batch has appended all of its responses.
    pub(crate) fn take_session_limit_grant_directive_pending(&mut self) -> bool {
        std::mem::take(&mut self.session_limit_grant_directive_pending)
    }

    /// Record a wall-clock-budget rejection for the current tool call.
    ///
    /// Returns `None` when the budget is not exhausted. On the first exhausted
    /// call it flags `first_notice` (so the full policy message is emitted once)
    /// and arms `wall_clock_directive_pending` so the handler pushes a single
    /// "synthesize now" system directive *after* the tool batch completes.
    pub(crate) fn record_wall_clock_exhaustion_notice(&mut self) -> Option<ToolWallClockExhaustionNotice> {
        let exhaustion = self.wall_clock_budget_exhaustion()?;
        let first_notice = !self.wall_clock_exhausted_emitted;
        if first_notice {
            self.wall_clock_exhausted_emitted = true;
            self.wall_clock_directive_pending = true;
        }
        Some(ToolWallClockExhaustionNotice { exhaustion, first_notice })
    }

    /// Consume the pending wall-clock synthesis-directive flag. Returns `true`
    /// exactly once per turn (after the batch where exhaustion first fired).
    pub(crate) fn take_wall_clock_directive_pending(&mut self) -> bool {
        std::mem::take(&mut self.wall_clock_directive_pending)
    }

    pub(crate) fn record_blocked_tool_call(&mut self) -> usize {
        self.blocked_tool_calls = self.blocked_tool_calls.saturating_add(1);
        self.consecutive_blocked_tool_calls = self.consecutive_blocked_tool_calls.saturating_add(1);
        self.consecutive_blocked_tool_calls
    }

    pub(crate) fn reset_blocked_tool_call_streak(&mut self) {
        self.consecutive_blocked_tool_calls = 0;
    }

    pub(crate) fn record_preview_gate_rejection(&mut self) {
        self.preview_gate_rejected_this_batch = true;
    }

    /// Returns true after two blind-inspection batches without an admitted
    /// tool between them. A batch of parallel calls counts only once.
    pub(crate) fn finish_preview_gate_batch(&mut self) -> bool {
        if !std::mem::take(&mut self.preview_gate_rejected_this_batch) {
            return false;
        }
        self.consecutive_preview_gate_batches = self.consecutive_preview_gate_batches.saturating_add(1);
        self.consecutive_preview_gate_batches >= 2
    }

    pub(crate) fn reset_preview_gate_batches(&mut self) {
        self.preview_gate_rejected_this_batch = false;
        self.consecutive_preview_gate_batches = 0;
    }

    pub(crate) fn record_preflight_failure(&mut self) -> usize {
        self.consecutive_preflight_failures = self.consecutive_preflight_failures.saturating_add(1);
        self.preflight_failures = self.preflight_failures.saturating_add(1);
        self.consecutive_preflight_failures
    }

    pub(crate) fn reset_preflight_failure_streak(&mut self) {
        self.consecutive_preflight_failures = 0;
    }

    pub(crate) fn tool_budget_usage_ratio(&self) -> f64 {
        if !self.has_tool_call_budget() {
            0.0
        } else {
            self.tool_calls as f64 / self.max_tool_calls as f64
        }
    }

    pub(crate) fn remaining_tool_calls(&self) -> usize {
        self.max_tool_calls.saturating_sub(self.tool_calls)
    }

    pub(crate) fn should_emit_tool_budget_warning(&self, threshold: f64) -> bool {
        self.has_tool_call_budget() && !self.tool_budget_warning_emitted && self.tool_budget_usage_ratio() >= threshold
    }

    pub(crate) fn mark_tool_budget_warning_emitted(&mut self) {
        self.tool_budget_warning_emitted = true;
    }

    pub(crate) fn mark_tool_budget_exhausted_emitted(&mut self) {
        self.tool_budget_exhausted_emitted = true;
    }

    pub(crate) fn activate_recovery(&mut self, reason: impl Into<String>) -> bool {
        self.activate_recovery_with_mode(reason, RecoveryMode::ToolFreeSynthesis)
    }

    /// Arm a recovery pass. Returns `false` when a pass is already armed or in
    /// flight (`Pending`/`InPass`/`Completed`), so callers can skip the
    /// user-facing "scheduling" feedback instead of claiming a pass they did
    /// not schedule.
    pub(crate) fn activate_recovery_with_mode(&mut self, reason: impl Into<String>, mode: RecoveryMode) -> bool {
        if matches!(self.recovery_phase, RecoveryPhase::Inactive) {
            self.recovery_activations = self.recovery_activations.saturating_add(1);
            self.recovery_reason = Some(reason.into());
            self.recovery_phase = RecoveryPhase::Pending;
            self.recovery_mode = Some(mode);
            self.recovery_retry_count = 0;
            return true;
        }
        false
    }

    /// Arm the single tool-enabled retry used after a provider failure follows
    /// successful tool execution. The runloop consumes the
    /// compaction flag before consuming the recovery pass, so the retry sees
    /// the compacted prefix plus the current request and tool outputs.
    pub(crate) fn arm_post_tool_tool_enabled_retry(
        &mut self,
        reason: impl Into<String>,
        context_capacity_failure: bool,
    ) -> bool {
        if !matches!(self.recovery_phase, RecoveryPhase::Inactive) {
            return false;
        }

        self.recovery_activations = self.recovery_activations.saturating_add(1);
        self.recovery_reason = Some(reason.into());
        self.recovery_phase = RecoveryPhase::Pending;
        self.recovery_mode = Some(RecoveryMode::ToolEnabledRetry);
        self.recovery_retry_count = 0;
        self.post_tool_compaction_pending = true;
        self.post_tool_context_capacity_failure = context_capacity_failure;
        self.post_tool_tool_enabled_retry_used = true;
        true
    }

    pub(crate) fn is_recovery_active(&self) -> bool {
        matches!(self.recovery_phase, RecoveryPhase::Pending | RecoveryPhase::InPass)
    }

    pub(crate) fn recovery_reason(&self) -> Option<&str> {
        self.recovery_reason.as_deref()
    }

    pub(crate) fn recovery_pass_used(&self) -> bool {
        matches!(self.recovery_phase, RecoveryPhase::InPass | RecoveryPhase::Completed)
    }

    #[cfg(test)]
    fn recovery_mode(&self) -> Option<RecoveryMode> {
        self.recovery_mode
    }

    /// Switch to tool-free synthesis mode and reset the recovery phase back to
    /// `Pending` so the next loop iteration can consume it.
    ///
    /// Unlike `activate_recovery_with_mode` (which is a guarded no-op once a
    /// pass is in flight), this unconditionally forces the phase to `Pending`,
    /// covering `Inactive`, `InPass`, and `Completed`. This is required
    /// because the post-tool follow-up failure path runs from a *non-recovery*
    /// turn (phase == `Inactive`): `activate_recovery_with_mode` would set the
    /// reason and mode but leave the phase as `Inactive`, so
    /// `consume_recovery_pass()` would return `false`, `tool_free_recovery`
    /// would evaluate to `false`, and tools would never be disabled at the API
    /// level.
    ///
    /// When transitioning from `Inactive`, this also resets the retry counter
    /// and seeds a default `recovery_reason` (mirroring
    /// `activate_recovery_with_mode`) so the `[Recovery Mode]` request block
    /// reports why recovery was engaged.
    ///
    /// Returns `true` when the phase actually changed.
    pub(crate) fn switch_to_tool_free_recovery(&mut self) -> bool {
        let was_inactive = matches!(self.recovery_phase, RecoveryPhase::Inactive);
        self.recovery_mode = Some(RecoveryMode::ToolFreeSynthesis);
        let changed = !matches!(self.recovery_phase, RecoveryPhase::Pending);
        self.recovery_phase = RecoveryPhase::Pending;
        if was_inactive {
            self.recovery_activations = self.recovery_activations.saturating_add(1);
            self.recovery_retry_count = 0;
            if self.recovery_reason.is_none() {
                self.recovery_reason = Some("post-tool follow-up failure".to_string());
            }
        }
        changed
    }

    /// Arm the bounded tool-free plan synthesis fallback used when
    /// `request_user_input` is permanently unavailable in the current
    /// runtime. The directive is flushed after the current tool batch so
    /// provider message ordering remains valid.
    pub(crate) fn arm_interview_denial_recovery(&mut self) {
        self.interview_denial_recovery_pending = true;
    }

    pub(crate) fn take_interview_denial_recovery(&mut self) -> bool {
        std::mem::take(&mut self.interview_denial_recovery_pending)
    }

    pub(crate) fn interview_denial_recovery_pending(&self) -> bool {
        self.interview_denial_recovery_pending
    }

    /// Arm the preflight circuit-breaker recovery so the tool batch can flush
    /// its synthesis directive after all tool responses land.
    pub(crate) fn arm_preflight_circuit_recovery(&mut self) {
        self.preflight_circuit_recovery_pending = true;
    }

    pub(crate) fn take_preflight_circuit_recovery(&mut self) -> bool {
        std::mem::take(&mut self.preflight_circuit_recovery_pending)
    }

    /// Arm the bounded tool-free recovery used after repeated blocked calls.
    /// The response batch consumes this flag after appending every required
    /// tool response, preserving provider message ordering. The telemetry
    /// snapshot is kept for the `TurnBlockedEvent` emitted at turn finalize.
    pub(crate) fn arm_blocked_tool_recovery(
        &mut self,
        reason: impl Into<String>,
        telemetry: BlockedToolRecoveryTelemetry,
    ) {
        self.blocked_tool_recovery_pending = true;
        self.blocked_tool_recovery_reason = Some(reason.into());
        self.blocked_tool_recovery_telemetry = Some(telemetry);
    }

    /// Record blocked-call telemetry without arming recovery. Used when the
    /// fuse hard-breaks the turn in recovery mode: no recovery pass is
    /// scheduled, but `finalize_turn` still needs the values for
    /// `TurnBlockedEvent`.
    pub(crate) fn record_blocked_tool_recovery_telemetry(&mut self, telemetry: BlockedToolRecoveryTelemetry) {
        self.blocked_tool_recovery_telemetry = Some(telemetry);
    }

    /// One-shot accessor for the blocked-call telemetry captured at fuse-trip
    /// time; consumed by `finalize_turn`.
    pub(crate) fn take_blocked_tool_recovery_telemetry(&mut self) -> Option<BlockedToolRecoveryTelemetry> {
        self.blocked_tool_recovery_telemetry.take()
    }

    pub(crate) fn take_blocked_tool_recovery(&mut self) -> bool {
        std::mem::take(&mut self.blocked_tool_recovery_pending)
    }

    pub(crate) fn blocked_tool_recovery_pending(&self) -> bool {
        self.blocked_tool_recovery_pending
    }

    pub(crate) fn take_blocked_tool_recovery_reason(&mut self) -> Option<String> {
        self.blocked_tool_recovery_reason.take()
    }

    pub(crate) fn recovery_is_tool_free(&self) -> bool {
        matches!(self.recovery_mode, Some(RecoveryMode::ToolFreeSynthesis))
    }

    #[cfg(test)]
    pub(crate) fn post_tool_compaction_pending(&self) -> bool {
        self.post_tool_compaction_pending
    }

    pub(crate) fn take_post_tool_compaction_pending(&mut self) -> bool {
        std::mem::take(&mut self.post_tool_compaction_pending)
    }

    pub(crate) fn post_tool_context_capacity_failure(&self) -> bool {
        self.post_tool_context_capacity_failure
    }

    pub(crate) fn mark_post_tool_context_compaction_failed(&mut self) {
        self.post_tool_context_compaction_failed = true;
    }

    pub(crate) fn post_tool_context_compaction_failed(&self) -> bool {
        self.post_tool_context_compaction_failed
    }

    pub(crate) fn post_tool_tool_enabled_retry_used(&self) -> bool {
        self.post_tool_tool_enabled_retry_used
    }

    pub(crate) fn set_approved_plan_execution(&mut self, active: bool) {
        self.approved_plan_execution = active;
        self.approved_plan_recovery_retries = 0;
    }

    pub(crate) fn queue_auto_permission_probe_warning(&mut self, warning: String) -> bool {
        if self.pending_auto_permission_probe_warning.is_some() {
            return false;
        }
        self.pending_auto_permission_probe_warning = Some(warning);
        true
    }

    pub(crate) fn take_auto_permission_probe_warning(&mut self) -> Option<String> {
        self.pending_auto_permission_probe_warning.take()
    }

    pub(crate) fn final_response_rendered(&self) -> bool {
        self.final_response_rendered
    }

    pub(crate) fn final_response_event_emitted(&self) -> bool {
        self.final_response_event_emitted
    }

    pub(crate) fn mark_final_response_rendered(&mut self) {
        self.final_response_rendered = true;
    }

    pub(crate) fn mark_final_response_event_emitted(&mut self) {
        self.final_response_event_emitted = true;
    }

    pub(crate) fn mark_streamed_response_event_emitted(&mut self) {
        self.streamed_response_event_emitted = true;
    }

    pub(crate) fn reset_streamed_response_event_emitted(&mut self) {
        self.streamed_response_event_emitted = false;
    }

    pub(crate) fn streamed_response_event_emitted(&self) -> bool {
        self.streamed_response_event_emitted
    }

    pub(crate) fn mark_final_response_fallback(&mut self) {
        self.final_response_was_fallback = true;
    }

    pub(crate) fn final_response_was_fallback(&self) -> bool {
        self.final_response_was_fallback
    }

    pub(crate) fn mark_turn_refused(&mut self) {
        self.turn_refused = true;
    }

    pub(crate) fn turn_refused(&self) -> bool {
        self.turn_refused
    }

    pub(crate) fn is_approved_plan_execution(&self) -> bool {
        self.approved_plan_execution
    }

    pub(crate) fn approved_plan_recovery_retries(&self) -> u8 {
        self.approved_plan_recovery_retries
    }

    pub(crate) fn record_approved_plan_recovery_retry(&mut self) {
        self.approved_plan_recovery_retries = self.approved_plan_recovery_retries.saturating_add(1);
    }

    pub(crate) fn consume_recovery_pass(&mut self) -> bool {
        if !matches!(self.recovery_phase, RecoveryPhase::Pending) {
            return false;
        }
        self.recovery_phase = RecoveryPhase::InPass;
        true
    }

    pub(crate) fn finish_recovery_pass(&mut self) -> bool {
        if !matches!(self.recovery_phase, RecoveryPhase::InPass) {
            return false;
        }
        self.recovery_phase = RecoveryPhase::Completed;
        true
    }

    /// Retry the recovery pass by resetting the phase back to `Pending`
    /// so the next loop iteration re-enters tool-free recovery mode.
    /// Increments the retry counter; the caller is responsible for checking
    /// `recovery_retry_count()` against its own limit.
    /// Only works if a recovery pass has been consumed (phase is InPass or Completed).
    pub(crate) fn retry_recovery_pass(&mut self) -> bool {
        if matches!(self.recovery_phase, RecoveryPhase::InPass | RecoveryPhase::Completed) {
            self.recovery_phase = RecoveryPhase::Pending;
            self.recovery_retry_count += 1;
            true
        } else {
            false
        }
    }

    pub(crate) fn recovery_retry_count(&self) -> u8 {
        self.recovery_retry_count
    }

    /// Record best-effort prose salvaged from a rejected recovery synthesis
    /// response. Later rejections overwrite earlier ones (the latest attempt
    /// is the most complete).
    pub(crate) fn record_recovery_rejected_synthesis(&mut self, text: String) {
        if !text.trim().is_empty() {
            self.recovery_rejected_synthesis = Some(text);
        }
    }

    pub(crate) fn take_recovery_rejected_synthesis(&mut self) -> Option<String> {
        self.recovery_rejected_synthesis.take()
    }

    pub(crate) fn post_tool_recovery_cycles(&self) -> u8 {
        self.post_tool_recovery_cycles
    }

    /// Increment the tool-free post-tool recovery cycle counter. Returns the new value.
    pub(crate) fn increment_post_tool_recovery_cycle(&mut self) -> u8 {
        self.post_tool_recovery_cycles = self.post_tool_recovery_cycles.saturating_add(1);
        self.post_tool_recovery_cycles
    }

    pub(crate) fn record_spool_chunk_read(&mut self) -> usize {
        self.consecutive_spool_chunk_reads = self.consecutive_spool_chunk_reads.saturating_add(1);
        self.consecutive_spool_chunk_reads
    }

    pub(crate) fn reset_spool_chunk_read_streak(&mut self) {
        self.consecutive_spool_chunk_reads = 0;
    }

    pub(crate) fn record_shell_command_run(&mut self, signature: String) -> usize {
        if self.last_shell_command_signature.as_deref() == Some(signature.as_str()) {
            self.consecutive_same_shell_command_runs = self.consecutive_same_shell_command_runs.saturating_add(1);
        } else {
            self.last_shell_command_signature = Some(signature);
            self.consecutive_same_shell_command_runs = 1;
        }

        self.consecutive_same_shell_command_runs
    }

    pub(crate) fn reset_shell_command_run_streak(&mut self) {
        self.last_shell_command_signature = None;
        self.consecutive_same_shell_command_runs = 0;
    }

    pub(crate) fn record_admitted_shell_command(&mut self, signature: String) {
        self.last_admitted_shell_command_signature = Some(signature);
    }

    /// Remember this turn's failed shell execution, keyed by command plus
    /// first-line error text. The cross-turn tracker compares the key across
    /// turns; only byte-identical command/error pairs extend the streak, so
    /// retries after a genuine fix (changed error or success) start over.
    /// The last failure wins: one representative per turn is enough because
    /// the streak requires the *same* key in consecutive turns.
    pub(crate) fn record_failed_shell_command(&mut self, signature: String, error_signature: String) {
        self.last_failed_shell_key = Some(format!("{signature}::err::{error_signature}"));
    }

    pub(crate) fn last_failed_shell_key(&self) -> Option<&str> {
        self.last_failed_shell_key.as_deref()
    }

    pub(crate) fn record_file_read_family_call(&mut self, signature: String) -> usize {
        if self.last_file_read_family_signature.as_deref() == Some(signature.as_str()) {
            self.consecutive_same_file_read_family_calls =
                self.consecutive_same_file_read_family_calls.saturating_add(1);
        } else {
            self.last_file_read_family_signature = Some(signature);
            self.consecutive_same_file_read_family_calls = 1;
        }

        self.consecutive_same_file_read_family_calls
    }

    pub(crate) fn reset_file_read_family_streak(&mut self) {
        self.last_file_read_family_signature = None;
        self.consecutive_same_file_read_family_calls = 0;
    }

    /// Record a read of `path` and return the total count of reads for that
    /// path this turn. Independent of slice (offset/limit/raw) — catches
    /// paginated reads of the same file that the slice-aware family key lets
    /// through.
    pub(crate) fn record_file_read_path_call(&mut self, path: String) -> usize {
        let count = self.file_read_path_counts.entry(path).or_insert(0);
        *count = count.saturating_add(1);
        *count
    }

    #[cfg(test)]
    fn reset_file_read_path_counts(&mut self) {
        self.file_read_path_counts.clear();
    }

    pub(crate) fn record_written_file(&mut self, path: &str) {
        self.recently_written_files.insert(path.to_string());
    }

    pub(crate) fn was_recently_written(&self, path: &str) -> bool {
        self.recently_written_files.contains(path)
    }

    pub(crate) fn record_task_tracker_create_signature(&mut self, signature: String) -> bool {
        self.seen_task_tracker_create_signatures.insert(signature)
    }

    pub(crate) fn clear_task_tracker_create_signatures(&mut self) {
        self.seen_task_tracker_create_signatures.clear();
    }

    pub(crate) fn record_successful_readonly_signature(&mut self, signature: String) -> bool {
        self.seen_successful_readonly_signatures.insert(signature)
    }

    pub(crate) fn has_successful_readonly_signature(&self, signature: &str) -> bool {
        self.seen_successful_readonly_signatures.contains(signature)
    }

    pub(crate) fn remember_streamed_tool_call_items<I>(&mut self, items: I)
    where
        I: IntoIterator<Item = (String, StreamedToolCallItem)>,
    {
        self.streamed_tool_call_item_ids.extend(items);
    }

    pub(crate) fn take_streamed_tool_call_item_id(&mut self, tool_call_id: &str) -> Option<StreamedToolCallItem> {
        self.streamed_tool_call_item_ids.remove(tool_call_id)
    }

    /// Drain every streamed tool-call item still registered. Turn teardown
    /// uses this to close items the LLM runtime started but whose calls never
    /// reached the pipeline (rejected, dropped, or interrupted mid-batch), so
    /// they do not dangle as `item.started` forever.
    pub(crate) fn take_all_streamed_tool_call_item_ids(&mut self) -> Vec<(String, StreamedToolCallItem)> {
        self.streamed_tool_call_item_ids.drain().collect()
    }

    pub(crate) fn set_phase(&mut self, phase: TurnPhase) {
        self.phase = phase;
    }

    pub(crate) fn execution_snapshot(&self) -> TurnExecutionSnapshot {
        TurnExecutionSnapshot {
            run_id: self.run_id.0.clone(),
            turn_id: self.turn_id.0.clone(),
            phase: self.phase.into(),
            max_tool_calls: self.max_tool_calls,
            max_tool_wall_clock_secs: self.max_tool_wall_clock.as_secs(),
            max_tool_retries: self.max_tool_retries,
        }
    }
}

fn preview_spool_path(object: Option<&serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    object
        .and_then(|value| value.get("spool_path"))
        .and_then(serde_json::Value::as_str)
        .map(|path| bounded_diagnosis_preview(path, TOOL_PREVIEW_METADATA_STRING_LIMIT))
}

fn preview_byte_count(object: Option<&serde_json::Map<String, serde_json::Value>>, fallback_len: usize) -> u64 {
    object
        .and_then(|value| {
            [
                "original_bytes",
                "spooled_bytes",
                "total_output_bytes",
                "total_bytes",
                "output_bytes",
                "byte_count",
                "bytes",
            ]
            .into_iter()
            .find_map(|key| value.get(key).and_then(serde_json::Value::as_u64))
        })
        .unwrap_or_else(|| u64::try_from(fallback_len).unwrap_or(u64::MAX))
}

fn preview_completion_state(object: Option<&serde_json::Map<String, serde_json::Value>>) -> &'static str {
    let Some(value) = object else {
        return "unknown";
    };
    if value.get("spool_pending").and_then(serde_json::Value::as_bool) == Some(true) {
        return "pending";
    }
    let exited = value.get("spool_complete").and_then(serde_json::Value::as_bool) == Some(true)
        || value.get("is_exited").and_then(serde_json::Value::as_bool) == Some(true)
        || value.get("exit_code").and_then(serde_json::Value::as_i64).is_some()
        || value.get("exit_code").and_then(serde_json::Value::as_u64).is_some()
        || value.get("success").and_then(serde_json::Value::as_bool) == Some(true)
        || value.get("command_success").and_then(serde_json::Value::as_bool) == Some(true)
        || matches!(value.get("status").and_then(serde_json::Value::as_str), Some("completed" | "success"))
        || matches!(value.get("outcome").and_then(serde_json::Value::as_str), Some("completed" | "success"));
    if exited { "complete" } else { "unknown" }
}

/// Payload-body fields counted against the preview budget. Outcome/control
/// metadata alone must not consume budget or trigger suppression.
///
/// Keep in sync with `PAYLOAD_BODY_FIELDS` in
/// `crates/codegen/vtcode-core/src/tools/registry/output_processing.rs` (same
/// field list; this side uses a broader visibility predicate covering
/// non-string bodies): the two lists must agree or Layer1/Layer2 accounting
/// diverges.
const TOOL_PREVIEW_BODY_FIELDS: [&str; 5] = ["output", "preview", "content", "stdout", "stderr"];

fn tool_preview_body_value_is_visible(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => !text.is_empty(),
        serde_json::Value::Array(items) => !items.is_empty(),
        serde_json::Value::Object(map) => !map.is_empty(),
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => true,
        serde_json::Value::Null => false,
    }
}

fn tool_preview_has_visible_body(content: &str) -> bool {
    if content.len() > TOOL_PREVIEW_METADATA_PARSE_LIMIT_BYTES {
        // Oversized plain text and JSON both consume the preview budget. Do
        // not parse an untrusted body merely to discover that it is large.
        return true;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        // Non-JSON previews (plain text) always consume budget.
        return true;
    };
    let Some(object) = value.as_object() else {
        return !content.is_empty();
    };
    TOOL_PREVIEW_BODY_FIELDS
        .iter()
        .filter_map(|field| object.get(*field))
        .any(tool_preview_body_value_is_visible)
}

fn bounded_tool_preview_metadata(tool_name: Option<&str>, content: &str) -> String {
    let parsed = (content.len() <= TOOL_PREVIEW_METADATA_PARSE_LIMIT_BYTES)
        .then(|| serde_json::from_str::<serde_json::Value>(content).ok())
        .flatten();
    let object = parsed.as_ref().and_then(serde_json::Value::as_object);

    let spool_path = preview_spool_path(object);
    let byte_count = preview_byte_count(object, content.len());
    let completion_state = preview_completion_state(object);

    let diagnosis = object
        .and_then(|value| value.get("diagnosis"))
        .and_then(serde_json::Value::as_object)
        .map(|diagnosis| {
            let mut bounded = serde_json::Map::new();
            for key in ["observed", "likely_cause", "next_action"] {
                if let Some(value) = diagnosis.get(key).and_then(serde_json::Value::as_str) {
                    bounded.insert(
                        key.to_string(),
                        serde_json::Value::String(bounded_diagnosis_preview(value, TOOL_PREVIEW_METADATA_STRING_LIMIT)),
                    );
                }
            }
            serde_json::Value::Object(bounded)
        });

    let note = if spool_path.is_some() {
        "Aggregate tool preview budget exhausted; complete output remains in the internal spool and current-session tool-output viewer."
    } else {
        "Aggregate tool preview budget exhausted; outcome metadata is preserved below. Do not repeat or rephrase this call solely to recover hidden output."
    };
    let mut metadata = serde_json::json!({
        "tool": tool_name.map(|name| bounded_preview_string(name, TOOL_PREVIEW_METADATA_STRING_LIMIT)),
        "spool_path": spool_path,
        "byte_count": byte_count,
        "completion_state": completion_state,
        "preview_budget_exhausted": true,
        "note": note,
    });
    if let Some(diagnosis) = diagnosis {
        metadata["diagnosis"] = diagnosis;
    }
    if let Some(object) = object {
        if let Some(error) = object.get("error")
            && let Some(bounded) = bounded_tool_failure_metadata(error)
        {
            metadata["error"] = bounded;
        }
        for key in [
            "error_summary",
            "original_error",
            "message",
            "stderr",
            "stderr_preview",
            "critical_note",
            "error_class",
            "category",
            "retry_summary",
            "recovery_suggestions",
        ] {
            let Some(value) = object.get(key) else {
                continue;
            };
            if let Some(bounded) = bounded_tool_failure_metadata(value) {
                metadata[key] = bounded;
            }
        }
        for key in [
            "success",
            "exit_code",
            "command_success",
            "blocked",
            "verification_required",
            "failure_kind",
            "status",
            "outcome",
            "output_truncated",
            "has_more",
            "next_action",
            "retryable",
            "is_exited",
            "spool_complete",
            "spool_pending",
            // Exec continuity + verifier metadata: session-vtcode-20260913T074747Z
            // stubs dropped `session_id`/`command`/`backend`, so the model could
            // neither continue pipe sessions via `write_stdin` nor tell which
            // verifier produced the stub. These scalars are bounded (strings
            // capped at 512 chars) and never carry payload bodies.
            "command",
            "session_id",
            "backend",
            "working_directory",
            "process_id",
            "wall_time",
            "waited_seconds",
            "total_output_bytes",
            "spooled_bytes",
            "matched_count",
            "truncated",
            "content_type",
        ] {
            let Some(value) = object.get(key) else {
                continue;
            };
            let bounded = match value {
                serde_json::Value::String(value) => {
                    serde_json::Value::String(bounded_diagnosis_preview(value, TOOL_PREVIEW_METADATA_STRING_LIMIT))
                }
                serde_json::Value::Bool(_) | serde_json::Value::Number(_) | serde_json::Value::Null => value.clone(),
                _ => continue,
            };
            metadata[key] = bounded;
        }
    }
    metadata.to_string()
}

fn generic_tool_preview_metadata(byte_count: usize) -> String {
    serde_json::json!({
        "byte_count": u64::try_from(byte_count).unwrap_or(u64::MAX),
        "preview_budget_exhausted": true,
        "suppressed": true,
    })
    .to_string()
}

fn bounded_tool_failure_metadata(value: &serde_json::Value) -> Option<serde_json::Value> {
    bounded_tool_failure_metadata_at_depth(value, TOOL_PREVIEW_METADATA_MAX_DEPTH)
}

fn bounded_tool_failure_metadata_at_depth(value: &serde_json::Value, depth: usize) -> Option<serde_json::Value> {
    if depth == 0 {
        return None;
    }

    match value {
        serde_json::Value::String(value) => {
            Some(serde_json::Value::String(bounded_diagnosis_preview(value, TOOL_PREVIEW_METADATA_STRING_LIMIT)))
        }
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) | serde_json::Value::Null => Some(value.clone()),
        serde_json::Value::Array(values) => {
            let bounded = values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .take(4)
                .map(|value| {
                    serde_json::Value::String(bounded_diagnosis_preview(value, TOOL_PREVIEW_METADATA_STRING_LIMIT))
                })
                .collect::<Vec<_>>();
            (!bounded.is_empty()).then_some(serde_json::Value::Array(bounded))
        }
        serde_json::Value::Object(object) => {
            let mut bounded = serde_json::Map::new();
            for key in [
                "tool_name",
                "error_type",
                "category",
                "message",
                "original_error",
                "retryable",
                "is_recoverable",
                "partial_state_possible",
                "rollback_performed",
                "circuit_breaker_impact",
                "retry_delay_ms",
                "retry_after_ms",
            ] {
                let Some(value) = object.get(key) else {
                    continue;
                };
                if let Some(value) = bounded_tool_failure_metadata_at_depth(value, depth - 1) {
                    bounded.insert(key.to_string(), value);
                }
            }
            if let Some(value) = object.get("recovery_suggestions")
                && let Some(value) = bounded_tool_failure_metadata_at_depth(value, depth - 1)
            {
                bounded.insert("recovery_suggestions".to_string(), value);
            }
            (!bounded.is_empty()).then_some(serde_json::Value::Object(bounded))
        }
    }
}

fn bounded_preview_string(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn bounded_diagnosis_preview(value: &str, limit: usize) -> String {
    let ansi_free = vtcode_commons::ansi::strip_ansi(value);
    let sanitized = vtcode_commons::sanitizer::sanitize_provider_diagnostic(ansi_free.as_bytes());
    bounded_preview_string(sanitized.trim(), limit)
}

pub(crate) struct RunLoopContext<'a> {
    pub renderer: &'a mut AnsiRenderer,
    pub handle: &'a InlineHandle,
    pub tool_registry: &'a mut ToolRegistry,
    pub tool_result_cache: &'a Arc<RwLock<ToolResultCache>>,
    pub tool_permission_cache: &'a Arc<RwLock<ToolPermissionCache>>,
    pub permissions_state: &'a Arc<RwLock<PermissionsConfig>>,
    pub decision_ledger: &'a Arc<RwLock<DecisionTracker>>,
    pub session_stats: &'a mut SessionStats,
    pub plan_session: &'a mut PlanningWorkflowSessionState,
    pub mcp_panel_state: &'a mut McpPanelState,
    pub approval_recorder: &'a ApprovalRecorder,
    pub session: &'a mut InlineSession,
    pub safety_validator: Option<&'a Arc<ToolCallSafetyValidator>>,
    pub traj: &'a TrajectoryLogger,
    pub harness_state: &'a mut HarnessTurnState,
    pub harness_emitter: Option<&'a HarnessEventEmitter>,
    pub auto_permission: Option<AutoPermissionRuntimeContext<'a>>,
    /// Whether ordinary confirmation prompts are bypassed for this turn.
    pub skip_confirmations: bool,
    /// Whether the session is operating under full-auto policy.
    pub full_auto: bool,
    pub active_agent_permissions: Option<&'a AgentPermissionsConfig>,
    /// Name of the currently active agent, if known
    pub agent_name: Option<String>,
    /// Configured execution agent used when a plan is approved from the
    /// dedicated planning agent without a previous agent to restore.
    pub default_primary_agent: Option<String>,
    /// Whether the current agent is a subagent
    pub is_subagent: bool,
}

pub(crate) struct AutoPermissionRuntimeContext<'a> {
    pub config: &'a CoreAgentConfig,
    pub vt_cfg: Option<&'a VTCodeConfig>,
    pub provider_client: &'a mut dyn uni::LLMProvider,
    pub working_history: &'a [uni::Message],
}

impl<'a> RunLoopContext<'a> {
    #[expect(
        clippy::too_many_arguments,
        reason = "Intentional compatibility, platform, test, or API-shape suppression."
    )]
    pub(crate) fn new(
        renderer: &'a mut AnsiRenderer,
        handle: &'a InlineHandle,
        tool_registry: &'a mut ToolRegistry,
        _tools: &'a Arc<RwLock<Vec<uni::ToolDefinition>>>,
        tool_result_cache: &'a Arc<RwLock<ToolResultCache>>,
        tool_permission_cache: &'a Arc<RwLock<ToolPermissionCache>>,
        permissions_state: &'a Arc<RwLock<PermissionsConfig>>,
        decision_ledger: &'a Arc<RwLock<DecisionTracker>>,
        session_stats: &'a mut SessionStats,
        plan_session: &'a mut PlanningWorkflowSessionState,
        mcp_panel_state: &'a mut McpPanelState,
        approval_recorder: &'a ApprovalRecorder,
        session: &'a mut InlineSession,
        safety_validator: Option<&'a Arc<ToolCallSafetyValidator>>,
        traj: &'a TrajectoryLogger,
        harness_state: &'a mut HarnessTurnState,
        harness_emitter: Option<&'a HarnessEventEmitter>,
    ) -> Self {
        Self::new_with_auto_permission_context(
            renderer,
            handle,
            tool_registry,
            _tools,
            tool_result_cache,
            tool_permission_cache,
            permissions_state,
            decision_ledger,
            session_stats,
            plan_session,
            mcp_panel_state,
            approval_recorder,
            session,
            safety_validator,
            traj,
            harness_state,
            harness_emitter,
            None,
            false,
            false,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Intentional compatibility, platform, test, or API-shape suppression."
    )]
    pub(crate) fn new_with_auto_permission_context(
        renderer: &'a mut AnsiRenderer,
        handle: &'a InlineHandle,
        tool_registry: &'a mut ToolRegistry,
        _tools: &'a Arc<RwLock<Vec<uni::ToolDefinition>>>,
        tool_result_cache: &'a Arc<RwLock<ToolResultCache>>,
        tool_permission_cache: &'a Arc<RwLock<ToolPermissionCache>>,
        permissions_state: &'a Arc<RwLock<PermissionsConfig>>,
        decision_ledger: &'a Arc<RwLock<DecisionTracker>>,
        session_stats: &'a mut SessionStats,
        plan_session: &'a mut PlanningWorkflowSessionState,
        mcp_panel_state: &'a mut McpPanelState,
        approval_recorder: &'a ApprovalRecorder,
        session: &'a mut InlineSession,
        safety_validator: Option<&'a Arc<ToolCallSafetyValidator>>,
        traj: &'a TrajectoryLogger,
        harness_state: &'a mut HarnessTurnState,
        harness_emitter: Option<&'a HarnessEventEmitter>,
        auto_permission: Option<AutoPermissionRuntimeContext<'a>>,
        skip_confirmations: bool,
        full_auto: bool,
    ) -> Self {
        Self {
            renderer,
            handle,
            tool_registry,
            tool_result_cache,
            tool_permission_cache,
            permissions_state,
            decision_ledger,
            session_stats,
            plan_session,
            mcp_panel_state,
            approval_recorder,
            session,
            safety_validator,
            traj,
            harness_state,
            harness_emitter,
            auto_permission,
            skip_confirmations,
            full_auto,
            active_agent_permissions: None,
            agent_name: None,
            default_primary_agent: None,
            is_subagent: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use hashbrown::HashSet;

    use super::{
        CrossTurnTracker, HarnessTurnState, MODEL_VISIBLE_TOOL_METADATA_BUDGET_BYTES, RecoveryMode,
        SESSION_LIMIT_AUTO_GRANT_INCREMENT, TOOL_BUDGET_WARNING_THRESHOLD, TOOL_PREVIEW_METADATA_PARSE_LIMIT_BYTES,
        ToolBudgetExhaustion, ToolBudgetExhaustionNotice, ToolBudgetWarning, ToolWallClockExhaustion,
        ToolWallClockExhaustionNotice, TurnExecutionPhase, TurnId, TurnPhase, TurnRunId, full_auto_loop_grants_enabled,
    };
    use vtcode_config::constants::output_limits::{
        TINY_PREVIEW_BYPASS_BYTES, TURN_PREVIEW_BUDGET_BYTES, TURN_PREVIEW_BUDGET_BYTES_PLANNING,
        TURN_TINY_PREVIEW_BUDGET_BYTES,
    };
    use vtcode_core::config::loader::VTCodeConfig;

    #[test]
    fn model_visible_tool_preview_budget_returns_bounded_metadata_after_exhaustion() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);
        let first = state.bound_model_visible_tool_preview(Some("exec_command"), "a".repeat(20 * 1024));
        assert_eq!(first.len(), 20 * 1024);

        let second = state.bound_model_visible_tool_preview(
            Some("run_pty_cmd"),
            serde_json::json!({
                "output": "b".repeat(20 * 1024),
                "spool_path": ".vtcode/context/tool_outputs/run-1.txt",
                "spooled_bytes": 20 * 1024,
                "spool_complete": true,
                "exit_code": 1,
                "status": "completed",
                "success": false,
                "error": {
                    "message": "permission denied: token=secret-not-for-context",
                    "original_error": "permission denied: token=secret-not-for-context",
                    "retryable": false,
                },
                "error_summary": "permission denied: token=secret-not-for-context",
                "diagnosis": {
                    "observed": "exit 1",
                    "likely_cause": "dependency check failed",
                    "next_action": "\u{1b}[31minspect the first compiler error\u{1b}[0m\npassword=secret-not-for-context"
                },
            })
            .to_string(),
        );

        assert!(second.len() < 2 * 1024);
        assert!(second.contains(".vtcode/context/tool_outputs/run-1.txt"));
        assert!(second.contains("\"byte_count\":20480"));
        assert!(second.contains("\"completion_state\":\"complete\""));
        assert!(second.contains("\"exit_code\":1"));
        assert!(second.contains("\"status\":\"completed\""));
        assert!(second.contains("\"success\":false"));
        assert!(second.contains("\"error_summary\":\"permission denied"));
        assert!(second.contains("\"message\":\"permission denied"));
        assert!(second.contains("preview_budget_exhausted"));
        assert!(second.contains("\"diagnosis\":{"));
        assert!(second.contains("inspect the first compiler error"));
        assert!(!second.contains("secret-not-for-context"));
        assert!(!second.contains('\u{1b}'));
        let repeated_b = "b".repeat(128);
        assert!(!second.contains(repeated_b.as_str()));
        assert_eq!(state.model_visible_tool_preview_bytes, TURN_PREVIEW_BUDGET_BYTES);
        assert!(state.model_visible_tool_preview_budget_exhausted);
        assert_eq!(state.suppressed_tool_previews, 1);

        // Verifier-sized payloads bypass the budget so small checks stay
        // visible after exhaustion (session-vtcode-20260913T074747Z).
        let tiny = state.bound_model_visible_tool_preview(
            Some("exec_command"),
            serde_json::json!({"exit_code": 0, "status": "completed", "success": true, "output": "later output"})
                .to_string(),
        );
        assert!(!tiny.contains("preview_budget_exhausted"));
        assert!(tiny.contains("later output"));
        assert_eq!(state.suppressed_tool_previews, 1);

        let later = state.bound_model_visible_tool_preview(
            Some("exec_command"),
            serde_json::json!({
                "exit_code": 0,
                "status": "completed",
                "success": true,
                "session_id": "run-later",
                "command": "grep -c pattern README.md",
                "backend": "pipe",
                "output": format!("later output {}", "x".repeat(5000)),
            })
            .to_string(),
        );
        assert!(later.contains("Aggregate tool preview budget exhausted"));
        assert!(!later.contains("later output"));
        assert!(later.contains("\"exit_code\":0"));
        assert!(later.contains("\"status\":\"completed\""));
        assert!(later.contains("\"success\":true"));
        assert!(later.contains("run-later"));
        assert!(later.contains("grep -c pattern README.md"));
        assert!(later.contains("\"backend\":\"pipe\""));
        assert_eq!(state.suppressed_tool_previews, 2);

        let mut aggregate_metadata_bytes = second.len() + later.len();
        for _ in 0..100 {
            aggregate_metadata_bytes += state
                .bound_model_visible_tool_preview(
                    Some("exec_command"),
                    serde_json::json!({
                        "status": "completed",
                        "success": true,
                        "diagnosis": {"next_action": "x".repeat(2048)},
                        "output": "hidden"
                    })
                    .to_string(),
                )
                .len();
        }
        assert!(
            aggregate_metadata_bytes < 100_000,
            "aggregate metadata bytes grew unboundedly: {aggregate_metadata_bytes}"
        );
        assert_eq!(state.model_visible_tool_metadata_bytes, MODEL_VISIBLE_TOOL_METADATA_BUDGET_BYTES);
    }

    #[test]
    fn tiny_verifier_previews_use_a_bounded_reserve_after_regular_budget_exhaustion() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);
        let primary =
            state.bound_model_visible_tool_preview(Some("read_file"), "a".repeat(TURN_PREVIEW_BUDGET_BYTES + 1));
        assert!(primary.contains("preview_budget_exhausted"));

        let output = "3";
        let tiny = serde_json::json!({"success": true, "exit_code": 0, "output": output}).to_string();
        assert!(tiny.len() <= TINY_PREVIEW_BYPASS_BYTES);
        let admitted_count = TURN_TINY_PREVIEW_BUDGET_BYTES / tiny.len();
        assert!(admitted_count > 0);
        for index in 0..admitted_count {
            let visible = state.bound_model_visible_tool_preview_for_call_with_budget(
                &format!("call-verifier-{index}"),
                Some("exec_command"),
                tiny.clone(),
                TURN_PREVIEW_BUDGET_BYTES,
            );
            assert_eq!(visible, tiny);
        }

        let suppressed = state.bound_model_visible_tool_preview_for_call_with_budget(
            "call-verifier-overflow",
            Some("exec_command"),
            tiny,
            TURN_PREVIEW_BUDGET_BYTES,
        );
        assert!(suppressed.contains("preview_budget_exhausted"));
        assert!(!suppressed.contains("\"output\":\"3\""));
        assert!(suppressed.contains("\"success\":true"));
        assert!(suppressed.contains("\"exit_code\":0"));
        assert_eq!(state.model_visible_tiny_tool_preview_bytes, TURN_TINY_PREVIEW_BUDGET_BYTES);
        assert_eq!(state.suppressed_tool_previews, 2);
    }

    #[test]
    fn preview_budget_exhausted_getter_tracks_bound_flip() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);
        assert!(!state.model_visible_preview_budget_exhausted());
        state.bound_model_visible_tool_preview(Some("exec_command"), "a".repeat(TURN_PREVIEW_BUDGET_BYTES + 1));
        assert!(state.model_visible_preview_budget_exhausted());
    }

    #[test]
    fn upstream_preview_exhaustion_latches_before_body_checks_and_is_idempotent() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 32, 10, 1);
        let registry_stub = serde_json::json!({
            "content_type": "git_diff",
            "exit_code": 0,
            "total_output_bytes": 5_212,
            "preview_budget_exhausted": true,
        })
        .to_string();

        assert!(state.observe_upstream_preview_budget_exhaustion(
            "call-diff",
            &registry_stub,
            TURN_PREVIEW_BUDGET_BYTES,
        ));
        assert!(state.model_visible_preview_budget_exhausted());
        assert_eq!(state.suppressed_tool_previews, 1);

        // A terminal update for the same call must not inflate diagnostics.
        assert!(state.observe_upstream_preview_budget_exhaustion(
            "call-diff",
            &registry_stub,
            TURN_PREVIEW_BUDGET_BYTES,
        ));
        assert_eq!(state.suppressed_tool_previews, 1);

        // A distinct suppressed result is counted independently.
        assert!(state.observe_upstream_preview_budget_exhaustion(
            "call-diff-2",
            &registry_stub,
            TURN_PREVIEW_BUDGET_BYTES,
        ));
        let diagnostics = state.snapshot_turn_diagnostics(Default::default(), 0);
        assert!(diagnostics.model_visible_tool_preview_budget_exhausted);
        assert_eq!(diagnostics.suppressed_tool_previews, 2);
    }

    #[test]
    fn replacement_suppression_is_idempotent_across_both_budget_layers() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 32, 10, 1);
        let locally_suppressed = state.bound_model_visible_tool_preview_for_call_with_budget(
            "call-read",
            Some("read_file"),
            "x".repeat(TURN_PREVIEW_BUDGET_BYTES + 1),
            TURN_PREVIEW_BUDGET_BYTES,
        );
        assert!(locally_suppressed.contains("preview_budget_exhausted"));
        assert_eq!(state.suppressed_tool_previews, 1);

        let registry_stub = serde_json::json!({
            "total_output_bytes": 80_000,
            "preview_budget_exhausted": true,
        })
        .to_string();
        let preserved = state.bound_model_visible_tool_preview_for_call_with_budget(
            "call-read",
            Some("read_file"),
            registry_stub.clone(),
            TURN_PREVIEW_BUDGET_BYTES,
        );
        assert_eq!(preserved, registry_stub);
        assert_eq!(state.suppressed_tool_previews, 1);

        state.bound_model_visible_tool_preview_for_call_with_budget(
            "call-read-2",
            Some("read_file"),
            "y".repeat(TURN_PREVIEW_BUDGET_BYTES + 1),
            TURN_PREVIEW_BUDGET_BYTES,
        );
        assert_eq!(state.suppressed_tool_previews, 2);
    }

    #[test]
    fn planning_preview_budget_keeps_midsize_payload_exec_strips_it() {
        let payload = "a".repeat(60 * 1024);
        assert!(payload.len() > TURN_PREVIEW_BUDGET_BYTES);
        assert!(payload.len() < TURN_PREVIEW_BUDGET_BYTES_PLANNING);

        let mut exec_state =
            HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);
        let exec_result = exec_state.bound_model_visible_tool_preview_with_budget(
            Some("exec_command"),
            payload.clone(),
            TURN_PREVIEW_BUDGET_BYTES,
        );
        assert!(exec_result.contains("preview_budget_exhausted"));
        assert!(exec_state.model_visible_preview_budget_exhausted());

        let mut plan_state =
            HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);
        let plan_result = plan_state.bound_model_visible_tool_preview_with_budget(
            Some("exec_command"),
            payload.clone(),
            TURN_PREVIEW_BUDGET_BYTES_PLANNING,
        );
        assert_eq!(plan_result, payload);
        assert!(!plan_state.model_visible_preview_budget_exhausted());
    }

    #[test]
    fn budget_exhausted_record_carries_exact_ceiling_values() {
        use super::{BudgetExhaustedMetrics, budget_exhausted_record, budget_kind};

        let value = serde_json::to_value(budget_exhausted_record(BudgetExhaustedMetrics {
            budget: budget_kind::TOOL_CALLS,
            used: 32,
            max: 32,
            step_count: None,
            planning_active: false,
            tool_calls: 32,
        }))
        .unwrap();
        assert_eq!(value["kind"], "budget_exhausted");
        assert_eq!(value["budget"], "tool_calls");
        assert_eq!(value["used"], 32);
        assert_eq!(value["max"], 32);
        assert!(value.get("step_count").is_none());
        assert_eq!(value["planning_active"], false);
        assert_eq!(value["tool_calls"], 32);
        assert!(value["ts"].is_number());

        // Asymmetric counterpart: a planning tool-loop record differs in
        // every mode-dependent field, so consumers can tell ceilings apart.
        let planned = serde_json::to_value(budget_exhausted_record(BudgetExhaustedMetrics {
            budget: budget_kind::TOOL_LOOP,
            used: 20,
            max: 20,
            step_count: Some(20),
            planning_active: true,
            tool_calls: 41,
        }))
        .unwrap();
        assert_eq!(planned["budget"], "tool_loop");
        assert_eq!(planned["step_count"], 20);
        assert_eq!(planned["planning_active"], true);
        assert_ne!(planned["budget"], value["budget"]);
    }

    #[test]
    fn spool_page_credit_keeps_pages_visible_after_exhaustion() {
        use super::SPOOL_PAGE_PREVIEW_CREDIT_BYTES;

        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);
        state.bound_model_visible_tool_preview(Some("exec_command"), "a".repeat(TURN_PREVIEW_BUDGET_BYTES + 1));
        assert!(state.model_visible_preview_budget_exhausted());

        // Without credit a page over the verifier bypass is stubbed.
        // (Pages at or under `TINY_PREVIEW_BYPASS_BYTES` bypass on their own,
        // so use 5 KiB here to exercise the exhaustion path.)
        let page = "p".repeat(5 * 1024);
        let stubbed = state.bound_model_visible_tool_preview(Some("read_file"), page.clone());
        assert!(stubbed.contains("preview_budget_exhausted"));
        assert!(!stubbed.contains(&page));

        // A granted page passes through fully visible without touching the
        // aggregate budget counters.
        state.grant_spool_page_preview_credit(SPOOL_PAGE_PREVIEW_CREDIT_BYTES);
        let preview_bytes_before = state.model_visible_tool_preview_bytes;
        let visible = state.bound_model_visible_tool_preview(Some("read_file"), page.clone());
        assert_eq!(visible, page);
        assert_eq!(state.model_visible_tool_preview_bytes, preview_bytes_before);
        assert_eq!(state.suppressed_tool_previews, 2);

        // Oversized pages are not partially exempted: they take the normal
        // budget path instead of creating partial-visibility states.
        let huge = "h".repeat(SPOOL_PAGE_PREVIEW_CREDIT_BYTES + 1);
        let huge_result = state.bound_model_visible_tool_preview(Some("read_file"), huge);
        assert!(huge_result.contains("preview_budget_exhausted"));
    }

    #[test]
    fn oversized_suppressed_preview_skips_unbounded_json_parse() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);
        state.bound_model_visible_tool_preview(Some("exec_command"), "a".repeat(TURN_PREVIEW_BUDGET_BYTES));

        let content = format!(
            "{{\"error_summary\":\"should-not-be-parsed\",\"output\":\"{}\"}}",
            "b".repeat(TOOL_PREVIEW_METADATA_PARSE_LIMIT_BYTES)
        );
        let metadata = state.bound_model_visible_tool_preview(Some("exec_command"), content);

        assert!(metadata.contains("\"preview_budget_exhausted\":true"));
        assert!(!metadata.contains("should-not-be-parsed"));
        assert!(metadata.contains("\"byte_count\":"));
    }

    #[test]
    fn harness_state_tracks_phase_transitions() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);

        // Verify that run_id and turn_id are accessible
        assert_eq!(state.run_id.0, "run-1");
        assert_eq!(state.turn_id.0, "turn-1");

        assert_eq!(state.phase, TurnPhase::Preparing);
        state.set_phase(TurnPhase::Requesting);
        assert_eq!(state.phase, TurnPhase::Requesting);
        state.set_phase(TurnPhase::ExecutingTools);
        assert_eq!(state.phase, TurnPhase::ExecutingTools);
        state.set_phase(TurnPhase::Finalizing);
        assert_eq!(state.phase, TurnPhase::Finalizing);
    }

    #[test]
    fn harness_state_accounts_for_out_of_band_tool_calls() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);

        state.record_out_of_band_tool_call();

        let diagnostics = state.snapshot_turn_diagnostics(Default::default(), 0);
        assert_eq!(diagnostics.requested_tool_calls, 1);
        assert_eq!(diagnostics.admitted_tool_calls, 1);
        assert_eq!(diagnostics.unadmitted_tool_calls, 0);
        assert!(state.has_out_of_band_tool_progress());
    }

    #[test]
    fn harness_state_tracks_spool_chunk_read_streak() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);

        assert_eq!(state.record_spool_chunk_read(), 1);
        assert_eq!(state.record_spool_chunk_read(), 2);
        state.reset_spool_chunk_read_streak();
        assert_eq!(state.record_spool_chunk_read(), 1);
    }

    #[test]
    fn harness_state_tracks_budget_warning_threshold_once() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert!(!state.should_emit_tool_budget_warning(TOOL_BUDGET_WARNING_THRESHOLD));
        state.record_tool_call(); // 1/4
        assert!(!state.should_emit_tool_budget_warning(TOOL_BUDGET_WARNING_THRESHOLD));
        state.record_tool_call(); // 2/4
        assert!(!state.should_emit_tool_budget_warning(TOOL_BUDGET_WARNING_THRESHOLD));
        state.record_tool_call(); // 3/4 => 75%
        assert!(state.should_emit_tool_budget_warning(TOOL_BUDGET_WARNING_THRESHOLD));
        state.mark_tool_budget_warning_emitted();
        assert!(!state.should_emit_tool_budget_warning(TOOL_BUDGET_WARNING_THRESHOLD));
        assert_eq!(state.remaining_tool_calls(), 1);
    }

    #[test]
    fn harness_state_records_budget_warning_once_via_helper() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert_eq!(state.record_tool_call_with_default_warning(), None);
        assert_eq!(state.record_tool_call_with_default_warning(), None);
        assert_eq!(
            state.record_tool_call_with_default_warning(),
            Some(ToolBudgetWarning { used: 3, max: 4, remaining: 1 })
        );
        assert_eq!(state.record_tool_call_with_default_warning(), None);
    }

    #[test]
    fn tool_budget_warning_system_message_matches_contract() {
        assert_eq!(
            ToolBudgetWarning { used: 3, max: 4, remaining: 1 }.system_message(),
            "Tool-call budget warning: 3/4 used; 1 remaining for this turn. Use targeted extraction/batching before additional tool calls."
        );
    }

    #[test]
    fn harness_state_records_budget_exhaustion_notice_once_via_helper() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 1, 10, 1);

        assert!(!state.tool_budget_exhausted());
        assert!(!state.tool_budget_exhausted_emitted);
        state.record_tool_call();
        assert!(state.tool_budget_exhausted());
        assert_eq!(
            state.record_tool_budget_exhaustion_notice(),
            Some(ToolBudgetExhaustionNotice {
                exhaustion: ToolBudgetExhaustion { used: 1, max: 1, remaining: 0 },
                first_notice: true,
            })
        );
        assert!(state.tool_budget_exhausted_emitted);
        assert_eq!(
            state.record_tool_budget_exhaustion_notice(),
            Some(ToolBudgetExhaustionNotice {
                exhaustion: ToolBudgetExhaustion { used: 1, max: 1, remaining: 0 },
                first_notice: false,
            })
        );
    }

    #[test]
    fn tool_budget_exhaustion_synthesis_directive_matches_contract() {
        assert_eq!(
            ToolBudgetExhaustion { used: 4, max: 4, remaining: 0 }.synthesis_directive_message(),
            "Tool-call budget exhausted for this turn (4/4). Tools are disabled for the rest of this turn, so further tool calls are skipped. Synthesize your final answer now from the tool outputs already gathered in this conversation."
        );
    }

    #[test]
    fn tool_budget_exhaustion_notice_arms_synthesis_directive_once() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 1, 600, 3);
        state.record_tool_call();
        assert!(state.record_tool_budget_exhaustion_notice().is_some());
        assert!(state.take_tool_budget_directive_pending());
        // Second rejected call in the same turn must not re-arm the directive.
        assert!(state.record_tool_budget_exhaustion_notice().is_some());
        assert!(!state.take_tool_budget_directive_pending());
    }

    #[test]
    fn tool_budget_rejection_is_separate_from_permission_denial() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 1, 10, 1);

        state.record_tool_budget_rejection();
        assert!(state.take_tool_budget_rejection());
        assert!(!state.take_tool_budget_rejection());
        assert_eq!(state.snapshot_turn_diagnostics(Default::default(), 0).denied_tool_calls, 0);

        state.record_denied_tool_call();
        assert_eq!(state.snapshot_turn_diagnostics(Default::default(), 0).denied_tool_calls, 1);
    }

    #[test]
    fn auto_permission_probe_warning_is_queued_once_until_flushed() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert!(state.queue_auto_permission_probe_warning("trusted warning".to_string()));
        assert!(!state.queue_auto_permission_probe_warning("duplicate warning".to_string()));
        assert_eq!(state.take_auto_permission_probe_warning().as_deref(), Some("trusted warning"));
        assert!(state.take_auto_permission_probe_warning().is_none());
    }

    #[test]
    fn tool_wall_clock_exhaustion_policy_violation_message_matches_contract() {
        assert_eq!(
            ToolWallClockExhaustion { max_secs: 600 }.policy_violation_message(),
            "Policy violation: exceeded tool wall clock budget (600s)"
        );
    }

    #[test]
    fn harness_state_treats_zero_tool_budget_as_unlimited() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 0, 10, 1);

        for _ in 0..8 {
            state.record_tool_call();
        }

        assert!(!state.has_tool_call_budget());
        assert!(!state.tool_budget_exhausted());
        assert_eq!(state.tool_budget_exhaustion(), None);
        assert!(!state.should_emit_tool_budget_warning(TOOL_BUDGET_WARNING_THRESHOLD));
    }

    #[test]
    fn harness_state_reports_wall_clock_budget_exhaustion() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert_eq!(state.wall_clock_budget_exhaustion(), None);
        state.turn_started_at = Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        assert_eq!(state.wall_clock_budget_exhaustion(), Some(ToolWallClockExhaustion { max_secs: 10 }));
    }

    #[test]
    fn harness_state_excludes_active_external_wait_from_wall_clock_budget() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);
        let wait_started_at = Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        state.turn_started_at = wait_started_at;
        state.wait_started_at = Some(wait_started_at);

        assert_eq!(state.wall_clock_budget_exhaustion(), None);

        state.end_budget_excluded_wait();
        assert_eq!(state.wall_clock_budget_exhaustion(), None);
    }

    #[test]
    fn harness_state_records_wall_clock_exhaustion_notice_once() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        // Not exhausted yet: no notice, no pending directive.
        assert_eq!(state.record_wall_clock_exhaustion_notice(), None);
        assert!(!state.take_wall_clock_directive_pending());

        // Simulate the wall-clock budget elapsing.
        state.turn_started_at = Instant::now().checked_sub(Duration::from_secs(11)).unwrap();

        // First rejection: first_notice=true and arms the directive.
        assert_eq!(
            state.record_wall_clock_exhaustion_notice(),
            Some(ToolWallClockExhaustionNotice {
                exhaustion: ToolWallClockExhaustion { max_secs: 10 },
                first_notice: true,
            })
        );
        assert!(state.wall_clock_exhausted_emitted);

        // Subsequent rejections in the same batch: first_notice=false.
        assert_eq!(
            state.record_wall_clock_exhaustion_notice(),
            Some(ToolWallClockExhaustionNotice {
                exhaustion: ToolWallClockExhaustion { max_secs: 10 },
                first_notice: false,
            })
        );

        // The directive is consumed exactly once.
        assert!(state.take_wall_clock_directive_pending());
        assert!(!state.take_wall_clock_directive_pending());
    }

    #[test]
    fn tool_wall_clock_exhaustion_directive_messages_match_contract() {
        let exhaustion = ToolWallClockExhaustion { max_secs: 600 };
        assert_eq!(exhaustion.skipped_call_message(), "Tool wall-clock budget exhausted for this turn; call skipped.");
        assert_eq!(
            exhaustion.synthesis_directive_message(),
            "Tool wall-clock budget exhausted for this turn (600s). Tools are disabled for the rest of this turn, so further tool calls are skipped. Synthesize your final answer now from the tool outputs already gathered in this conversation."
        );
    }

    #[test]
    fn harness_state_tracks_blocked_call_streak() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert_eq!(state.blocked_tool_calls, 0);
        assert_eq!(state.record_blocked_tool_call(), 1);
        assert_eq!(state.record_blocked_tool_call(), 2);
        assert_eq!(state.blocked_tool_calls, 2);
        state.reset_blocked_tool_call_streak();
        assert_eq!(state.consecutive_blocked_tool_calls, 0);
    }

    #[test]
    fn harness_state_tracks_and_resets_preflight_failure_streak() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert_eq!(state.record_preflight_failure(), 1);
        assert_eq!(state.record_preflight_failure(), 2);
        state.reset_preflight_failure_streak();
        assert_eq!(state.consecutive_preflight_failures, 0);
    }

    #[test]
    fn harness_state_tracks_recovery_state() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert!(!state.is_recovery_active());
        assert!(!state.recovery_pass_used());

        state.activate_recovery("loop detector");
        assert!(state.is_recovery_active());
        assert_eq!(state.recovery_reason(), Some("loop detector"));
        assert_eq!(state.recovery_mode(), Some(RecoveryMode::ToolFreeSynthesis));
        assert!(state.recovery_is_tool_free());

        assert!(state.consume_recovery_pass());
        assert!(state.recovery_pass_used());
        assert!(state.finish_recovery_pass());
        assert!(!state.is_recovery_active());
    }

    #[test]
    fn harness_state_consumes_recovery_pass_once() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert!(!state.consume_recovery_pass());

        state.activate_recovery("loop detector");
        assert!(state.consume_recovery_pass());
        assert!(!state.consume_recovery_pass());
        assert!(state.recovery_pass_used());
        assert!(state.finish_recovery_pass());
        assert!(!state.finish_recovery_pass());
    }

    #[test]
    fn harness_state_supports_tool_enabled_recovery_retries() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        state.activate_recovery_with_mode("empty response", RecoveryMode::ToolEnabledRetry);

        assert!(state.is_recovery_active());
        assert_eq!(state.recovery_mode(), Some(RecoveryMode::ToolEnabledRetry));
        assert!(!state.recovery_is_tool_free());
        assert!(state.consume_recovery_pass());
        assert!(state.finish_recovery_pass());
    }

    #[test]
    fn harness_state_arms_one_post_tool_compaction_retry() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert!(state.arm_post_tool_tool_enabled_retry("transient post-tool failure", false));
        assert!(state.is_recovery_active());
        assert_eq!(state.recovery_mode(), Some(RecoveryMode::ToolEnabledRetry));
        assert!(!state.recovery_is_tool_free());
        assert!(state.post_tool_compaction_pending());
        assert!(state.post_tool_tool_enabled_retry_used());
        assert!(!state.post_tool_context_capacity_failure());
        assert!(state.take_post_tool_compaction_pending());
        assert!(!state.post_tool_compaction_pending());
        assert!(!state.arm_post_tool_tool_enabled_retry("must remain bounded", false));
    }

    #[test]
    fn harness_state_marks_context_capacity_recovery() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert!(state.arm_post_tool_tool_enabled_retry("context limit", true));
        assert!(state.post_tool_context_capacity_failure());
        assert!(!state.post_tool_context_compaction_failed());

        state.mark_post_tool_context_compaction_failed();
        assert!(state.post_tool_context_compaction_failed());
    }

    #[test]
    fn harness_state_switch_to_tool_free_recovery_from_inactive() {
        // Regression guard for the post-tool follow-up infinite loop:
        // `switch_to_tool_free_recovery` must transition `Inactive -> Pending`
        // (not just `InPass` or `Completed` to `Pending`). When a normal
        // (non-recovery)
        // turn's follow-up LLM phase fails, the phase is `Inactive`; if the
        // switch left it there, `consume_recovery_pass()` would return false,
        // `tool_free_recovery` would evaluate to false, and tools would never
        // be disabled at the API level.
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        // Fresh state: recovery is inactive.
        assert!(!state.is_recovery_active());
        assert!(!state.recovery_is_tool_free());

        // Switching from Inactive must engage a tool-free recovery pass.
        assert!(state.switch_to_tool_free_recovery(), "switch from Inactive must report a phase change");
        assert!(state.is_recovery_active(), "phase must be Pending");
        assert_eq!(state.recovery_mode(), Some(RecoveryMode::ToolFreeSynthesis));
        assert!(state.recovery_is_tool_free());

        // The pass must be consumable. This is what the turn loop checks to
        // decide `tool_free_recovery = true` and disable tools at the API level.
        assert!(state.consume_recovery_pass(), "consume_recovery_pass must succeed after switch from Inactive");

        // A default recovery reason must be seeded so the [Recovery Mode]
        // request block reports why recovery was engaged.
        assert!(state.recovery_reason().is_some(), "recovery_reason must be seeded when switching from Inactive");
    }

    #[test]
    fn harness_state_switch_to_tool_free_recovery_from_in_pass_keeps_consumable() {
        // Switching from InPass (a pass already in flight) must still reset to
        // Pending so the next loop iteration can consume it.
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        state.activate_recovery_with_mode("empty response", RecoveryMode::ToolEnabledRetry);
        assert!(state.consume_recovery_pass()); // -> InPass
        assert_eq!(state.recovery_mode(), Some(RecoveryMode::ToolEnabledRetry));

        assert!(state.switch_to_tool_free_recovery());
        assert_eq!(state.recovery_mode(), Some(RecoveryMode::ToolFreeSynthesis));
        assert!(state.consume_recovery_pass(), "pass must be consumable again after switching from InPass");
    }

    #[test]
    fn harness_state_switch_to_tool_free_recovery_from_completed_keeps_consumable() {
        // No-regression guard: switching from Completed must reset to Pending.
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        state.activate_recovery_with_mode("empty response", RecoveryMode::ToolEnabledRetry);
        assert!(state.consume_recovery_pass()); // -> InPass
        assert!(state.finish_recovery_pass()); // -> Completed
        assert!(!state.is_recovery_active());

        assert!(state.switch_to_tool_free_recovery());
        assert!(state.is_recovery_active());
        assert!(state.consume_recovery_pass());
    }

    #[test]
    fn harness_state_switch_to_tool_free_recovery_idempotent_when_pending() {
        // When already Pending, switching reports no phase change but still
        // forces the mode to ToolFreeSynthesis.
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        state.activate_recovery("loop detector");
        assert!(state.is_recovery_active()); // Pending

        assert!(!state.switch_to_tool_free_recovery(), "switch from Pending must report no phase change");
        assert_eq!(state.recovery_mode(), Some(RecoveryMode::ToolFreeSynthesis));
        assert!(state.consume_recovery_pass());
    }

    #[test]
    fn harness_state_switch_to_tool_free_recovery_resets_retry_count_from_inactive() {
        // Switching from Inactive resets the retry counter (mirrors
        // activate_recovery_with_mode) so any stale count does not
        // prematurely exhaust the in-pass retry budget on the new pass.
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        // Fresh state: retry_count is 0, phase is Inactive.
        assert_eq!(state.recovery_retry_count(), 0);

        // Switch from Inactive to Pending: retry count stays 0.
        state.switch_to_tool_free_recovery();
        assert_eq!(state.recovery_retry_count(), 0);

        // Complete this pass and start a second cycle.
        assert!(state.consume_recovery_pass());
        assert!(state.retry_recovery_pass()); // retry_count becomes 1
        assert_eq!(state.recovery_retry_count(), 1);

        // Switch from Completed to Pending: retry count is not reset
        // (only Inactive triggers the reset).
        assert!(state.consume_recovery_pass());
        assert!(state.finish_recovery_pass());
        state.switch_to_tool_free_recovery();
        assert_eq!(state.recovery_retry_count(), 1, "retry count must NOT be reset when switching from Completed");

        // Consume and retry: the budget should tick up to 2, not start over.
        assert!(state.consume_recovery_pass());
        assert!(state.retry_recovery_pass());
        assert_eq!(state.recovery_retry_count(), 2);
    }

    #[test]
    fn harness_state_tracks_post_tool_recovery_cycles() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert_eq!(state.post_tool_recovery_cycles(), 0);
        assert_eq!(state.increment_post_tool_recovery_cycle(), 1);
        assert_eq!(state.post_tool_recovery_cycles(), 1);
        assert_eq!(state.increment_post_tool_recovery_cycle(), 2);
        assert_eq!(state.post_tool_recovery_cycles(), 2);
        assert_eq!(state.increment_post_tool_recovery_cycle(), 3);
        assert_eq!(state.post_tool_recovery_cycles(), 3);
    }

    #[test]
    fn harness_state_tracks_task_tracker_create_signatures() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert!(state.record_task_tracker_create_signature(
            "task_tracker::create::{\"title\":\"A\",\"items\":[\"x\"]}".to_string()
        ));
        assert!(!state.record_task_tracker_create_signature(
            "task_tracker::create::{\"title\":\"A\",\"items\":[\"x\"]}".to_string()
        ));
        assert!(state.record_task_tracker_create_signature(
            "task_tracker::create::{\"title\":\"A\",\"items\":[\"y\"]}".to_string()
        ));
    }

    #[test]
    fn harness_state_tracks_successful_readonly_signatures() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert!(!state.has_successful_readonly_signature("file_operation:ro:len10-fnv1234"));
        assert!(state.record_successful_readonly_signature("file_operation:ro:len10-fnv1234".to_string()));
        assert!(state.has_successful_readonly_signature("file_operation:ro:len10-fnv1234"));
        assert!(!state.record_successful_readonly_signature("file_operation:ro:len10-fnv1234".to_string()));
    }

    #[test]
    fn harness_state_tracks_identical_shell_command_streak() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert_eq!(state.record_shell_command_run("exec_command::cargo check".to_string()), 1);
        assert_eq!(state.record_shell_command_run("exec_command::cargo check".to_string()), 2);
        assert_eq!(state.record_shell_command_run("exec_command::cargo test".to_string()), 1);
        assert_eq!(state.last_shell_command_signature.as_deref(), Some("exec_command::cargo test"));
    }

    #[test]
    fn harness_state_resets_shell_command_streak() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        state.record_shell_command_run("exec_command::cargo check".to_string());
        state.reset_shell_command_run_streak();
        assert_eq!(state.consecutive_same_shell_command_runs, 0);
        assert!(state.last_shell_command_signature.is_none());
    }

    #[test]
    fn harness_state_tracks_file_read_family_streak() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 4, 10, 1);

        assert_eq!(state.record_file_read_family_call("apply_patch::read::src/lib.rs".to_string()), 1);
        assert_eq!(state.record_file_read_family_call("apply_patch::read::src/lib.rs".to_string()), 2);
        assert_eq!(state.record_file_read_family_call("apply_patch::read::src/main.rs".to_string()), 1);

        state.reset_file_read_family_streak();
        assert_eq!(state.consecutive_same_file_read_family_calls, 0);
    }

    #[test]
    fn file_read_path_counts_track_per_path_regardless_of_slice() {
        let mut state =
            HarnessTurnState::new(TurnRunId("run-path".to_string()), TurnId("turn-path".to_string()), 20, 600, 3);

        // Same path, different offsets — each increments the path counter.
        assert_eq!(state.record_file_read_path_call("src/lib.rs".to_string()), 1);
        assert_eq!(state.record_file_read_path_call("src/lib.rs".to_string()), 2);
        assert_eq!(state.record_file_read_path_call("src/lib.rs".to_string()), 3);
        // Different path gets its own counter.
        assert_eq!(state.record_file_read_path_call("src/main.rs".to_string()), 1);
        // Original path continues counting.
        assert_eq!(state.record_file_read_path_call("src/lib.rs".to_string()), 4);

        state.reset_file_read_path_counts();
        assert_eq!(state.record_file_read_path_call("src/lib.rs".to_string()), 1);
    }

    #[test]
    fn session_limit_grant_is_recorded_once_and_exposed_for_cleanup() {
        let mut state =
            HarnessTurnState::new(TurnRunId("run-limit".to_string()), TurnId("turn-limit".to_string()), 20, 600, 3);

        assert!(!state.has_session_limit_grant());
        assert!(!state.take_session_limit_grant_directive_pending());

        state.record_session_limit_grant();

        assert!(state.has_session_limit_grant());
        assert!(state.take_session_limit_grant_directive_pending());
        assert!(!state.take_session_limit_grant_directive_pending());
    }

    #[test]
    fn harness_state_builds_execution_snapshot() {
        let mut state = HarnessTurnState::new(TurnRunId("run-9".to_string()), TurnId("turn-3".to_string()), 6, 120, 2);
        state.set_phase(TurnPhase::ExecutingTools);

        let snapshot = state.execution_snapshot();
        assert_eq!(snapshot.run_id, "run-9");
        assert_eq!(snapshot.turn_id, "turn-3");
        assert_eq!(snapshot.phase, TurnExecutionPhase::ExecutingTools);
        assert_eq!(snapshot.max_tool_calls, 6);
        assert_eq!(snapshot.max_tool_wall_clock_secs, 120);
        assert_eq!(snapshot.max_tool_retries, 2);
    }

    #[test]
    fn turn_diagnostic_counters_saturate_and_count_recovery_once() {
        let mut state = HarnessTurnState::new(
            TurnRunId("run-diagnostics".to_string()),
            TurnId("turn-diagnostics".to_string()),
            4,
            120,
            2,
        );
        state.requested_tool_calls = u32::MAX - 1;
        state.record_requested_tool_calls(usize::MAX);
        state.raw_spooled_bytes = u64::MAX - 2;
        state.model_visible_output_bytes = u64::MAX - 2;
        state.record_tool_output_metrics(true, true, 10, usize::MAX);
        state.record_reused_result();
        state.activate_recovery("adaptive planning synthesis");
        state.activate_recovery("duplicate activation");

        let diagnostics = state.snapshot_turn_diagnostics(Default::default(), u32::MAX);
        assert_eq!(diagnostics.requested_tool_calls, u32::MAX);
        assert_eq!(diagnostics.unadmitted_tool_calls, u32::MAX);
        assert_eq!(diagnostics.reused_results, 2);
        assert_eq!(diagnostics.spooled_results, 1);
        assert_eq!(diagnostics.raw_spooled_bytes, u64::MAX);
        assert_eq!(diagnostics.model_visible_output_bytes, u64::MAX);
        assert_eq!(diagnostics.low_signal_tool_calls, u32::MAX);
        assert_eq!(diagnostics.recovery_activations, 1);
    }

    // --- CrossTurnTracker tests ---

    #[test]
    fn cross_turn_tracker_no_warning_on_first_turn() {
        let mut tracker = CrossTurnTracker::new();
        let read_sigs = vec!["apply_patch::read::src/main.rs".to_string()];
        let written = HashSet::new();
        assert!(tracker.seal_turn(&read_sigs, &written, None, false).is_none());
    }

    #[test]
    fn cross_turn_tracker_detects_repeated_turn() {
        let mut tracker = CrossTurnTracker::new();
        let read_sigs = vec!["apply_patch::read::src/main.rs".to_string()];
        let written = HashSet::new();

        // First turn: no warning
        assert!(tracker.seal_turn(&read_sigs, &written, None, false).is_none());

        // Second turn with same signatures: cross-turn loop detected
        let warning = tracker.seal_turn(&read_sigs, &written, None, false);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("Cross-turn loop detected"));
    }

    #[test]
    fn cross_turn_tracker_no_false_positive_different_turns() {
        let mut tracker = CrossTurnTracker::new();
        let read_sigs_1 = vec!["apply_patch::read::src/main.rs".to_string()];
        let read_sigs_2 = vec!["apply_patch::read::src/lib.rs".to_string()];
        let written = HashSet::new();

        assert!(tracker.seal_turn(&read_sigs_1, &written, None, false).is_none());
        assert!(tracker.seal_turn(&read_sigs_2, &written, None, false).is_none());
    }

    #[test]
    fn cross_turn_tracker_stuck_no_progress() {
        let mut tracker = CrossTurnTracker::new();
        let written = HashSet::new();

        // Use different signatures each turn to avoid cross-turn loop detection
        // and isolate the stuck (zero-mutation) detection.
        let sigs_1 = vec!["apply_patch::read::src/a.rs".to_string()];
        let sigs_2 = vec!["apply_patch::read::src/b.rs".to_string()];
        let sigs_3 = vec!["apply_patch::read::src/c.rs".to_string()];

        assert!(tracker.seal_turn(&sigs_1, &written, None, false).is_none());
        assert!(tracker.seal_turn(&sigs_2, &written, None, false).is_none());

        // Third consecutive read-only turn: stuck warning
        let warning = tracker.seal_turn(&sigs_3, &written, None, false);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("No progress detected"));
    }

    #[test]
    fn cross_turn_tracker_mutation_resets_stuck_counter() {
        let mut tracker = CrossTurnTracker::new();
        let empty_written = HashSet::new();

        // Two read-only turns with different signatures (avoid cross-turn loop)
        let sigs_a = vec!["apply_patch::read::src/a.rs".to_string()];
        let sigs_b = vec!["apply_patch::read::src/b.rs".to_string()];
        assert!(tracker.seal_turn(&sigs_a, &empty_written, None, false).is_none());
        assert!(tracker.seal_turn(&sigs_b, &empty_written, None, false).is_none());

        // A mutating turn resets the counter
        let mut written = HashSet::new();
        written.insert("src/main.rs".to_string());
        let sigs_c = vec!["apply_patch::read::src/c.rs".to_string()];
        assert!(tracker.seal_turn(&sigs_c, &written, None, false).is_none());

        // Two more read-only turns: no stuck warning (counter was reset)
        let sigs_d = vec!["apply_patch::read::src/d.rs".to_string()];
        let sigs_e = vec!["apply_patch::read::src/e.rs".to_string()];
        assert!(tracker.seal_turn(&sigs_d, &empty_written, None, false).is_none());
        assert!(tracker.seal_turn(&sigs_e, &empty_written, None, false).is_none());
    }

    #[test]
    fn cross_turn_tracker_command_execution_resets_stuck_counter() {
        let mut tracker = CrossTurnTracker::new();
        let written = HashSet::new();

        assert!(tracker.seal_turn(&["read::a".to_string()], &written, None, false).is_none());
        assert!(tracker.seal_turn(&["read::b".to_string()], &written, None, false).is_none());
        assert!(tracker.seal_turn(&[], &written, Some("exec::cargo-check"), false).is_none());
        assert_eq!(tracker.zero_mutation_turns(), 0);

        assert!(tracker.seal_turn(&["read::c".to_string()], &written, None, false).is_none());
        assert!(tracker.seal_turn(&["read::d".to_string()], &written, None, false).is_none());
    }

    #[test]
    fn cross_turn_tracker_out_of_band_progress_resets_stuck_counter() {
        let mut tracker = CrossTurnTracker::new();
        let written = HashSet::new();

        assert!(tracker.seal_turn(&["read::a".to_string()], &written, None, false).is_none());
        assert!(tracker.seal_turn(&["read::b".to_string()], &written, None, false).is_none());
        assert_eq!(tracker.zero_mutation_turns(), 2);

        assert!(
            tracker
                .seal_turn_with_progress(&[], &written, None, None, true, false)
                .is_none()
        );
        assert_eq!(tracker.zero_mutation_turns(), 0);

        assert!(tracker.seal_turn(&["read::c".to_string()], &written, None, false).is_none());
        assert_eq!(tracker.zero_mutation_turns(), 1);
    }

    #[test]
    fn harness_state_separates_guarded_and_admitted_shell_signatures() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);

        state.record_shell_command_run("exec_command::blocked".to_string());
        assert_eq!(state.last_admitted_shell_command_signature, None);

        state.record_admitted_shell_command("exec_command::admitted".to_string());
        assert_eq!(state.last_admitted_shell_command_signature.as_deref(), Some("exec_command::admitted"));
    }

    #[test]
    fn harness_state_records_failed_shell_key() {
        let mut state = HarnessTurnState::new(TurnRunId("run-1".to_string()), TurnId("turn-1".to_string()), 2, 10, 1);
        assert_eq!(state.last_failed_shell_key(), None);

        state
            .record_failed_shell_command("command_session::cargo check".to_string(), "error: bad manifest".to_string());
        assert_eq!(state.last_failed_shell_key(), Some("command_session::cargo check::err::error: bad manifest"));
    }

    #[test]
    fn cross_turn_tracker_empty_turn_no_warning() {
        let mut tracker = CrossTurnTracker::new();
        let empty_sigs: Vec<String> = Vec::new();
        let empty_written = HashSet::new();

        // Empty turns should not trigger warnings or corrupt state
        assert!(tracker.seal_turn(&empty_sigs, &empty_written, None, false).is_none());
        assert!(tracker.seal_turn(&empty_sigs, &empty_written, None, false).is_none());
    }

    #[test]
    fn cross_turn_tracker_identical_shell_failure_warns_on_third_turn() {
        let mut tracker = CrossTurnTracker::new();
        let written = HashSet::new();
        let key = Some("command_session::cargo check::err::error: failed to load manifest");

        // Vary the read sets so the fingerprint loop detector stays quiet;
        // the identical-failure streak must fire regardless.
        let sigs_a = vec!["code_search::alpha".to_string()];
        let sigs_b = vec!["code_search::beta".to_string()];
        let sigs_c = vec!["code_search::gamma".to_string()];
        assert!(
            tracker
                .seal_turn_with_progress(&sigs_a, &written, None, key, false, false)
                .is_none()
        );
        assert!(
            tracker
                .seal_turn_with_progress(&sigs_b, &written, None, key, false, false)
                .is_none()
        );
        let warning = tracker.seal_turn_with_progress(&sigs_c, &written, None, key, false, false);
        assert!(warning.is_some());
        let warning = warning.unwrap();
        assert!(warning.contains("Identical shell failure"), "warning names the pattern: {warning}");
        assert!(warning.contains("cargo check"), "warning names the command: {warning}");
    }

    #[test]
    fn cross_turn_tracker_failed_shell_streak_resets_on_change_or_success() {
        let mut tracker = CrossTurnTracker::new();
        let written = HashSet::new();
        let empty: Vec<String> = Vec::new();
        let key_a = Some("command_session::cargo check::err::error: bad manifest");
        let key_b = Some("command_session::cargo check::err::error: missing crate");

        assert!(
            tracker
                .seal_turn_with_progress(&empty, &written, None, key_a, false, false)
                .is_none()
        );
        assert!(
            tracker
                .seal_turn_with_progress(&empty, &written, None, key_a, false, false)
                .is_none()
        );
        // Changed error text restarts the streak: no warning on what would
        // otherwise be the third consecutive failure turn.
        assert!(
            tracker
                .seal_turn_with_progress(&empty, &written, None, key_b, false, false)
                .is_none()
        );
        // A clean turn clears the streak entirely.
        assert!(
            tracker
                .seal_turn_with_progress(&empty, &written, None, None, false, false)
                .is_none()
        );
        assert!(
            tracker
                .seal_turn_with_progress(&empty, &written, None, key_b, false, false)
                .is_none()
        );
        assert!(
            tracker
                .seal_turn_with_progress(&empty, &written, None, key_b, false, false)
                .is_none()
        );
        let warning = tracker.seal_turn_with_progress(&empty, &written, None, key_b, false, false);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("Identical shell failure"));
    }

    #[test]
    fn cross_turn_tracker_order_independent_fingerprint() {
        let mut tracker = CrossTurnTracker::new();
        let written = HashSet::new();

        // Same signatures in different order should produce same fingerprint
        let sigs_a = vec![
            "apply_patch::read::src/main.rs".to_string(),
            "code_search::grep::fn".to_string(),
        ];
        let sigs_b = vec![
            "code_search::grep::fn".to_string(),
            "apply_patch::read::src/main.rs".to_string(),
        ];

        assert!(tracker.seal_turn(&sigs_a, &written, None, false).is_none());
        let warning = tracker.seal_turn(&sigs_b, &written, None, false);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("Cross-turn loop detected"));
    }

    #[test]
    fn full_auto_loop_grants_require_runtime_and_config_and_opt_in() {
        assert_eq!(SESSION_LIMIT_AUTO_GRANT_INCREMENT, 100);

        // No config at all: interactive sessions keep prompting.
        assert!(!full_auto_loop_grants_enabled(true, None));
        assert!(!full_auto_loop_grants_enabled(false, None));

        let mut cfg = VTCodeConfig::default();
        // Default config has full-auto disabled: no auto-grant either way.
        assert!(!full_auto_loop_grants_enabled(true, Some(&cfg)));
        assert!(!full_auto_loop_grants_enabled(false, Some(&cfg)));

        // Full-auto runtime + enabled config grants by default (opt-out flag on).
        cfg.automation.full_auto.enabled = true;
        assert!(full_auto_loop_grants_enabled(true, Some(&cfg)));
        // Ordinary runtime never grants, even with full-auto configured.
        assert!(!full_auto_loop_grants_enabled(false, Some(&cfg)));

        // Explicit opt-out restores prompting in full-auto runs.
        cfg.automation.full_auto.auto_grant_tool_limits = false;
        assert!(!full_auto_loop_grants_enabled(true, Some(&cfg)));
    }
}
