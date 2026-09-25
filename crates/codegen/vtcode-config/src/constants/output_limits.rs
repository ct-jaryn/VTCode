//! Output truncation limits inspired by OpenAI Codex multi-tier truncation.
//! Prevents OOM with three independent size limits.
//! Reference: <https://openai.com/index/unrolling-the-codex-agent-loop/>

/// Maximum size for single agent message payloads (bytes) - 4 MB.
pub const MAX_AGENT_MESSAGES_SIZE: usize = 4 * 1024 * 1024;

/// Maximum size for entire message history payloads (bytes) - 24 MB.
pub const MAX_ALL_MESSAGES_SIZE: usize = 24 * 1024 * 1024;

/// Maximum retained error-log payload bytes in the in-memory collector (10 MiB).
pub const ERROR_LOG_BUFFER_SIZE_LIMIT_BYTES: usize = 10 * 1024 * 1024;

/// Maximum size per line (bytes) - 256 KB.
/// Prevents OOM on malformed output with very long lines.
pub const MAX_LINE_LENGTH: usize = 256 * 1024;

/// Default message count limit for history.
pub const DEFAULT_MESSAGE_LIMIT: usize = 4_000;

/// Maximum message count limit.
pub const MAX_MESSAGE_LIMIT: usize = 20_000;

/// Regular aggregate provider-visible tool preview budget across one turn
/// (64 KiB execution, 96 KiB planning).
///
/// Once a turn's regular tool previews exhaust this budget, later responses
/// keep bounded outcome/control metadata while payload bodies are truncated
/// or omitted. Verifier-sized responses can still use the separate, finite
/// `TURN_TINY_PREVIEW_BUDGET_BYTES` reserve. Planning gets the larger regular
/// budget because read-only research needs roughly a dozen spooled previews
/// before synthesis; execution keeps a slightly tighter bound so recovery
/// still converges promptly, while allowing a normal ~10-15 tool-call turn
/// (2-5 KiB per preview) to complete without blinding the model.
pub const TURN_PREVIEW_BUDGET_BYTES: usize = 64 * 1024;
/// Plan-mode per-turn preview budget (96 KiB ≈ a dozen spooled previews).
pub const TURN_PREVIEW_BUDGET_BYTES_PLANNING: usize = 96 * 1024;

/// Effective per-turn preview budget for the active workflow mode.
#[inline]
pub const fn turn_preview_budget_bytes(planning_active: bool) -> usize {
    if planning_active {
        TURN_PREVIEW_BUDGET_BYTES_PLANNING
    } else {
        TURN_PREVIEW_BUDGET_BYTES
    }
}

/// Maximum payload-body size admitted to the small verifier-preview reserve.
///
/// Session `session-vtcode-20260913T074747Z_225432-45397` exhausted its 32 KiB
/// budget (now 64 KiB) on a 24 KiB README read, then stripped 25/45 later outputs — even
/// 5-byte `grep -c` / link-check verifiers — blinding the model into repeated
/// identical shell runs. Small outcome payloads (exit codes, counts, short
/// `BROKEN:` lists) are the evidence verifiers need. Keep them available after
/// the regular budget is spent, while charging them to a separate per-turn
/// reserve so repeated calls remain bounded. Both preview layers must use this
/// threshold and reserve.
pub const TINY_PREVIEW_BYPASS_BYTES: usize = 1024;

/// Aggregate allowance for verifier-sized payloads per turn (8 KiB).
///
/// This reserve is independent of the regular execution/planning preview
/// budget and is reset at the same turn boundary.
pub const TURN_TINY_PREVIEW_BUDGET_BYTES: usize = 8 * 1024;

/// Truncation marker appended when content is cut off.
const TRUNCATION_MARKER: &str = "\n[... content truncated due to size limit ...]";

/// Collect content with lazy truncation (Codex pattern).
/// Marks truncated but continues draining to prevent pipe blocking.
///
/// # Arguments
/// * `output` - Accumulated output buffer
/// * `new_content` - New content to append
/// * `max_size` - Maximum allowed size
/// * `truncated` - Mutable flag tracking truncation state
///
/// # Returns
/// `true` if content was appended, `false` if truncated
#[inline]
fn collect_with_truncation(output: &mut String, new_content: &str, max_size: usize, truncated: &mut bool) -> bool {
    let new_size = output.len() + new_content.len();

    if new_size > max_size {
        if !*truncated {
            output.push_str(TRUNCATION_MARKER);
            *truncated = true;
        }
        return false;
    }

    output.push_str(new_content);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_within_limit() {
        let mut output = String::new();
        let mut truncated = false;

        assert!(collect_with_truncation(&mut output, "hello", 100, &mut truncated));
        assert_eq!(output, "hello");
        assert!(!truncated);
    }

    #[test]
    fn test_collect_at_limit_triggers_truncation() {
        let mut output = String::from("hello");
        let mut truncated = false;

        assert!(!collect_with_truncation(&mut output, " world that exceeds", 10, &mut truncated));
        assert!(output.contains(TRUNCATION_MARKER));
        assert!(truncated);
    }

    #[test]
    fn test_truncation_marker_appended_only_once() {
        let mut output = String::new();
        let mut truncated = false;

        // Triggers initial truncation
        collect_with_truncation(&mut output, "first content", 5, &mut truncated);
        let len_after_marker = output.len();
        assert!(truncated);

        // Should not append marker again
        collect_with_truncation(&mut output, "second content", 5, &mut truncated);
        assert_eq!(output.len(), len_after_marker);
    }

    #[test]
    fn turn_preview_budget_splits_by_workflow_mode() {
        assert_eq!(turn_preview_budget_bytes(false), 64 * 1024);
        assert_eq!(turn_preview_budget_bytes(true), 96 * 1024);
    }
}
