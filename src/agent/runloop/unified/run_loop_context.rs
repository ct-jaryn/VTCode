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
use vtcode_core::types::CompactStr;
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
const TOOL_PREVIEW_METADATA_MAX_DEPTH: usize = 8;

const TOOL_PREVIEW_METADATA_STRING_LIMIT: usize = 512;
/// Do not materialize an arbitrarily large suppressed payload merely to
/// recover optional metadata. Normal tool responses are compacted before this
/// point; oversized or malformed payloads keep only the generic byte count.
const TOOL_PREVIEW_METADATA_PARSE_LIMIT_BYTES: usize = 128 * 1024;

/// Shared per-turn cap for auxiliary model-backed checks (failure diagnosis,
/// prompt-injection probes). These run invisibly between tool results, so a
/// tool-heavy turn must not multiply them without bound.
const AUX_MODEL_CALL_BUDGET: u32 = 3;

/// Minimal probe for the exhaustion marker. Decoding only the single control
/// flag avoids materializing the full tool payload (`Value` IR) on every
/// response when all we need is one bool.
#[derive(serde::Deserialize)]
struct PreviewBudgetExhaustedProbe {
    #[serde(default)]
    preview_budget_exhausted: Option<bool>,
}

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
pub(crate) const BUDGET_EXHAUSTED_SYNTHESIS_NOTE: &str = "Tools are disabled for the rest of this turn, so further tool calls are \
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

/// Identity of a failed tool call for the turn-local diagnosis memo.
/// `evidence` can be large (bounded by the diagnosis evidence cap).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DiagnosisMemoKey {
    pub tool: CompactStr,
    pub evidence: String,
}

/// Bounded diagnosis fields stored in the turn-local memo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiagnosisMemoEntry {
    pub observed: CompactStr,
    pub likely_cause: CompactStr,
    pub next_action: CompactStr,
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
    model_visible_tool_preview_budget_exhausted: bool,
    suppressed_tool_previews: u32,
    /// Tool calls with a truncated preview or a legacy suppression marker.
    /// Shared identity tracking keeps diagnostics idempotent
    /// when an in-progress response is replaced by its terminal result.
    suppressed_tool_call_ids: HashSet<String>,
    recovery_activations: u32,
    pub blocked_tool_calls: usize,
    pub consecutive_blocked_tool_calls: usize,
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
    /// Batch validation reserves the one path-cap exception before execution.
    claimed_patch_recovery_paths: HashSet<std::path::PathBuf>,
    pub(crate) seen_successful_readonly_signatures: HashSet<String>,
    streamed_tool_call_item_ids: HashMap<String, StreamedToolCallItem>,
    /// Turn-local memo of failure diagnoses. Fix-verify loops re-hit the
    /// same failure shape; skip the extra lightweight LLM round-trip after
    /// the first diagnosis.
    failure_diagnosis_memo: HashMap<DiagnosisMemoKey, DiagnosisMemoEntry>,
    /// Cap on model-backed diagnosis calls per turn. After this, deterministic
    /// fallbacks only — failure-heavy loops must not multiply model calls.
    failure_diagnosis_model_calls: u32,
    /// Cap on model-backed prompt-injection probes per turn. Tool-heavy
    /// full-auto turns otherwise add one hidden LLM round-trip per result.
    auto_permission_probe_model_calls: u32,
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
    /// Reason frozen into the `[Recovery Mode]` prompt block for the current
    /// recovery activation. Updated only when a new activation starts so the
    /// system-prompt bytes stay cache-stable across recovery retries.
    recovery_prompt_reason: Option<String>,
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
            model_visible_tool_preview_budget_exhausted: false,
            suppressed_tool_previews: 0,
            suppressed_tool_call_ids: HashSet::new(),
            recovery_activations: 0,
            blocked_tool_calls: 0,
            consecutive_blocked_tool_calls: 0,
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
            claimed_patch_recovery_paths: HashSet::new(),
            seen_successful_readonly_signatures: HashSet::new(),
            streamed_tool_call_item_ids: HashMap::new(),
            failure_diagnosis_memo: HashMap::new(),
            failure_diagnosis_model_calls: 0,
            auto_permission_probe_model_calls: 0,
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
            recovery_prompt_reason: None,
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

    /// Return a memoized failure-diagnosis entry for `key`.
    pub(crate) fn failure_diagnosis_memo_get(&self, key: &DiagnosisMemoKey) -> Option<DiagnosisMemoEntry> {
        self.failure_diagnosis_memo.get(key).cloned()
    }

    /// Store a failure-diagnosis entry. Memo is bounded so a pathological
    /// failure storm cannot grow the turn state without limit.
    pub(crate) fn failure_diagnosis_memo_put(&mut self, key: DiagnosisMemoKey, entry: DiagnosisMemoEntry) {
        const MAX_MEMO: usize = 32;
        if self.failure_diagnosis_memo.len() >= MAX_MEMO {
            self.failure_diagnosis_memo.clear();
        }
        self.failure_diagnosis_memo.insert(key, entry);
    }

    /// Whether another model-backed diagnosis is allowed this turn.
    pub(crate) fn can_spend_failure_diagnosis_model_call(&self) -> bool {
        self.failure_diagnosis_model_calls < AUX_MODEL_CALL_BUDGET
    }

    /// Count a model-backed diagnosis attempt (success or failure).
    pub(crate) fn record_failure_diagnosis_model_call(&mut self) {
        self.failure_diagnosis_model_calls = self.failure_diagnosis_model_calls.saturating_add(1);
    }

    /// Whether another model-backed prompt-injection probe is allowed this turn.
    pub(crate) fn can_spend_auto_permission_probe_model_call(&self) -> bool {
        self.auto_permission_probe_model_calls < AUX_MODEL_CALL_BUDGET
    }

    /// Count a model-backed prompt-injection probe attempt (success or failure).
    pub(crate) fn record_auto_permission_probe_model_call(&mut self) {
        self.auto_permission_probe_model_calls = self.auto_permission_probe_model_calls.saturating_add(1);
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

    /// Per-result fallback for provider-history insertion. Registry outputs
    /// are already bounded; other producers still need a finite preview.
    #[cfg(test)]
    pub(crate) fn bound_model_visible_tool_preview_with_budget(
        &mut self,
        tool_name: Option<&str>,
        content: String,
        budget_bytes: usize,
    ) -> String {
        self.bound_model_visible_tool_preview_inner(None, tool_name, content, budget_bytes)
    }

    pub(crate) fn bound_model_visible_tool_preview_for_call_with_budget(
        &mut self,
        tool_call_id: &str,
        tool_name: Option<&str>,
        content: String,
        budget_bytes: usize,
    ) -> String {
        // Retain diagnostics for archived/legacy markers without letting
        // them hide new output or revoke the current tool catalog.
        self.observe_upstream_preview_budget_exhaustion(tool_call_id, &content, budget_bytes);
        self.bound_model_visible_tool_preview_inner(Some(tool_call_id), tool_name, content, budget_bytes)
    }

    fn bound_model_visible_tool_preview_inner(
        &mut self,
        tool_call_id: Option<&str>,
        tool_name: Option<&str>,
        content: String,
        budget_bytes: usize,
    ) -> String {
        let limit = budget_bytes.max(1);
        if content.len() <= limit {
            return content;
        }
        self.record_suppressed_tool_preview(tool_call_id);
        let preview = bounded_tool_preview_metadata(tool_name, &content);
        if preview.len() <= limit {
            preview
        } else {
            // Pathological nested metadata must not produce a cut JSON object.
            // Keep a finite excerpt in a valid envelope even on this fallback.
            let fallback = serde_json::json!({
                "preview_truncated": true,
                "byte_count": content.len(),
                "preview": vtcode_commons::sanitizer::redact_secrets(
                    vtcode_commons::preview::condense_text_bytes(&content, limit / 16, limit / 16),
                ),
            })
            .to_string();
            if fallback.len() <= limit {
                fallback
            } else if limit >= 2 {
                "{}".to_owned()
            } else {
                "0".to_owned()
            }
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

    /// Recognize archived exhaustion markers for diagnostic compatibility.
    /// These markers never gate calls, hide fresh results, or arm recovery.
    /// Call identity keeps response replacements from inflating diagnostics.
    pub(crate) fn observe_upstream_preview_budget_exhaustion(
        &mut self,
        tool_call_id: &str,
        content: &str,
        _budget_bytes: usize,
    ) -> bool {
        // Registry responses are already bounded. Refuse to parse an
        // oversized marker candidate here so arbitrary local/MCP output cannot
        // force a second unbounded JSON parse before the local limiter runs.
        let exhausted = content.len() <= TOOL_PREVIEW_METADATA_PARSE_LIMIT_BYTES
            && content.contains("\"preview_budget_exhausted\"")
            && serde_json::from_str::<PreviewBudgetExhaustedProbe>(content)
                .ok()
                .and_then(|probe| probe.preview_budget_exhausted)
                == Some(true);
        if !exhausted {
            return false;
        }

        self.model_visible_tool_preview_budget_exhausted = true;
        self.record_suppressed_tool_preview(Some(tool_call_id));
        true
    }

    /// Legacy exhaustion diagnostic, retained for replay compatibility only.
    /// This flag does not gate new tool calls or preview visibility.
    #[cfg(test)]
    pub(crate) fn model_visible_preview_budget_exhausted(&self) -> bool {
        self.model_visible_tool_preview_budget_exhausted
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
            self.recovery_prompt_reason = self.recovery_reason.clone();
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
        self.recovery_prompt_reason = self.recovery_reason.clone();
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

    #[cfg(test)]
    pub(crate) fn recovery_reason(&self) -> Option<&str> {
        self.recovery_reason.as_deref()
    }

    /// Reason frozen into the `[Recovery Mode]` prompt block. Stable for the
    /// whole recovery activation so consecutive recovery turns keep an
    /// identical system-prompt prefix.
    pub(crate) fn recovery_prompt_reason(&self) -> Option<&str> {
        self.recovery_prompt_reason.as_deref().or(self.recovery_reason.as_deref())
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
            self.recovery_prompt_reason = self.recovery_reason.clone();
        } else if self.recovery_prompt_reason.is_none() {
            self.recovery_prompt_reason = self.recovery_reason.clone();
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

    /// Reserve the one path-cap exception before a batch starts executing.
    pub(crate) fn claim_patch_recovery_path(&mut self, path: std::path::PathBuf) -> bool {
        self.claimed_patch_recovery_paths.insert(path)
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

/// Prefer one substantive body for a per-result head/tail excerpt. Outcome
/// metadata is preserved separately; identical producer aliases are not copied
/// into multiple preview fields.
const TOOL_PREVIEW_BODY_FIELDS: [&str; 5] = ["output", "preview", "content", "stdout", "stderr"];

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
        "This result has a bounded preview; complete output remains in the spool and current-session tool-output viewer."
    } else {
        "This result has a bounded preview; use targeted extraction for additional evidence."
    };
    let mut metadata = serde_json::json!({
        "tool": tool_name.map(|name| bounded_preview_string(name, TOOL_PREVIEW_METADATA_STRING_LIMIT)),
        "spool_path": spool_path,
        "byte_count": byte_count,
        "completion_state": completion_state,
        "preview_truncated": true,
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
            "spool_line_count",
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
    let body = object
        .and_then(|value| {
            TOOL_PREVIEW_BODY_FIELDS
                .iter()
                .find_map(|field| value.get(*field).and_then(serde_json::Value::as_str))
        })
        .unwrap_or(content);
    metadata["preview"] =
        serde_json::Value::String(vtcode_commons::ansi::strip_ansi(&vtcode_commons::sanitizer::redact_secrets(
            vtcode_commons::preview::condense_text_bytes(body, 2 * 1024, 2 * 1024),
        )));
    metadata.to_string()
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
mod tests;
