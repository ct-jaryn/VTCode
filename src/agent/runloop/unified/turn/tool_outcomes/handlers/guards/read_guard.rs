//! Guards for file read operations.
//!
//! Contains two guards:
//! 1. **Read-after-write guard**: Prevents reading a file that was just written
//! 2. **Repeated read-only call guard**: Prevents excessive reads of the same file
//!
//! The repeated read guard uses a two-tier approach:
//! - **Family cap**: Catches identical slice retries (same path + same offset/limit)
//! - **Per-file-path cap**: Catches paginated reads of the same file (different offsets)

use serde_json::{Value, json};
use vtcode_core::config::constants::tools as tool_names;
use vtcode_core::tools::tool_intent::{ShellActivity, classify_shell_activity};

use super::super::ValidationResult;
use super::super::looping::low_signal_family_key;
use super::common::{extract_read_path, is_read_action, push_guard_failure_messages};
use crate::agent::runloop::unified::tool_reads::spool_page_source_path;
use crate::agent::runloop::unified::turn::context::TurnProcessingContext;
use crate::agent::runloop::unified::turn::tool_outcomes::helpers::{find_duplicate_in_history, signature_key_for};
use crate::agent::runloop::unified::turn::tool_outcomes::response_content::maybe_inline_spooled;

/// Maximum consecutive reads of the same file with the same slice (offset/limit/raw).
const MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS: usize = 4;

/// Per-file-path read cap, independent of slice (offset/limit/raw). Catches
/// paginated reads of the same file that the slice-aware family key lets
/// through. Set higher than the family cap to allow legitimate pagination
/// (e.g., reading a large file in 3-4 chunks) while stopping excessive
/// re-reads (8+ reads of the same file with different offsets).
const MAX_SAME_FILE_PATH_READ_CALLS: usize = 6;

/// Planning doubles both read caps, mirroring the generous planning research
/// budget (120 calls/turn floor) and the wider blocked-call fuse in plan mode.
/// Execution mode keeps the strict caps so genuine loops still converge.
pub(crate) fn effective_read_family_cap(planning_active: bool) -> usize {
    if planning_active {
        MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS.saturating_mul(2)
    } else {
        MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS
    }
}

/// Planning-aware per-file-path cap. See [`effective_read_family_cap`].
pub(crate) fn effective_read_path_cap(planning_active: bool) -> usize {
    if planning_active {
        MAX_SAME_FILE_PATH_READ_CALLS.saturating_mul(2)
    } else {
        MAX_SAME_FILE_PATH_READ_CALLS
    }
}

/// Decision returned by `check_read_family_cap`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadFamilyCapDecision {
    /// No family-key applies (non-read tool), or the streak is still under the cap.
    BelowCap,
    /// The streak reached the cap.
    Tripped {
        /// Human-readable target extracted from the family key.
        target: String,
        /// System-facing reason describing why recovery was scheduled.
        block_reason: String,
        /// Model-facing error payload (serialized JSON).
        error_content: String,
    },
}

/// Extract a human-readable target from a read-family key.
///
/// Family keys look like:
///   - `read_file::<path>`
///   - `unified_file::read::<path>`
///   - `unified_file::read::<path>::off=N::lim=M::raw=bool`
///
/// The slice-suffix segments (`off=`, `lim=`, `raw=`) are stripped.
pub(crate) fn read_family_target(family_key: &str) -> String {
    let mut segments = family_key.split("::");
    // Skip the leading tool name (`read_file`/`unified_file`).
    segments.next();
    // The next segment is the action marker (`read`) for unified tools,
    // or the path itself for `read_file`. Skip it only if it is an action.
    let second = segments.next().unwrap_or("");
    if !matches!(second, "read" | "run") {
        // `read_file::<path>` — the second segment IS the target.
        if !second.is_empty()
            && !second.starts_with("off=")
            && !second.starts_with("lim=")
            && !second.starts_with("raw=")
        {
            return second.to_string();
        }
    }
    segments
        .filter(|segment| {
            !segment.is_empty()
                && !segment.starts_with("off=")
                && !segment.starts_with("lim=")
                && !segment.starts_with("raw=")
        })
        .next()
        .unwrap_or("current file")
        .to_string()
}

/// Pure decision: does this read-family streak trip the per-turn cap?
pub(crate) fn check_read_family_cap(
    canonical_tool_name: &str,
    effective_args: &Value,
    streak: usize,
    cap: usize,
    planning_active: bool,
) -> ReadFamilyCapDecision {
    let Some(family_key) = repeated_file_read_family_key(canonical_tool_name, effective_args) else {
        return ReadFamilyCapDecision::BelowCap;
    };
    if streak < cap {
        return ReadFamilyCapDecision::BelowCap;
    }
    let target = if canonical_tool_name == tool_names::CODE_SEARCH {
        normalised_code_search_path(effective_args).unwrap_or_else(|| "workspace".to_string())
    } else {
        read_family_target(&family_key)
    };
    let block_reason = if planning_active {
        format!(
            "Repeated read-only exploration of '{target}' hit the per-turn family cap ({cap}). Scheduling a final recovery pass without more tools; synthesize the `<proposed_plan>` from evidence already gathered."
        )
    } else {
        format!(
            "Repeated read-only exploration of '{target}' hit the per-turn family cap ({cap}). Scheduling a final recovery pass without more tools."
        )
    };
    let error_content = build_repeated_file_read_family_error_content_for_mode(&target, planning_active);
    ReadFamilyCapDecision::Tripped { target, block_reason, error_content }
}

/// Get the family key for a read-file call.
fn repeated_file_read_family_key(canonical_tool_name: &str, args: &Value) -> Option<String> {
    use super::super::looping::spool_chunk_read_path;

    if spool_chunk_read_path(canonical_tool_name, args).is_some() {
        return None;
    }

    match canonical_tool_name {
        tool_names::READ_FILE | tool_names::UNIFIED_FILE => low_signal_family_key(canonical_tool_name, args),
        tool_names::CODE_SEARCH => vtcode_core::tools::normalised_code_search_loop_identity(args)
            .map(|identity| format!("code_search::{identity}")),
        tool_names::UNIFIED_EXEC | tool_names::EXEC_COMMAND | "command_session" => {
            if let Some(exec_read) = parse_simple_exec_read_target(args) {
                return Some(format!("unified_exec::read::{}{}", exec_read.path, exec_read.slice_suffix));
            }
            // Track file-reading shell commands in the family guard to prevent
            // bypass via unified_exec. Only commands on the is_readonly_unified_exec_command
            // allowlist (tool_intent.rs) reach this point — cat, head, tail, bat.
            let parts = vtcode_core::tools::command_args::command_words(args).ok()??;
            let command_name = parts.first()?.as_str();
            if !matches!(command_name, "cat" | "head" | "tail" | "bat") {
                return None;
            }
            // Use the full command as the family key so different files are tracked separately
            let command_str = parts.join(" ");
            Some(format!("unified_exec::run::{command_str}"))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecReadTarget {
    path: String,
    slice_suffix: String,
}

fn parse_simple_exec_read_target(args: &Value) -> Option<ExecReadTarget> {
    let parts = vtcode_core::tools::command_args::command_words(args).ok()??;
    if parts.iter().any(|part| matches!(part.as_str(), "&&" | "|" | ";")) {
        return None;
    }

    parse_simple_sed_read_target(parts.as_slice())
}

fn parse_simple_sed_read_target(parts: &[String]) -> Option<ExecReadTarget> {
    if parts.first().map(String::as_str) != Some("sed") {
        return None;
    }

    let mut cursor = 1usize;
    if parts.get(cursor).map(String::as_str) != Some("-n") {
        return None;
    }
    cursor += 1;

    let script = parts.get(cursor)?.as_str();
    cursor += 1;

    let path = parts.get(cursor)?.as_str();
    if cursor + 1 != parts.len() {
        return None;
    }

    let (start, end) = parse_simple_sed_print_range(script)?;
    let limit = end.saturating_sub(start).saturating_add(1);
    Some(ExecReadTarget {
        path: path.to_string(),
        slice_suffix: format!("::off={start}::lim={limit}"),
    })
}

fn parse_simple_sed_print_range(script: &str) -> Option<(usize, usize)> {
    let range = script.strip_suffix('p')?;
    let (start, end) = match range.split_once(',') {
        Some((start, end)) => (start, end),
        None => (range, range),
    };

    let start = start.parse::<usize>().ok()?;
    let end = end.parse::<usize>().ok()?;
    (start <= end).then_some((start, end))
}

fn repeated_read_path(canonical_tool_name: &str, effective_args: &Value) -> Option<String> {
    if let Some(target) = parse_simple_exec_read_target(effective_args) {
        if matches!(canonical_tool_name, tool_names::UNIFIED_EXEC | tool_names::EXEC_COMMAND | "command_session") {
            return Some(target.path);
        }
    }

    if is_read_action(canonical_tool_name, effective_args) {
        return extract_read_path(effective_args);
    }

    if canonical_tool_name == tool_names::CODE_SEARCH {
        // Distinct `code_search` queries scoped to the same path are diverse
        // research, not paginated re-reads: the family cap already guards
        // identical searches via the query-aware loop identity
        // (`normalised_code_search_loop_identity`), whose streak resets on a
        // new query. Counting every distinct query toward the per-path total
        // tripped the cap after a handful of legitimate planning queries on
        // one file (e.g. seven distinct queries on `benches/startup.rs`).
        return None;
    }

    None
}

fn normalised_code_search_path(effective_args: &Value) -> Option<String> {
    let path = effective_args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .unwrap_or(".");
    Some(vtcode_core::tools::normalised_code_search_path(path))
}

/// Planning-aware variant: in plan mode the model must finalize the
/// `<proposed_plan>` from evidence already gathered instead of starting more
/// research, otherwise the tool-free recovery synthesis emits prose and the
/// turn blocks with no approval-ready draft.
/// Planning-mode next step for family-cap rejections: once the same read
/// family repeats, more reads add no evidence.
const PLANNING_READ_CAP_NEXT_STEP: &str = "Synthesize the `<proposed_plan>` from the output already gathered; \
re-reading files already read this turn adds no new evidence.";

#[cold]
fn build_repeated_file_read_family_error_content_for_mode(target: &str, planning_active: bool) -> String {
    let next_step = if planning_active {
        PLANNING_READ_CAP_NEXT_STEP
    } else {
        "Reuse the output already gathered or try a different approach."
    };
    let guidance = format!(
        "Repeated exploration of the same file or path ('{target}') exceeded the per-turn cap, so further reads of it are blocked this turn. {next_step}"
    );
    super::super::super::execution_result::build_error_content(guidance, None, None, "repeated_read_family").to_string()
}

/// Returns the path if this is a read of a planning artifact (a runtime-owned
/// plan file or tracker) while planning mode is active.
///
/// Scoped to `.vtcode/plans/` and `.vtcode/tasks/` so ordinary markdown docs
/// and paths that merely contain `plan` (for example `planning_workflow`)
/// do not receive plan-specific reuse guidance.
fn is_plan_artifact_read(canonical_tool_name: &str, args: &Value) -> Option<String> {
    if !is_read_action(canonical_tool_name, args) {
        return None;
    }
    let path = extract_read_path(args)?;
    let normalized = path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    if lower.contains(".vtcode/plans/") || lower.contains(".vtcode/tasks/") || lower.ends_with(".tasks.md") {
        Some(path)
    } else {
        None
    }
}

/// Whether a read-only call is an inspection whose value is its visible body.
/// Once the model-visible preview budget is exhausted, inspections return
/// metadata stubs without content, so admitting more of them only burns
/// request cycles. Fail-open: unknown tools are never inspections.
fn is_preview_gated_inspection(canonical_tool_name: &str, effective_args: &Value) -> bool {
    if is_read_action(canonical_tool_name, effective_args) {
        return true;
    }
    if matches!(
        canonical_tool_name,
        tool_names::CODE_SEARCH | tool_names::UNIFIED_SEARCH | tool_names::GREP_FILE | tool_names::LIST_FILES
    ) {
        return true;
    }
    if matches!(canonical_tool_name, tool_names::UNIFIED_EXEC | tool_names::EXEC_COMMAND | "command_session") {
        return matches!(classify_shell_activity(canonical_tool_name, effective_args), ShellActivity::Inspection);
    }
    false
}

#[cold]
fn build_preview_exhaustion_error_content(planning_active: bool) -> String {
    let guidance = if planning_active {
        "Tool preview budget is exhausted this turn; further inspection returns hidden stubs. \
         Synthesize the `<proposed_plan>` now from the evidence already gathered. \
         Verification, task_tracker, session polling, spool paging in small ranges using a spool_path already in this conversation, and plan-draft re-reads stay open; exhausted inspections are blocked."
    } else {
        "Tool preview budget is exhausted this turn; further inspection returns hidden stubs. \
         Work from the evidence already visible: summarize status, edit, verify, or report. \
         To read a spooled output, page it in small ranges using a spool_path already in this conversation."
    };
    super::super::super::execution_result::build_error_content(
        guidance.to_string(),
        None,
        None,
        "preview_exhaustion_gate",
    )
    .to_string()
}

/// Reject read-only inspections once the model-visible preview budget is
/// exhausted. Post-exhaustion responses are metadata stubs without body
/// content, so executing more inspections cannot surface new evidence — it
/// only grows the request and starves synthesis or implementation.
///
/// Channels that stay useful without visible bodies remain open: verification
/// commands (exit codes survive in stub metadata), the planning interview,
/// task bookkeeping, session polling (verifier completions arrive through
/// it), spool paging (kept visible through preview credit), and plan-draft
/// re-reads (which carry finalize-the-plan guidance). Rejections feed the
/// existing blocked-call fuse, so persistent flailing still converges on
/// recovery instead of looping here.
pub(crate) fn enforce_preview_exhaustion_inspection_gate(
    ctx: &mut TurnProcessingContext<'_>,
    tool_call_id: &str,
    canonical_tool_name: &str,
    effective_args: &Value,
    readonly_classification: bool,
) -> Option<ValidationResult> {
    if !readonly_classification {
        return None;
    }
    if !ctx.harness_state.model_visible_preview_budget_exhausted() {
        return None;
    }
    if matches!(
        canonical_tool_name,
        tool_names::TASK_TRACKER | tool_names::REQUEST_USER_INPUT | tool_names::WRITE_STDIN
    ) {
        return None;
    }
    if matches!(canonical_tool_name, tool_names::UNIFIED_EXEC | tool_names::EXEC_COMMAND | "command_session")
        && matches!(classify_shell_activity(canonical_tool_name, effective_args), ShellActivity::Verification)
    {
        return None;
    }
    if spool_page_source_path(canonical_tool_name, effective_args).is_some() {
        return None;
    }
    if ctx.tool_registry.is_planning_active() && is_plan_artifact_read(canonical_tool_name, effective_args).is_some() {
        return None;
    }
    if !is_preview_gated_inspection(canonical_tool_name, effective_args) {
        return None;
    }
    let planning_active = ctx.tool_registry.is_planning_active();
    let block_reason = if planning_active {
        "Tool preview budget exhausted; inspection blocked. Synthesize the plan from collected evidence."
    } else {
        "Tool preview budget exhausted; inspection blocked. Work from visible evidence instead."
    }
    .to_string();
    let error_content = build_preview_exhaustion_error_content(planning_active);
    push_guard_failure_messages(ctx, tool_call_id, canonical_tool_name, error_content, &block_reason);
    Some(ValidationResult::PreviewExhausted)
}

/// Build the error content for a read-after-write guard trip.
#[cold]
fn build_read_after_write_error(path: &str) -> String {
    super::super::super::execution_result::build_error_content(
        format!(
            "File '{path}' was just written in this turn. The write response includes a diff preview. Reuse the diff output or specify offset/limit for a specific range."
        ),
        None,
        None,
        "read_after_write",
    )
    .to_string()
}

/// Enforce the read-after-write guard.
///
/// Returns `Some(ValidationResult::Blocked)` when the guard trips,
/// or `None` when the guard passes.
pub(crate) fn enforce_read_after_write_guard(
    ctx: &mut TurnProcessingContext<'_>,
    tool_call_id: &str,
    canonical_tool_name: &str,
    effective_args: &Value,
) -> Option<ValidationResult> {
    if !is_read_action(canonical_tool_name, effective_args) {
        return None;
    }

    let path = extract_read_path(effective_args)?;

    if !ctx.harness_state.was_recently_written(&path) {
        return None;
    }

    let content = build_read_after_write_error(&path);
    ctx.push_rejected_tool_response(tool_call_id, Some(canonical_tool_name), Some(effective_args), content);
    Some(ValidationResult::Blocked)
}

/// Enforce the repeated read-only call guard.
///
/// Uses a two-tier approach:
/// 1. Family cap: Catches identical slice retries (same path + same offset/limit)
/// 2. Per-file-path cap: Catches paginated reads of the same file
///
/// Returns `Some(ValidationResult::Blocked)` when either guard trips,
/// or `None` when both guards pass.
pub(crate) fn enforce_repeated_read_only_call_guard(
    ctx: &mut TurnProcessingContext<'_>,
    tool_call_id: &str,
    canonical_tool_name: &str,
    effective_args: &Value,
    readonly_classification: bool,
) -> Option<ValidationResult> {
    if !readonly_classification {
        return None;
    }

    // Blindness brake first: post-exhaustion inspections return hidden stubs,
    // so executing them cannot surface new evidence. Reject before counting
    // or serving anything; verification, spool paging, and bookkeeping stay
    // open inside the gate itself.
    if let Some(outcome) = enforce_preview_exhaustion_inspection_gate(
        ctx,
        tool_call_id,
        canonical_tool_name,
        effective_args,
        readonly_classification,
    ) {
        return Some(outcome);
    }

    // Planning doubles the read caps, mirroring the generous planning research
    // budget (120 calls/turn floor) and the wider blocked-call fuse in plan mode.
    // Execution mode keeps the strict caps so genuine loops still converge.
    let planning_active = ctx.tool_registry.is_planning_active();
    let family_cap = effective_read_family_cap(planning_active);
    let path_cap = effective_read_path_cap(planning_active);
    let signature = signature_key_for(canonical_tool_name, effective_args);

    // Plan-artifact fast path: serve runtime-owned plan/tracker re-reads
    // WITHOUT advancing the family/path counters. Planning synthesis
    // legitimately re-reads its own draft, so counting those toward loop caps
    // starves synthesis. This stays scoped to `.vtcode/plans/`,
    // `.vtcode/tasks/`, and `*.tasks.md` so ordinary docs never bypass caps.
    // All other reads fall through to the caps below first, so identical-slice
    // retry loops still trip instead of looping forever on cache hits.
    let plan_path = planning_active
        .then(|| is_plan_artifact_read(canonical_tool_name, effective_args))
        .flatten();
    let plan_lookup_done = plan_path.is_some();
    if let Some(plan_path) = plan_path.as_deref() {
        if let Some(mut reused_value) = ctx.tool_registry.find_recent_successful_by_read_target(
            canonical_tool_name,
            effective_args,
            ctx.harness_state.max_tool_wall_clock,
        ) {
            if let Some(obj) = reused_value.as_object_mut() {
                super::super::apply_reused_read_only_loop_metadata(obj);
                // Overwrite with planning-specific guidance AFTER the generic
                // metadata is applied, since apply_reused_read_only_loop_metadata
                // sets its own loop_detected_note.
                obj.insert(
                    "loop_detected_note".to_string(),
                    json!(format!(
                        "Planning mode: plan file '{}' was already read. Stop re-reading and finalize the plan.",
                        plan_path
                    )),
                );
            }
            ctx.push_tool_response(
                tool_call_id,
                Some(canonical_tool_name),
                maybe_inline_spooled(canonical_tool_name, &reused_value),
            );
            ctx.harness_state.record_successful_readonly_signature(signature);
            ctx.harness_state.record_reused_result();
            return Some(ValidationResult::Handled);
        }
    }

    if let Some(family_key) = repeated_file_read_family_key(canonical_tool_name, effective_args) {
        // The streak mutation is stateful and stays here; the cap *decision*
        // is delegated to the pure `check_read_family_cap` helper so it can be
        // tested without the full TurnProcessingContext harness.
        let streak = ctx.harness_state.record_file_read_family_call(family_key);
        if let ReadFamilyCapDecision::Tripped { target: _, block_reason, error_content } =
            check_read_family_cap(canonical_tool_name, effective_args, streak, family_cap, planning_active)
        {
            ctx.activate_recovery(block_reason.clone());
            push_guard_failure_messages(ctx, tool_call_id, canonical_tool_name, error_content, &block_reason);
            return Some(ValidationResult::Blocked);
        }
    }

    // Per-file-path cap: catches paginated reads of the same file that the
    // slice-aware family key lets through (e.g., 8 reads of anthropic_types.rs
    // at different offsets each get a different family key and never collide).
    // `code_search` is excluded from this cap (see `repeated_read_path`):
    // distinct queries on one path are diverse research guarded by the
    // query-aware family cap above, not pagination.
    if let Some(path) = repeated_read_path(canonical_tool_name, effective_args) {
        let path_count = ctx.harness_state.record_file_read_path_call(path.clone());
        if path_count > path_cap {
            let block_reason = format!(
                "Repeated reads of '{path}' hit the per-file-path cap ({path_cap}), so further reads of this path are blocked for the rest of this turn. Reads of other paths, edits, and other useful actions remain available; continue from the evidence already gathered."
            );
            let error_content = super::super::super::execution_result::build_error_content(
                block_reason.clone(),
                None,
                None,
                "repeated_read_path",
            )
            .to_string();
            push_guard_failure_messages(ctx, tool_call_id, canonical_tool_name, error_content, &block_reason);
            return Some(ValidationResult::Blocked);
        }
    }

    // Cap-first: exact duplicates, cross-turn TTL matches, and history
    // duplicates are served only after the counters above have advanced. This
    // preserves the identical-slice loop guard: serving a cached hit must not
    // hide a retry loop that should force tool-free recovery.
    if ctx.harness_state.has_successful_readonly_signature(signature.as_str())
        && let Some(mut reused_value) = ctx.tool_registry.find_recent_successful_output(
            canonical_tool_name,
            effective_args,
            ctx.harness_state.max_tool_wall_clock,
        )
    {
        if let Some(obj) = reused_value.as_object_mut() {
            super::super::apply_reused_read_only_loop_metadata(obj);
        }
        ctx.push_tool_response(
            tool_call_id,
            Some(canonical_tool_name),
            maybe_inline_spooled(canonical_tool_name, &reused_value),
        );
        ctx.harness_state.record_reused_result();
        return Some(ValidationResult::Handled);
    }

    // Cross-turn TTL-bounded cache (covers same-path different-offset supersets
    // via `read_extent_matches`). Plan artifacts already looked this up before
    // the caps above, so skip the duplicate lookup for them.
    if !plan_lookup_done
        && let Some(mut reused_value) = ctx.tool_registry.find_recent_successful_by_read_target(
            canonical_tool_name,
            effective_args,
            ctx.harness_state.max_tool_wall_clock,
        )
    {
        if let Some(obj) = reused_value.as_object_mut() {
            super::super::apply_reused_read_only_loop_metadata(obj);
        }
        ctx.push_tool_response(
            tool_call_id,
            Some(canonical_tool_name),
            maybe_inline_spooled(canonical_tool_name, &reused_value),
        );
        ctx.harness_state.record_successful_readonly_signature(signature);
        ctx.harness_state.record_reused_result();
        return Some(ValidationResult::Handled);
    }

    // Cross-turn duplicate: scan working history.
    if let Some(raw_output) = find_duplicate_in_history(
        ctx.working_history,
        canonical_tool_name,
        effective_args,
        ctx.tool_registry.workspace_root(),
    ) {
        if let Ok(mut parsed) = serde_json::from_str::<Value>(&raw_output) {
            if let Some(obj) = parsed.as_object_mut() {
                super::super::apply_reused_read_only_loop_metadata(obj);
            }
            ctx.push_tool_response(
                tool_call_id,
                Some(canonical_tool_name),
                maybe_inline_spooled(canonical_tool_name, &parsed),
            );
        } else {
            ctx.push_tool_response(tool_call_id, Some(canonical_tool_name), raw_output);
        }
        ctx.harness_state.record_reused_result();
        return Some(ValidationResult::Handled);
    }

    None
}

#[cfg(test)]
mod tests {
    use vtcode_core::config::constants::tools as tool_names;

    use super::*;

    #[test]
    fn repeated_file_read_family_key_tracks_cat_via_unified_exec() {
        let args = serde_json::json!({"command": "cat README.md"});
        let key = repeated_file_read_family_key(tool_names::UNIFIED_EXEC, &args);
        assert_eq!(key, Some("unified_exec::run::cat README.md".to_string()));
    }

    #[test]
    fn repeated_file_read_family_key_tracks_head_via_unified_exec() {
        let args = serde_json::json!({"command": "head -n 10 file.txt"});
        let key = repeated_file_read_family_key(tool_names::UNIFIED_EXEC, &args);
        assert_eq!(key, Some("unified_exec::run::head -n 10 file.txt".to_string()));
    }

    #[test]
    fn repeated_file_read_family_key_ignores_non_file_reading_commands() {
        let args = serde_json::json!({"command": "ls -la"});
        let key = repeated_file_read_family_key(tool_names::UNIFIED_EXEC, &args);
        assert_eq!(key, None);
    }

    #[test]
    fn repeated_file_read_family_key_ignores_git_status() {
        let args = serde_json::json!({"command": "git status"});
        let key = repeated_file_read_family_key(tool_names::UNIFIED_EXEC, &args);
        assert_eq!(key, None);
    }

    #[test]
    fn repeated_file_read_family_key_handles_cmd_alias() {
        let args = serde_json::json!({"cmd": "cat Cargo.toml"});
        let key = repeated_file_read_family_key(tool_names::UNIFIED_EXEC, &args);
        assert_eq!(key, Some("unified_exec::run::cat Cargo.toml".to_string()));
    }

    #[test]
    fn repeated_file_read_family_key_tracks_simple_sed_ranges_via_unified_exec() {
        let args = serde_json::json!({"command": "sed -n '440,520p' Cargo.toml"});
        let key = repeated_file_read_family_key(tool_names::UNIFIED_EXEC, &args);
        assert_eq!(key, Some("unified_exec::read::Cargo.toml::off=440::lim=81".to_string()));
    }

    #[test]
    fn repeated_file_read_family_key_tracks_simple_sed_ranges_via_public_exec_command() {
        let args = serde_json::json!({"cmd": "sed -n '440,520p' Cargo.toml"});
        let key = repeated_file_read_family_key(tool_names::EXEC_COMMAND, &args);
        assert_eq!(key, Some("unified_exec::read::Cargo.toml::off=440::lim=81".to_string()));
    }

    #[test]
    fn repeated_file_read_family_key_ignores_complex_sed_commands() {
        let args = serde_json::json!({"command": "sed -n '440,520p' Cargo.toml extra.txt"});
        let key = repeated_file_read_family_key(tool_names::UNIFIED_EXEC, &args);
        assert_eq!(key, None);
    }

    #[test]
    fn repeated_file_read_family_key_returns_none_for_missing_command() {
        let args = serde_json::json!({});
        let key = repeated_file_read_family_key(tool_names::UNIFIED_EXEC, &args);
        assert_eq!(key, None);
    }

    #[test]
    fn repeated_file_read_family_key_tracks_code_search_identity() {
        let args = serde_json::json!({"query": "HarnessTurnState", "path": "README.md"});
        let key = repeated_file_read_family_key(tool_names::CODE_SEARCH, &args)
            .expect("valid code_search should have a loop identity");
        assert!(key.starts_with("code_search::"));
        assert!(key.contains("HarnessTurnState"));
    }

    #[test]
    fn repeated_read_path_excludes_code_search_distinct_queries() {
        // Distinct `code_search` queries scoped to one path are diverse
        // research, not pagination: the query-aware family cap guards
        // identical searches, so the per-path total must not count them.
        // This is the `benches/startup.rs` regression: seven distinct queries
        // tripped the per-turn cap and blocked planning with no draft.
        assert_eq!(
            repeated_read_path(tool_names::CODE_SEARCH, &serde_json::json!({"query": "fn", "path": "README.md"})),
            None
        );
        assert_eq!(repeated_read_path(tool_names::CODE_SEARCH, &serde_json::json!({"query": "fn"})), None);
        assert_eq!(
            repeated_read_path(
                tool_names::CODE_SEARCH,
                &serde_json::json!({"query": "fn", "path": "./docs/../README.md"})
            ),
            None
        );
    }

    #[test]
    fn code_search_family_cap_reports_path_not_serialized_identity() {
        let args = serde_json::json!({"query": "a very long query", "path": "./docs/../README.md"});
        let decision = check_read_family_cap(
            tool_names::CODE_SEARCH,
            &args,
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            false,
        );

        let ReadFamilyCapDecision::Tripped { target, block_reason, .. } = decision else {
            panic!("expected code_search family cap to trip");
        };
        assert_eq!(target, "README.md");
        assert!(block_reason.contains("'README.md'"));
        assert!(!block_reason.contains("a very long query"));
    }

    #[test]
    fn read_family_cap_decision_below_cap_for_non_read_tool() {
        let decision = check_read_family_cap(
            tool_names::UNIFIED_EXEC,
            &serde_json::json!({"command": "ls -la"}),
            99,
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            false,
        );
        assert_eq!(decision, ReadFamilyCapDecision::BelowCap);
    }

    #[test]
    fn read_family_cap_decision_below_cap_when_streak_under_cap() {
        let decision = check_read_family_cap(
            tool_names::UNIFIED_FILE,
            &serde_json::json!({"action": "read", "path": "src/lib.rs", "offset": 0, "limit": 100}),
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS - 1,
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            false,
        );
        assert_eq!(decision, ReadFamilyCapDecision::BelowCap);
    }

    #[test]
    fn read_family_cap_decision_tripped_at_cap() {
        let decision = check_read_family_cap(
            tool_names::UNIFIED_FILE,
            &serde_json::json!({"action": "read", "path": "src/lib.rs", "offset": 0, "limit": 100}),
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            false,
        );
        match decision {
            ReadFamilyCapDecision::Tripped { target, block_reason, error_content } => {
                assert_eq!(target, "src/lib.rs");
                assert!(block_reason.contains("per-turn family cap"));
                assert!(error_content.contains("repeated_read_family"));
            }
            ReadFamilyCapDecision::BelowCap => panic!("expected Tripped at cap"),
        }
    }

    #[test]
    fn read_family_cap_planning_guidance_directs_plan_synthesis() {
        let decision = check_read_family_cap(
            tool_names::UNIFIED_FILE,
            &serde_json::json!({"action": "read", "path": "src/lib.rs"}),
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            true,
        );
        match decision {
            ReadFamilyCapDecision::Tripped { block_reason, error_content, .. } => {
                assert!(block_reason.contains("<proposed_plan>"));
                assert!(error_content.contains("<proposed_plan>"));
            }
            ReadFamilyCapDecision::BelowCap => panic!("expected Tripped at cap"),
        }
    }

    #[test]
    fn read_family_target_strips_slice_suffix() {
        assert_eq!(read_family_target("unified_file::read::src/cli/update.rs::off=81::lim=229"), "src/cli/update.rs");
        assert_eq!(read_family_target("read_file::src/main.rs::off=80::lim=200::raw=true"), "src/main.rs");
        assert_eq!(read_family_target("unified_file::read::src/cli/update.rs"), "src/cli/update.rs");
        assert_eq!(read_family_target("unified_exec::run::cat README.md"), "cat README.md");
        assert_eq!(read_family_target("unified_exec::read::Cargo.toml::off=440::lim=81"), "Cargo.toml");
        assert_eq!(read_family_target("read_file::src/lib.rs"), "src/lib.rs");
    }

    #[test]
    fn read_family_cap_decision_tripped_above_cap() {
        let decision = check_read_family_cap(
            tool_names::READ_FILE,
            &serde_json::json!({"path": "src/main.rs"}),
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS + 5,
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            false,
        );
        assert!(matches!(decision, ReadFamilyCapDecision::Tripped { .. }));
    }

    #[test]
    fn read_family_cap_decision_uses_bare_path_target_when_unpaginated() {
        let decision = check_read_family_cap(
            tool_names::UNIFIED_FILE,
            &serde_json::json!({"action": "read", "path": "src/cli/update.rs"}),
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            false,
        );
        match decision {
            ReadFamilyCapDecision::Tripped { target, .. } => {
                assert_eq!(target, "src/cli/update.rs");
            }
            ReadFamilyCapDecision::BelowCap => panic!("expected Tripped at cap"),
        }
    }

    #[test]
    fn is_read_action_returns_true_for_unified_file_read() {
        assert!(is_read_action(
            tool_names::UNIFIED_FILE,
            &serde_json::json!({"action": "read", "path": "src/lib.rs"})
        ));
        assert!(is_read_action(tool_names::UNIFIED_FILE, &serde_json::json!({"path": "src/lib.rs"})));
        assert!(!is_read_action(
            tool_names::UNIFIED_FILE,
            &serde_json::json!({"action": "write", "path": "src/lib.rs"})
        ));
    }

    #[test]
    fn extract_read_path_returns_path_from_args() {
        assert_eq!(extract_read_path(&serde_json::json!({"path": "src/lib.rs"})), Some("src/lib.rs".to_string()));
        assert_eq!(extract_read_path(&serde_json::json!({})), None);
    }

    #[test]
    fn max_same_file_path_read_calls_is_stricter_than_family_cap() {
        const _: () = assert!(
            MAX_SAME_FILE_PATH_READ_CALLS >= MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS,
            "per-file-path cap must be >= family cap"
        );
        const _: () = assert!(MAX_SAME_FILE_PATH_READ_CALLS < 10, "per-file-path cap must catch excessive reads");
    }

    #[test]
    fn planning_doubles_both_read_caps() {
        assert_eq!(effective_read_family_cap(false), MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS);
        assert_eq!(effective_read_path_cap(false), MAX_SAME_FILE_PATH_READ_CALLS);
        assert_eq!(effective_read_family_cap(true), MAX_CONSECUTIVE_SAME_FILE_READ_FAMILY_CALLS.saturating_mul(2));
        assert_eq!(effective_read_path_cap(true), MAX_SAME_FILE_PATH_READ_CALLS.saturating_mul(2));
    }

    #[test]
    fn plan_artifact_read_is_scoped_to_runtime_owned_paths() {
        let plan_file = serde_json::json!({"action": "read", "path": ".vtcode/plans/session-123.md"});
        assert!(is_plan_artifact_read(tool_names::UNIFIED_FILE, &plan_file).is_some());

        let tracker = serde_json::json!({"action": "read", "path": ".vtcode/tasks/current_task.md"});
        assert!(is_plan_artifact_read(tool_names::UNIFIED_FILE, &tracker).is_some());

        // Ordinary markdown docs must not receive plan-specific guidance.
        let doc = serde_json::json!({"action": "read", "path": "docs/guides/planning-workflow.md"});
        assert!(is_plan_artifact_read(tool_names::UNIFIED_FILE, &doc).is_none());

        // Paths that merely contain `plan` are not plan artifacts.
        let code =
            serde_json::json!({"action": "read", "path": "src/agent/runloop/unified/planning_workflow_state.rs"});
        assert!(is_plan_artifact_read(tool_names::UNIFIED_FILE, &code).is_none());
    }
}
