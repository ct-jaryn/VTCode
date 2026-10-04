//! LoopTracker: cross-tool loop, low-signal, and verification gate state.

use super::*;

/// Threshold: number of consecutive read/search operations before the Navigation
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
    pub(super) coarse_inspection_attempts: FxHashMap<String, (usize, Instant)>,
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
    pub(super) nav_signatures: FxHashSet<String>,
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

    pub(super) fn record_low_signal(&mut self, signature: String) -> usize {
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
    pub(super) fn reset_navigation_window(&mut self, clear_low_signal_attempts: bool) {
        self.consecutive_navigations = 0;
        self.nav_signatures.clear();
        self.reset_low_signal_navigation_counters();
        if clear_low_signal_attempts {
            self.reset_low_signal_attempts();
        }
    }

    pub(super) fn record_navigation_signal(&mut self, is_low_signal: bool) {
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

    pub(super) fn record_successful_mutation(&mut self) {
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

    pub(super) fn mark_verification_complete(&mut self) {
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
