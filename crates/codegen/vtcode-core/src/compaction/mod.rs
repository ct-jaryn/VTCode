use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::fmt::Write;
use std::sync::Arc;
use vtcode_commons::is_context_capacity_message;
use vtcode_commons::llm::FinishReason;
use vtcode_config::constants::context::DEFAULT_COMPACTION_TRIGGER_RATIO;

use crate::config::types::{ReasoningEffortLevel, VerbosityLevel};
use crate::exec::events::CompactionMode;
use crate::llm::reasoning_effort::ReasoningEffortMapper;
use crate::llm::utils::truncate_to_token_limit;
use crate::llm::{
    collect_single_response,
    provider::{
        LLMProvider, LLMRequest, LLMResponse, Message, MessageContent, MessageRole, ResponsesCompactionOptions,
        ToolChoice, ToolDefinition,
    },
};

pub mod auto;
pub mod memory_envelope;
pub mod prefire;
pub mod two_pass;

pub use crate::compaction::memory_envelope::{effective_context_budget, effective_session_context_budget};
pub use crate::compaction::prefire::{AsyncCompactionCache, PrefireState};

pub const SUPPRESS_NONE: u8 = 0;
pub const SUPPRESS_TURN: u8 = 1;
pub const SUPPRESS_STICKY: u8 = 2;
pub const SUPPRESS_UNTIL_SUCCESS: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuppressReason {
    CreditBlock,
    Size,
    Auth,
    Schema,
    Other,
}

impl SuppressReason {
    fn suppress_state(self) -> u8 {
        match self {
            SuppressReason::Size | SuppressReason::Schema => SUPPRESS_STICKY,
            SuppressReason::CreditBlock | SuppressReason::Auth => SUPPRESS_UNTIL_SUCCESS,
            SuppressReason::Other => SUPPRESS_TURN,
        }
    }
}

/// Classify a deterministic compaction failure's error text into a fixed
/// [`SuppressReason`] (drives telemetry + sticky-vs-per-turn scope).
pub(crate) fn classify_suppress_reason(error_msg: &str) -> SuppressReason {
    let m = error_msg.to_ascii_lowercase();
    if m.contains("spending-limit")
        || m.contains("spending limit")
        || m.contains("out of credits")
        || m.contains("usage balance exhausted")
        || m.contains("usage limit reached")
    {
        SuppressReason::CreditBlock
    } else if m.contains("context length") || m.contains("too many tokens") {
        SuppressReason::Size
    } else if m.contains("status 401") || m.contains("unauthorized") {
        SuppressReason::Auth
    } else if m.contains("invalid_request_error") {
        SuppressReason::Schema
    } else {
        SuppressReason::Other
    }
}

const DEFAULT_COMPACTION_TARGET_THRESHOLD: f64 = 0.50;
const DEFAULT_COMPACTION_KEEP_LAST_MESSAGES: usize = 10;
const DEFAULT_RETAINED_USER_MESSAGE_TOKENS: usize = 20_000;
const DEFAULT_RETAINED_USER_MESSAGES: usize = 6;
/// Internal continuity budget. This is deliberately not configuration: changing
/// it changes the shape of every compacted request and therefore provider cache
/// behavior.
const CONTINUITY_TAIL_TARGET_TOKENS: usize = 20_000;
/// Keep history below the model window after reserving space for the system
/// prompt, memory envelope, summary framing, and the next response.
const COMPACTION_CONTEXT_OVERHEAD_FRACTION_DENOMINATOR: usize = 8;
const COMPACTION_CONTEXT_FIXED_OVERHEAD_TOKENS: usize = 512;
const SUMMARY_PREFIX: &str = "Previous conversation summary:\n";
const ABSTRACT_PREFIX: &str = "Earlier context (abstract):\n";
const DETAIL_PREFIX: &str = "Recent context (summary):\n";

/// Default summarization prompt. Structures the summary for continuity: after
/// reading it, the next context must feel like a seamless continuation, not a
/// fresh start. Kept as a `const` so `CompactionConfig::default` does not
/// re-allocate a ~1KB literal on every construction.
const DEFAULT_SUMMARY_PROMPT: &str = "Summarize the conversation so far using this exact structure. The goal is continuity: after reading this summary, the next context must feel like a seamless continuation of the same task, not a fresh start.\n\n## Goal\n[What the user is trying to accomplish]\n\n## Constraints & Preferences\n- [Requirements, preferences, or constraints from the user]\n\n## What I Was Just Doing\n[The single most recent action in progress: what step the agent was executing, which tool or edit was underway, and where it left off. This is the continuity anchor.]\n\n## Last Action & Result\n[The last completed action and its outcome (success, error, or partial). Include the exact error or status if relevant.]\n\n## Progress\n### Done\n- [Completed work]\n\n### In Progress\n- [Current work]\n\n### Blocked\n- [Blocking issues, if any]\n\n## Key Decisions\n- **[Decision]**: [Reason]\n\n## Next Steps\n1. [Most important next step]\n\n## Critical Context\n- [Facts needed to continue]\n\nKeep it concise and actionable. Always preserve the current task objective and acceptance criteria, file paths that were read or modified, test results and error messages, and decisions with their reasoning.";

/// Compaction configuration for context window management.
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// Threshold (0.0-1.0) at which to trigger compaction.
    pub trigger_threshold: f64,
    /// Target usage ratio (0.0-1.0) after compaction.
    pub target_threshold: f64,
    /// Prompt for summarization.
    pub summary_prompt: String,
    /// Legacy short-circuit used to skip local compaction for tiny histories.
    pub keep_last_messages: usize,
    /// Total token budget reserved for retaining real user messages verbatim.
    pub retained_user_message_tokens: usize,
    /// Maximum number of recent user messages to retain verbatim.
    pub retained_user_messages: usize,
    /// Force local summarization even for short histories and providers with native compaction.
    pub always_summarize: bool,
    /// Enable hierarchical summarization (multi-level pyramid).
    ///
    /// When `true`, compaction produces three tiers instead of a flat summary:
    /// - **Abstract**: oldest turns compressed into 1-2 sentences
    /// - **Detail**: middle turns summarized into a paragraph
    /// - **Verbatim**: most recent turns kept as-is
    ///
    /// When `false` (default), all old turns become a single flat summary.
    pub hierarchical: bool,
    /// Auto-compaction suppression state. `SUPPRESS_NONE` (0) means compaction
    /// is allowed; any other value gates automatic compaction until the
    /// appropriate clearing event (success, model switch, etc.).
    pub auto_compact_suppressed: u8,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            trigger_threshold: DEFAULT_COMPACTION_TRIGGER_RATIO,
            target_threshold: DEFAULT_COMPACTION_TARGET_THRESHOLD,
            summary_prompt: DEFAULT_SUMMARY_PROMPT.to_string(),
            keep_last_messages: DEFAULT_COMPACTION_KEEP_LAST_MESSAGES,
            retained_user_message_tokens: DEFAULT_RETAINED_USER_MESSAGE_TOKENS,
            retained_user_messages: DEFAULT_RETAINED_USER_MESSAGES,
            always_summarize: false,
            hierarchical: false,
            auto_compact_suppressed: SUPPRESS_NONE,
        }
    }
}

/// Per-route compaction policy in the DeepSeek-harness shape.
///
/// `threshold_ratio` caps the auto-compaction trigger as a fraction of the
/// route window (fail-safe: it can only fire *earlier*). `retain_ratio` sizes
/// the verbatim continuity tail as a fraction of the route window so small
/// windows keep a usable summary instead of a tail-only history.
/// `max_overflow_retries` bounds extra bounded-fork attempts after a
/// context-capacity rejection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionRoutePolicy {
    /// Trigger ceiling as a fraction of the route window. `1.0` preserves the
    /// absolute-tokens trigger unchanged.
    pub threshold_ratio: f64,
    /// Verbatim-tail share of the route window.
    pub retain_ratio: f64,
    /// Extra attempts after a context-capacity rejection (progressive halving).
    pub max_overflow_retries: u32,
}

/// Floor keeping the continuity anchor meaningful on tiny windows.
const MIN_TAIL_TARGET_TOKENS: usize = 1_024;

impl Default for CompactionRoutePolicy {
    fn default() -> Self {
        Self {
            threshold_ratio: 1.0,
            retain_ratio: 0.16,
            max_overflow_retries: 1,
        }
    }
}

/// Per-route overrides, matched as `(provider_substring, model_substring)`.
/// Empty until a route demonstrates a need: the window-derived defaults
/// already adapt retention and budgets to every route's resolved capacity.
const ROUTE_POLICIES: &[(&str, &str, CompactionRoutePolicy)] = &[];

impl CompactionRoutePolicy {
    /// Resolve the policy for a provider/model route (first table hit wins,
    /// otherwise the default).
    #[must_use]
    pub fn resolve(provider_name: &str, model: &str) -> Self {
        ROUTE_POLICIES
            .iter()
            .find(|(provider, model_match, _)| provider_name.contains(provider) && model.contains(model_match))
            .map(|(_, _, policy)| *policy)
            .unwrap_or_default()
    }

    /// Verbatim-tail budget for a route window. Unknown windows (`0`) keep the
    /// legacy constant; otherwise the tail scales with the window so small
    /// routes keep a usable summary instead of a tail-only history.
    #[allow(
        clippy::cast_sign_loss,
        reason = "The ratio is clamped to [0.0, 1.0] and the window is non-negative, so the product cannot be negative."
    )]
    #[must_use]
    pub fn tail_target_tokens(&self, window_tokens: usize) -> usize {
        if window_tokens == 0 {
            return CONTINUITY_TAIL_TARGET_TOKENS;
        }
        ((window_tokens as f64 * self.retain_ratio.clamp(0.0, 1.0)) as usize)
            .clamp(MIN_TAIL_TARGET_TOKENS, CONTINUITY_TAIL_TARGET_TOKENS)
    }

    /// Fail-safe trigger cap: only fires compaction *earlier*, never later.
    /// A `1.0` ratio (the default) leaves the absolute-tokens trigger
    /// unchanged.
    #[allow(
        clippy::cast_sign_loss,
        reason = "The ratio is clamped to [0.0, 1.0] and the window is non-negative, so the product cannot be negative."
    )]
    #[must_use]
    pub fn apply_threshold_cap(&self, threshold_tokens: usize, window_tokens: usize) -> usize {
        if self.threshold_ratio < 1.0 && window_tokens > 0 {
            threshold_tokens.min((window_tokens as f64 * self.threshold_ratio.clamp(0.0, 1.0)) as usize)
        } else {
            threshold_tokens
        }
    }
}

/// Resolve the route tail budget from the session budget (already intersected
/// with the provider window) or the provider window when unset.
#[must_use]
pub(crate) fn route_tail_target_tokens(
    provider: &dyn LLMProvider,
    model: &str,
    context_budget: Option<usize>,
) -> usize {
    let policy = CompactionRoutePolicy::resolve(provider.name(), model);
    let window = context_budget
        .filter(|value| *value > 0)
        .unwrap_or_else(|| provider.effective_context_size(model));
    policy.tail_target_tokens(window)
}

/// Parent request prefix for cache-safe compaction forking.
///
/// Prompt caching is a prefix match: a compaction request only reuses the
/// parent conversation's cached prefix when it carries the *exact same*
/// system prompt, tool definitions, and history prefix, with the compaction
/// instruction appended as the final new turn. A standalone single-message
/// summary prompt pays the full uncached input rate for the entire history.
#[derive(Debug, Clone, Default)]
pub struct CompactionParentContext {
    /// Exact system prompt of the parent segment (`request_envelope.system_prompt()`).
    pub system_prompt: Option<Arc<str>>,
    /// Exact ordered tool catalog of the parent segment
    /// (`request_envelope.ordered_tools()`). Empty catalogs should be `None`.
    pub tools: Option<Arc<Vec<ToolDefinition>>>,
}

impl CompactionParentContext {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.system_prompt.is_none() && self.tools.is_none()
    }
}

/// Build a cache-safe compaction history: the parent's full conversation
/// prefix verbatim, plus the compaction instruction as the only new turn.
/// From the provider's perspective this looks nearly identical to the
/// parent's last request, so the cached prefix is reused.
#[must_use]
pub fn build_cache_safe_compaction_history(history: &[Message], compaction_prompt: &str) -> Vec<Message> {
    let mut forked = Vec::with_capacity(history.len().saturating_add(1));
    forked.extend(history.iter().cloned());
    forked.push(Message::user(compaction_prompt.to_string()));
    forked
}

/// Bound the summarizer's input to the resolved context budget.
///
/// The local-summary path forks the parent's entire conversation verbatim and
/// appends the compaction instruction, but nothing bounded that fork. On a
/// near-full context (the normal reason to run `/compact`) or after switching
/// to a model with a smaller window, the summary request itself exceeded the
/// summarizer's window and the provider rejected it, failing the whole
/// compaction. Local output paths are bounded by
/// [`bound_compacted_history_to_context`]; native provider output is treated as
/// canonical and must not be rewritten. Bound the input so local requests fit:
/// keep the newest complete protocol groups that fit and drop the oldest.
///
/// Returns the history to append the instruction to (see
/// [`build_cache_safe_compaction_history`]), not the finished fork.
fn bound_history_for_summarization(history: &[Message], instructions: &str, budget: Option<usize>) -> Vec<Message> {
    let Some(budget) = budget.filter(|value| *value > 0) else {
        return history.to_vec();
    };
    let instruction_tokens = Message::user(instructions.to_string()).estimate_tokens();
    let history_budget = budget.saturating_sub(instruction_tokens).max(4);
    bound_history_to_token_budget(history, history_budget)
}

/// Bound a native compaction request while retaining the latest provider
/// compaction marker. Provider-native windows use that opaque marker as the
/// continuity anchor; dropping it while selecting a recent suffix loses the
/// state needed to replay the window on the next request.
fn bound_history_for_native_compaction(history: &[Message], instructions: &str, budget: Option<usize>) -> Vec<Message> {
    let Some(budget) = budget.filter(|value| *value > 0) else {
        return history.to_vec();
    };
    let instruction_tokens = Message::user(instructions.to_string()).estimate_tokens();
    let history_budget = budget.saturating_sub(instruction_tokens).max(4);
    let total_tokens = history.iter().map(Message::estimate_tokens).sum::<usize>();
    if total_tokens <= history_budget {
        return history.to_vec();
    }

    let Some(marker_index) = history.iter().rposition(is_provider_compaction_message) else {
        return bound_history_to_token_budget(history, history_budget);
    };
    let marker = history[marker_index].clone();
    let marker_tokens = marker.estimate_tokens();
    let mut bounded = vec![marker];
    if marker_tokens < history_budget {
        bounded.extend(bound_history_to_token_budget(&history[marker_index + 1..], history_budget - marker_tokens));
    }
    bounded
}

fn bound_history_to_token_budget(history: &[Message], history_budget: usize) -> Vec<Message> {
    let total_tokens = history.iter().map(Message::estimate_tokens).sum::<usize>();
    if total_tokens <= history_budget {
        return history.to_vec();
    }

    let group_starts: Vec<usize> = history
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == MessageRole::User).then_some(index))
        .collect();
    let last_group_index = group_starts.len().saturating_sub(1);

    let mut selected_start = history.len();
    let mut selected_end = history.len();
    let mut selected_tokens = 0usize;
    for (position, &start) in group_starts.iter().enumerate().rev() {
        let natural_end = group_starts.get(position + 1).copied().unwrap_or(history.len());
        // The trailing group may end on an unanswered tool call; drop that
        // invalid protocol suffix instead of shipping it to the provider.
        let end = if position == last_group_index {
            start.saturating_add(complete_protocol_group_prefix(&history[start..natural_end]))
        } else {
            natural_end
        };
        if start >= end {
            continue;
        }
        let group_tokens = history[start..end].iter().map(Message::estimate_tokens).sum::<usize>();
        if selected_tokens.saturating_add(group_tokens) > history_budget {
            break;
        }
        if selected_start == history.len() {
            selected_end = end;
        }
        selected_tokens += group_tokens;
        selected_start = start;
    }

    if selected_start >= selected_end {
        // No complete protocol group fits (for example a single oversized tool
        // result). Degrade to protocol-bounded previews so the request still
        // goes out instead of failing the entire compaction.
        return bounded_protocol_group(history, history_budget);
    }
    history[selected_start..selected_end].to_vec()
}

fn is_provider_compaction_message(message: &Message) -> bool {
    message.role == MessageRole::Assistant
        && message
            .reasoning_details
            .as_ref()
            .is_some_and(|details| details.iter().any(is_compaction_detail))
}

#[cfg(test)]
mod summarization_fork_bounds_tests {
    use super::{Message, bound_history_for_native_compaction, bound_history_for_summarization};

    const INSTRUCTIONS: &str = "Summarize now.";

    fn total_tokens(messages: &[Message]) -> usize {
        messages.iter().map(Message::estimate_tokens).sum()
    }

    #[test]
    fn keeps_history_verbatim_when_it_already_fits() {
        let history = vec![Message::user("a".repeat(4_000)), Message::user("b".repeat(4_000))];
        let bounded = bound_history_for_summarization(&history, INSTRUCTIONS, Some(10_000_000));
        assert_eq!(bounded.len(), history.len());
        assert_eq!(total_tokens(&bounded), total_tokens(&history));
    }

    #[test]
    fn keeps_history_verbatim_when_budget_is_unknown() {
        let history = vec![Message::user("x".repeat(40_000))];
        let bounded = bound_history_for_summarization(&history, INSTRUCTIONS, None);
        assert_eq!(bounded.len(), 1);
        assert_eq!(total_tokens(&bounded), total_tokens(&history));
    }

    #[test]
    fn trims_oldest_groups_when_over_budget_and_keeps_the_newest_turn() {
        let history = vec![
            Message::user("old".repeat(2_000)),
            Message::user("middle".repeat(2_000)),
            Message::user("newest".repeat(64)),
        ];
        // Derive the limit from the measured fixture so this remains a
        // genuine over-budget case across tokenizer changes.
        let budget = total_tokens(&history).saturating_sub(1);
        let bounded = bound_history_for_summarization(&history, INSTRUCTIONS, Some(budget));
        assert!(bounded.len() < history.len(), "expected trimming, kept {}", bounded.len());
        assert_eq!(
            bounded.last().unwrap().content.as_text().as_ref(),
            history.last().unwrap().content.as_text().as_ref(),
            "the newest turn must survive the trim"
        );
        assert!(total_tokens(&bounded) <= budget);
    }

    #[test]
    fn falls_back_to_protocol_previews_when_no_group_fits() {
        let history = vec![Message::user("x".repeat(40_000))];
        // Derive the limit from the measured fixture so this remains a
        // genuine no-group-fits case across tokenizer changes.
        let budget = total_tokens(&history) / 4;
        let bounded = bound_history_for_summarization(&history, INSTRUCTIONS, Some(budget));
        assert_eq!(bounded.len(), 1);
        assert!(total_tokens(&bounded) < total_tokens(&history), "an oversized single group must still be reduced");
        assert!(
            total_tokens(&bounded) <= budget,
            "fallback must respect budget {budget}, used {}",
            total_tokens(&bounded)
        );
    }

    #[test]
    fn native_bound_preserves_latest_provider_compaction_marker() {
        let marker = Message::assistant(String::new()).with_reasoning_details(Some(vec![serde_json::json!({
            "type": "compaction",
            "content": null,
            "encrypted_content": "opaque-state",
        })]));
        let newest = Message::user("newest".repeat(64));
        let history = vec![marker.clone(), Message::user("old".repeat(4_000)), newest.clone()];
        let instruction_tokens = Message::user(INSTRUCTIONS.to_string()).estimate_tokens();
        let history_budget = marker.estimate_tokens() + newest.estimate_tokens() + 4;
        let bounded =
            bound_history_for_native_compaction(&history, INSTRUCTIONS, Some(instruction_tokens + history_budget));

        assert_eq!(bounded.first(), Some(&marker));
        assert_eq!(bounded.last(), Some(&newest));
        assert!(bounded.len() < history.len(), "expected old pre-compaction history to be dropped");
    }
}

/// Build a cache-safe local-summary request that reuses the parent prefix.
/// `tool_choice` is forced to `none` so the summarizer cannot spend the
/// compaction pass on tool calls while the system/tools/messages prefix
/// stays identical to the parent's last request.
fn compaction_summary_request(
    model: &str,
    history: &[Message],
    instructions: &str,
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<ReasoningEffortLevel>,
    verbosity: Option<VerbosityLevel>,
    parent: Option<&CompactionParentContext>,
) -> LLMRequest {
    LLMRequest {
        messages: Arc::new(build_cache_safe_compaction_history(history, instructions)),
        model: model.to_string(),
        system_prompt: parent.and_then(|parent| parent.system_prompt.clone()),
        tools: parent.and_then(|parent| parent.tools.clone()).filter(|tools| !tools.is_empty()),
        tool_choice: Some(ToolChoice::none()),
        max_tokens: max_output_tokens,
        reasoning_effort,
        verbosity,
        ..Default::default()
    }
}

/// Compact conversation history using the configured summarizer.
#[cfg_attr(feature = "profiling", hotpath::measure)]
pub async fn compact_history(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    config: &CompactionConfig,
) -> Result<Vec<Message>> {
    compact_history_with_budget(provider, model, history, config, None).await
}

/// Compact conversation history using a caller-resolved context budget for
/// input bounding and locally rebuilt output. Standalone native responses are
/// returned as the provider's canonical next context window.
pub async fn compact_history_with_budget(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    config: &CompactionConfig,
    context_budget: Option<usize>,
) -> Result<Vec<Message>> {
    if history.is_empty() {
        return Ok(Vec::new());
    }

    if !config.always_summarize && history.len() <= config.keep_last_messages {
        // Message-count retention is only a fast path. A single large tool
        // result can exceed the resolved token budget even when the history
        // contains fewer messages than `keep_last_messages`.
        return Ok(bound_compacted_history_to_context(history.to_vec(), provider, model, context_budget));
    }

    // `supports_responses_compaction` is shared by standalone Responses
    // endpoints and Anthropic's inline context-management path. This legacy
    // function calls `compact_history` directly, so only the narrower
    // standalone capability is safe here; inline providers use the manual
    // strategy dispatcher below (or the local fallback on this legacy path).
    if !config.always_summarize && provider.supports_manual_openai_compaction(model) {
        let native_source = bound_history_for_native_compaction(
            history,
            "",
            compaction_history_budget(provider, model, context_budget),
        );
        let compacted = provider
            .compact_history(model, &native_source)
            .await
            .context("Failed to compact history via Responses compact endpoint")?;
        // A standalone compaction response is the provider's canonical next
        // context window. Do not append a locally selected continuity tail or
        // discard opaque provider items from it. The provider has already
        // produced the exact window that must be replayed on the next turn.
        return Ok(compacted);
    }

    let effective_config = context_bounded_compaction_config(provider, model, history, config, context_budget);
    // Cache-safe forking: prepend the parent's full conversation prefix and
    // append the compaction instruction, instead of reformatting the entire
    // history into a single new user message (which would pay the full
    // uncached input rate). No parent system/tools are available on this
    // legacy path; callers with a request envelope should prefer
    // `compact_history_manual_with_parent_context`.
    // Bound the fork so the summary request itself fits the summarizer window;
    // an unbounded fork is what made `/compact` fail on near-full or
    // smaller-window (model-switch) histories.
    let history_budget = compaction_history_budget(provider, model, context_budget);
    let tail_target = route_tail_target_tokens(provider, model, context_budget);
    // Trim oversized tool outputs before bounding so large dumps do not evict
    // whole protocol groups from the summarizer fork.
    let pruned_history = prune_oversized_tool_outputs(history);
    let summary_source =
        bound_history_for_summarization(&pruned_history, &effective_config.summary_prompt, history_budget);
    let summary = generate_local_summary_with_retry(
        provider,
        model,
        &pruned_history,
        &summary_source,
        &effective_config.summary_prompt,
        history_budget,
        &ManualCompactionOptions::default(),
        None,
    )
    .await?;

    Ok(bound_compacted_history_to_context(
        build_local_compacted_history(
            history,
            &summary,
            effective_config.retained_user_message_tokens,
            effective_config.retained_user_messages,
            // Keep the same protocol-safe continuity tail as the live/manual
            // paths. Forked histories must not lose the newest working turn.
            true,
            tail_target,
        ),
        provider,
        model,
        context_budget,
    ))
}

/// How the manual `/compact` command compacts for a given provider/model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStrategy {
    /// Provider exposes a standalone on-demand compaction endpoint
    /// (OpenAI `/responses/compact`). Delegates to `LLMProvider::compact_history_with_options`.
    NativeStandalone,
    /// Provider compacts inline via request fields, threshold-triggered
    /// (Anthropic `compact_20260112`). Invoked through the capability-aware
    /// one-shot response collector with `context_management` set and
    /// `pause_after_compaction`.
    NativeInline,
    /// Universal fallback: summarize history via the capability-aware one-shot
    /// response collector and rebuild as a summary message plus retained recent
    /// user messages. Works for every provider.
    Local,
}

/// Select the manual-compaction strategy for a provider/model.
///
/// `NativeStandalone` when the provider opts in via `supports_manual_openai_compaction`
/// (e.g. OpenAI `/responses/compact`), `NativeInline` when the provider reports
/// inline compaction support via `supports_native_inline_compaction` (e.g.
/// Anthropic `compact_20260112`), otherwise `Local`.
///
/// Note: `supports_responses_compaction` is intentionally *not* the discriminator
/// for `NativeInline`. It is overloaded — true for both OpenAI-compatible
/// standalone compaction and Anthropic inline compaction — so OpenAI-compatible
/// custom endpoints (which report it but cannot serve an Anthropic
/// `compact_20260112` edit) would otherwise be misrouted to `NativeInline` and
/// waste a rejected `generate` call before falling back to `Local`.
pub fn manual_compaction_strategy(provider: &dyn LLMProvider, model: &str) -> CompactionStrategy {
    if provider.supports_manual_openai_compaction(model) {
        CompactionStrategy::NativeStandalone
    } else if provider.supports_native_inline_compaction(model) {
        CompactionStrategy::NativeInline
    } else {
        CompactionStrategy::Local
    }
}

/// Whether a compaction result is worth keeping.
///
/// Local compaction always appends framing around the summarized history (a
/// summary message plus the persisted session-memory envelope), so a result
/// whose message count is not strictly smaller than the input grows the
/// conversation instead of compacting it. This happens when the continuity
/// tail already covers the whole history (small, tool-heavy sessions where a
/// handful of user turns anchor dozens of assistant/tool messages): the
/// rebuild keeps every message and adds framing on top (observed as
/// `86 -> 88`). Such results must be discarded in favor of the already-compact
/// path so callers never report growth as a successful compaction.
///
/// The check is intentionally message-count based: that is the unit surfaced
/// in user-facing progress (`86 -> 12`) and harness `compact_boundary`
/// events, so the persisted history must never exceed it.
#[must_use]
pub fn compacted_history_shrinks(original_len: usize, compacted_len: usize, mode: CompactionMode) -> bool {
    // Local mode injects one envelope message into the returned history during
    // persistence; account for it here so the *final* history is what shrinks.
    // Provider-native windows (and the `Unknown` catch-all, which never arises
    // from a live compaction pass) are canonical replay state with no envelope
    // injected, so the raw lengths compare directly.
    let envelope_messages = match mode {
        CompactionMode::Local => 1,
        CompactionMode::Provider | CompactionMode::Unknown => 0,
    };
    compacted_len.saturating_add(envelope_messages) < original_len
}

/// Universally meaningful manual-compaction options.
///
/// Provider-specific extras (OpenAI `service_tier` / `prompt_cache_key` / `store` /
/// `include`) are intentionally absent: the manual `/compact` command exposes only
/// the options that apply across every provider.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManualCompactionOptions {
    /// Overrides the default summary/compaction prompt when set.
    pub instructions: Option<String>,
    /// Caps the summary/compaction output length on every provider.
    pub max_output_tokens: Option<u32>,
    /// Optional reasoning effort override for the compaction pass.
    pub reasoning_effort: Option<ReasoningEffortLevel>,
    /// Permit an explicit lower supported effort when the requested level is
    /// unavailable on the selected provider/model route. The default is
    /// strict blocking so compaction never silently changes reasoning
    /// fidelity.
    pub allow_reasoning_effort_downgrade: bool,
    /// Optional verbosity override for the compaction output.
    pub verbosity: Option<VerbosityLevel>,
}

impl From<ManualCompactionOptions> for ResponsesCompactionOptions {
    fn from(options: ManualCompactionOptions) -> Self {
        Self {
            instructions: options.instructions,
            max_output_tokens: options.max_output_tokens,
            reasoning_effort: options.reasoning_effort,
            verbosity: options.verbosity,
            responses_include: None,
            response_store: None,
            service_tier: None,
            prompt_cache_key: None,
        }
    }
}

impl CompactionConfig {
    /// Return a config with the manual options' instructions applied as the
    /// summary prompt override. The remaining option fields
    /// (`max_output_tokens`, `reasoning_effort`, `verbosity`) are applied to the
    /// summary `LLMRequest` directly by `summarize_locally`, not stored here.
    fn with_manual_overrides(self, options: &ManualCompactionOptions) -> Self {
        let summary_prompt = options
            .instructions
            .clone()
            .map(|instructions| instructions.trim().to_string())
            .filter(|instructions| !instructions.is_empty())
            .unwrap_or(self.summary_prompt);
        Self { summary_prompt, ..self }
    }
}

/// Compact history for the manual `/compact` command using provider-native
/// compaction when available, falling back to local summarization otherwise.
///
/// Returns the compacted messages and the `CompactionMode` that produced them
/// (`Provider` for native compaction, `Local` for client-side summarization).
#[cfg_attr(feature = "profiling", hotpath::measure)]
pub async fn compact_history_manual(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    config: &CompactionConfig,
    options: &ManualCompactionOptions,
) -> Result<(Vec<Message>, CompactionMode)> {
    compact_history_manual_with_budget(provider, model, history, config, options, None).await
}

/// Manual compaction with the resolved session context budget applied to native
/// request inputs and locally rebuilt summaries. Native provider responses are
/// returned unchanged so opaque continuation state remains valid.
pub async fn compact_history_manual_with_budget(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    config: &CompactionConfig,
    options: &ManualCompactionOptions,
    context_budget: Option<usize>,
) -> Result<(Vec<Message>, CompactionMode)> {
    compact_history_manual_with_parent_context(provider, model, history, config, options, context_budget, None).await
}

/// Manual compaction that reuses the parent segment's cached prefix.
///
/// Pass the live request envelope's system prompt + ordered tools as `parent`
/// so the local-summary fork (`summarize_locally`) hits the provider prompt
/// cache instead of re-paying full input cost for the entire history.
pub async fn compact_history_manual_with_parent_context(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    config: &CompactionConfig,
    options: &ManualCompactionOptions,
    context_budget: Option<usize>,
    parent: Option<&CompactionParentContext>,
) -> Result<(Vec<Message>, CompactionMode)> {
    if history.is_empty() {
        return Ok((Vec::new(), CompactionMode::Local));
    }
    let options = resolve_manual_compaction_options(provider, model, options)?;
    match manual_compaction_strategy(provider, model) {
        CompactionStrategy::NativeStandalone => {
            let responses_options: ResponsesCompactionOptions = options.clone().into();
            // Bound the native input the same way as the local fork: a
            // near-full history would otherwise exceed the summarizer window
            // and fail the whole `/compact` command.
            let native_source = bound_history_for_native_compaction(
                history,
                options.instructions.as_deref().unwrap_or(""),
                compaction_history_budget(provider, model, context_budget),
            );
            let compacted = provider
                .compact_history_with_options(model, &native_source, &responses_options)
                .await
                .context("Failed to compact history via provider-native compaction")?;
            // A standalone compaction response is the provider's canonical
            // next context window. Keep its retained items and opaque
            // compaction items intact; the next provider request must receive
            // this window as returned rather than a locally pruned variant.
            Ok((compacted, CompactionMode::Provider))
        }
        CompactionStrategy::NativeInline => {
            compact_history_native_inline(provider, model, history, config, &options, context_budget, parent).await
        }
        CompactionStrategy::Local => {
            let compacted =
                summarize_locally(provider, model, history, config, &options, context_budget, parent).await?;
            Ok((compacted, CompactionMode::Local))
        }
    }
}

/// Resolve a manual compaction effort before selecting a provider strategy.
/// Every strategy eventually emits an `LLMRequest` or provider compaction
/// options, so validating once at this boundary prevents native, inline, and
/// hierarchical paths from silently coercing unsupported levels.
fn resolve_manual_compaction_options(
    provider: &dyn LLMProvider,
    model: &str,
    options: &ManualCompactionOptions,
) -> Result<ManualCompactionOptions> {
    let Some(requested) = options.reasoning_effort else {
        return Ok(options.clone());
    };

    let mapping = ReasoningEffortMapper::resolve(provider, model, requested, options.allow_reasoning_effort_downgrade)
        .with_context(|| {
            format!("Failed to resolve compaction reasoning effort for {} / {}", provider.name(), model)
        })?;

    if mapping.degraded() {
        tracing::warn!(
            provider = provider.name(),
            model,
            requested = %mapping.requested,
            effective = %mapping.effective,
            "Compaction reasoning effort explicitly downgraded"
        );
    }

    let mut resolved = options.clone();
    resolved.reasoning_effort = Some(mapping.effective);
    Ok(resolved)
}

/// Native inline compaction (Anthropic `compact_20260112`).
///
/// Forces a compaction pass by setting the minimum trigger threshold with
/// `pause_after_compaction: true`, so the response contains only the compaction
/// block. If compaction does not fire (history below the provider's minimum
/// trigger, currently 50k tokens for Anthropic), transparently falls back to
/// local summarization so the manual command always succeeds.
///
/// The inline request already carries the full parent history; the parent
/// system/tools prefix is attached as well so the fork hits the provider
/// prompt cache. `tool_choice` stays `none` so the pass cannot be spent on
/// tool calls, and both local fallbacks forward `parent` for the same reason.
/// Anthropic inline context-management triggers, matching the provider docs:
/// thinking blocks are cheapest to clear, tool results next, and full
/// compaction is the most aggressive last resort.
const ANTHROPIC_COMPACT_TRIGGER_FLOOR: u64 = 50_000;
const ANTHROPIC_CLEAR_TOOL_USES_TRIGGER_TOKENS: u64 = 100_000;
const ANTHROPIC_CLEAR_TOOL_USES_KEEP: u64 = 3;
const ANTHROPIC_CLEAR_THINKING_KEEP_TURNS: u64 = 2;

/// Build the Anthropic inline `context_management.edits` ladder.
///
/// Order matters: the thinking-block edit runs first, then tool-result
/// clearing, then the `compact_20260112` summarization edit. The cheaper
/// clearing edits are only included when the provider reports
/// `supports_context_edits`; otherwise the request carries the compact edit
/// alone so non-Anthropic inline routes keep working.
fn anthropic_inline_compaction_edits(instructions: Option<&str>, include_context_edits: bool) -> Vec<Value> {
    let mut edits = Vec::with_capacity(3);
    if include_context_edits {
        edits.push(json!({
            "type": "clear_thinking_20251015",
            "keep": { "type": "thinking_turns", "value": ANTHROPIC_CLEAR_THINKING_KEEP_TURNS },
        }));
        edits.push(json!({
            "type": "clear_tool_uses_20250919",
            "trigger": { "type": "input_tokens", "value": ANTHROPIC_CLEAR_TOOL_USES_TRIGGER_TOKENS },
            "keep": { "type": "tool_uses", "value": ANTHROPIC_CLEAR_TOOL_USES_KEEP },
        }));
    }
    let mut compact_edit = serde_json::Map::new();
    compact_edit.insert("type".to_string(), json!("compact_20260112"));
    compact_edit
        .insert("trigger".to_string(), json!({ "type": "input_tokens", "value": ANTHROPIC_COMPACT_TRIGGER_FLOOR }));
    compact_edit.insert("pause_after_compaction".to_string(), json!(true));
    if let Some(instructions) = instructions.map(str::trim).filter(|instructions| !instructions.is_empty()) {
        compact_edit.insert("instructions".to_string(), json!(instructions));
    }
    edits.push(Value::Object(compact_edit));
    edits
}

async fn compact_history_native_inline(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    config: &CompactionConfig,
    options: &ManualCompactionOptions,
    context_budget: Option<usize>,
    parent: Option<&CompactionParentContext>,
) -> Result<(Vec<Message>, CompactionMode)> {
    let edits =
        anthropic_inline_compaction_edits(options.instructions.as_deref(), provider.supports_context_edits(model));

    // Bound the inline request input so a near-full history fits the
    // summarizer window, mirroring the local-summary fork bound.
    let inline_source = bound_history_for_native_compaction(
        history,
        options.instructions.as_deref().unwrap_or(""),
        compaction_history_budget(provider, model, context_budget),
    );
    let request = LLMRequest {
        messages: Arc::new(inline_source),
        model: model.to_string(),
        system_prompt: parent.and_then(|parent| parent.system_prompt.clone()),
        tools: parent.and_then(|parent| parent.tools.clone()).filter(|tools| !tools.is_empty()),
        tool_choice: Some(ToolChoice::none()),
        context_management: Some(json!({ "edits": edits })),
        max_tokens: options.max_output_tokens,
        reasoning_effort: options.reasoning_effort,
        verbosity: options.verbosity,
        ..Default::default()
    };

    // The inline compaction request is Anthropic-specific (`compact_20260112`).
    // If the provider does not actually support inline compaction (e.g. it was
    // selected because it exposes a different standalone Responses compact
    // endpoint) the request may be rejected. Per the manual `/compact` contract
    // ("always succeeds"), swallow the inline error and fall back to local
    // summarization rather than aborting the whole command.
    let response = match collect_single_response(provider, request).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                error = ?error,
                "provider-native inline compaction request failed; \
                 falling back to local summarization"
            );
            let compacted =
                summarize_locally(provider, model, history, config, options, context_budget, parent).await?;
            return Ok((compacted, CompactionMode::Local));
        }
    };

    if response.finish_reason == FinishReason::Pause
        && let Some(summary) = response
            .compaction
            .as_ref()
            .map(|summary| summary.trim())
            .filter(|summary| !summary.is_empty())
    {
        let effective_config = context_bounded_compaction_config(provider, model, history, config, context_budget);
        let tail_target = route_tail_target_tokens(provider, model, context_budget);
        if let Some(detail) = response_compaction_detail(&response) {
            // The provider compaction block is opaque state. Preserve the exact
            // block and the selected protocol-safe tail for replay; applying the
            // generic local bound here could turn it into an ordinary text/system
            // message and invalidate the provider's continuation contract.
            return Ok((
                build_provider_compacted_history(history, detail, &effective_config, true, tail_target),
                CompactionMode::Provider,
            ));
        }

        // A provider may expose only the public summary field without the raw
        // continuation block. Treat that response as an ordinary local summary
        // so it receives the local context bound and is not mislabeled as a
        // provider-native replay window.
        let compacted = bound_compacted_history_to_context(
            build_summary_compacted_history(history, summary, &effective_config, true, tail_target),
            provider,
            model,
            context_budget,
        );
        return Ok((compacted, CompactionMode::Local));
    }

    // Compaction did not fire (e.g. history below the minimum trigger threshold);
    // fall back to local summarization so the manual command always succeeds.
    let compacted = summarize_locally(provider, model, history, config, options, context_budget, parent).await?;
    Ok((compacted, CompactionMode::Local))
}

/// Local (provider-agnostic) summarization compaction.
///
/// Forks the parent conversation cache-safely: same history prefix plus the
/// compaction instruction appended, with the parent system/tools prefix when
/// supplied (`None` keeps the legacy standalone shape). Rebuilds the history
/// as a summary system message plus the retained recent user messages.
/// Applies the manual options to the summary request.
///
/// When `config.hierarchical` is `true`, delegates to
/// [`summarize_locally_hierarchical`] which produces a multi-tier pyramid
/// (abstract + detail + verbatim) instead of a flat summary.
async fn summarize_locally(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    config: &CompactionConfig,
    options: &ManualCompactionOptions,
    context_budget: Option<usize>,
    parent: Option<&CompactionParentContext>,
) -> Result<Vec<Message>> {
    if config.hierarchical {
        return summarize_locally_hierarchical(provider, model, history, config, options, context_budget, parent).await;
    }

    let effective_config = context_bounded_compaction_config(
        provider,
        model,
        history,
        &config.clone().with_manual_overrides(options),
        context_budget,
    );
    let history_budget = compaction_history_budget(provider, model, context_budget);
    let tail_target = route_tail_target_tokens(provider, model, context_budget);
    // Bound the fork so the summary request itself fits the summarizer window.
    // `/compact` is normally run when the context is already near full, so an
    // unbounded fork is rejected by the provider and the whole command fails.
    // Oversized tool outputs are pruned first so large dumps do not evict
    // whole protocol groups from the fork.
    let pruned_history = prune_oversized_tool_outputs(history);
    let summary_source =
        bound_history_for_summarization(&pruned_history, &effective_config.summary_prompt, history_budget);
    let summary = generate_local_summary_with_retry(
        provider,
        model,
        &pruned_history,
        &summary_source,
        &effective_config.summary_prompt,
        history_budget,
        options,
        parent,
    )
    .await?;

    Ok(bound_compacted_history_to_context(
        build_summary_compacted_history(history, summary, &effective_config, true, tail_target),
        provider,
        model,
        context_budget,
    ))
}

/// Generate a summary request, retrying with progressively halved input budgets
/// when the provider rejects the request as over context capacity.
///
/// Token estimates are heuristic, so a bounded input can still overflow the
/// summarizer window. Bounded retries recover instead of failing the whole
/// `/compact` command; any other error (or exhausted retries) propagates.
/// `make_request` rebuilds the request shape (cache-safe fork or single-shot
/// band prompt) from the given source messages so every local summarization
/// path shares one retry contract.
async fn generate_summary_with_capacity_retry(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    instructions: &str,
    first_source: &[Message],
    history_budget: Option<usize>,
    max_overflow_retries: u32,
    error_context: &'static str,
    make_request: impl Fn(&[Message]) -> LLMRequest,
) -> Result<String> {
    let first_tokens = first_source.iter().map(Message::estimate_tokens).sum::<usize>();
    let mut current_source: &[Message] = first_source;
    let mut current_tokens = first_tokens;
    let mut current_budget = history_budget;
    let mut remaining_retries = max_overflow_retries;
    let mut retry_storage: Vec<Message>;
    loop {
        match collect_single_response(provider, make_request(current_source)).await {
            Ok(response) => {
                let summary = response.content.unwrap_or_default().trim().to_string();
                if summary.is_empty() {
                    return Err(anyhow::anyhow!(
                        "{error_context} for {} / {}: provider returned an empty summary (input_tokens={current_tokens}, budget={current_budget:?})",
                        provider.name(),
                        model,
                    ));
                }
                return Ok(summary);
            }
            Err(error) if is_context_capacity_message(&error.to_string()) && remaining_retries > 0 => {
                // With an unknown budget the bound is verbatim, so halving
                // `None` would resend the identical fork. Derive a fallback
                // from the failing attempt so the retry can still shrink.
                current_budget = match current_budget {
                    Some(budget) => Some((budget / 2).max(4)),
                    None => Some((current_tokens / 2).max(4)),
                };
                retry_storage = bound_history_for_summarization(history, instructions, current_budget);
                // A retry only helps when it actually shrinks the input: with
                // an already-fitting history it would resend the identical
                // request, so propagate the original failure instead.
                let retry_tokens = retry_storage.iter().map(Message::estimate_tokens).sum::<usize>();
                if retry_tokens >= current_tokens {
                    return Err(anyhow::Error::from(error).context(format!(
                        "{error_context} for {} / {} (input_tokens={current_tokens}, budget={current_budget:?})",
                        provider.name(),
                        model,
                    )));
                }
                tracing::warn!(
                    provider = provider.name(),
                    model,
                    input_tokens = current_tokens,
                    retry_tokens,
                    budget = ?current_budget,
                    error = ?error,
                    "{error_context}: input exceeded context capacity; retrying with a halved input budget"
                );
                current_source = &retry_storage;
                current_tokens = retry_tokens;
                remaining_retries -= 1;
            }
            Err(error) => {
                return Err(anyhow::Error::from(error).context(format!(
                    "{error_context} for {} / {} (input_tokens={current_tokens}, budget={current_budget:?})",
                    provider.name(),
                    model,
                )));
            }
        }
    }
}

/// Generate a cache-safe fork summary with the shared capacity-retry contract.
async fn generate_local_summary_with_retry(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    summary_source: &[Message],
    instructions: &str,
    history_budget: Option<usize>,
    options: &ManualCompactionOptions,
    parent: Option<&CompactionParentContext>,
) -> Result<String> {
    generate_summary_with_capacity_retry(
        provider,
        model,
        history,
        instructions,
        summary_source,
        history_budget,
        CompactionRoutePolicy::resolve(provider.name(), model).max_overflow_retries,
        "Failed to generate compaction summary",
        |source| {
            compaction_summary_request(
                model,
                source,
                instructions,
                options.max_output_tokens,
                options.reasoning_effort,
                options.verbosity,
                parent,
            )
        },
    )
    .await
}

/// Intro framing for the hierarchical abstract band (oldest third).
const ABSTRACT_BAND_INTRO: &str =
    "In 1-2 sentences, what was the overall goal and major progress in this portion of the conversation?\n\n";

/// Build a single-shot band summary request: one user message carrying the
/// pre-rendered band prompt, with the parent system/tools prefix reused.
fn band_summary_request(
    model: &str,
    prompt: String,
    max_tokens: Option<u32>,
    options: &ManualCompactionOptions,
    parent: Option<&CompactionParentContext>,
) -> LLMRequest {
    LLMRequest {
        messages: Arc::new(vec![Message::user(prompt)]),
        model: model.to_string(),
        system_prompt: parent.and_then(|parent| parent.system_prompt.clone()),
        tools: parent.and_then(|parent| parent.tools.clone()).filter(|tools| !tools.is_empty()),
        tool_choice: Some(ToolChoice::none()),
        max_tokens,
        reasoning_effort: options.reasoning_effort,
        verbosity: options.verbosity,
        ..Default::default()
    }
}
/// Hierarchical local summarization: abstract + detail + verbatim pyramid.
///
/// Splits the history into three bands and summarizes each with a different
/// compression target:
/// - **Abstract** (oldest third): 1-2 sentence overview
/// - **Detail** (middle third): paragraph-level summary
/// - **Verbatim** (newest third): kept as-is plus importance-weighted retained messages
///
/// This follows the hierarchical summarization strategy from the context window
/// management literature: recent turns verbatim, older turns as paragraph
/// summaries, oldest turns as a single abstract.
///
/// Band requests carry only their band's messages (not the full history
/// prefix), so message-prefix reuse does not apply; the parent system/tools
/// prefix is still reused, and `tool_choice` stays `none` so neither pass can
/// spend the compaction budget on tool calls.
async fn summarize_locally_hierarchical(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    config: &CompactionConfig,
    options: &ManualCompactionOptions,
    context_budget: Option<usize>,
    parent: Option<&CompactionParentContext>,
) -> Result<Vec<Message>> {
    let effective_config = context_bounded_compaction_config(
        provider,
        model,
        history,
        &config.clone().with_manual_overrides(options),
        context_budget,
    );
    let (summary_history, _) =
        split_continuity_history_with_target(history, route_tail_target_tokens(provider, model, context_budget));

    // Split history into three bands at roughly equal thirds. Each band is
    // bounded to the summarizer window so a near-full context cannot overflow
    // the band request itself (same class of bug as the flat-path fork).
    // Oversized tool outputs are pruned first so large dumps do not evict
    // whole protocol groups from a band.
    let total = summary_history.len();
    let band_size = total / 3;
    let abstract_end = band_size;
    let detail_end = band_size * 2;
    let history_budget = compaction_history_budget(provider, model, context_budget);
    let max_overflow_retries = CompactionRoutePolicy::resolve(provider.name(), model).max_overflow_retries;

    // Band 1 (oldest): compress into 1-2 sentence abstract.
    let pruned_abstract = prune_oversized_tool_outputs(&summary_history[..abstract_end]);
    let abstract_band = bound_history_for_summarization(&pruned_abstract, "", history_budget);
    let abstract_summary = generate_summary_with_capacity_retry(
        provider,
        model,
        &pruned_abstract,
        "",
        &abstract_band,
        history_budget,
        max_overflow_retries,
        "Failed to generate abstract summary",
        |band| {
            band_summary_request(
                model,
                format!("{ABSTRACT_BAND_INTRO}{}", build_summary_prompt(band, "")),
                Some(150),
                options,
                parent,
            )
        },
    )
    .await?;

    // Band 2 (middle): paragraph-level summary using the full summary prompt.
    let pruned_detail = prune_oversized_tool_outputs(&summary_history[abstract_end..detail_end]);
    let detail_band = bound_history_for_summarization(&pruned_detail, &effective_config.summary_prompt, history_budget);
    let detail_summary = generate_summary_with_capacity_retry(
        provider,
        model,
        &pruned_detail,
        &effective_config.summary_prompt,
        &detail_band,
        history_budget,
        max_overflow_retries,
        "Failed to generate detail summary",
        |band| {
            band_summary_request(
                model,
                build_summary_prompt(band, &effective_config.summary_prompt),
                options.max_output_tokens,
                options,
                parent,
            )
        },
    )
    .await?;

    // Band 3 (newest): retain verbatim via the bounded protocol tail.
    let recent_band = &summary_history[detail_end..];
    let retained = collect_retained_user_messages(
        recent_band,
        effective_config.retained_user_message_tokens,
        effective_config.retained_user_messages,
    );

    // Assemble: [abstract, detail, ...retained_recent, ...continuity_tail]
    let mut new_history = Vec::with_capacity(2 + retained.len());
    new_history.push(Message::system(format!("{ABSTRACT_PREFIX}{abstract_summary}")));
    new_history.push(Message::system(format!("{DETAIL_PREFIX}{detail_summary}")));
    new_history.extend(retained);
    // Live compaction: retain the most recent turn verbatim for continuity.
    for message in continuity_tail_with_target(history, route_tail_target_tokens(provider, model, context_budget)) {
        new_history.push(message.clone());
    }
    Ok(bound_compacted_history_to_context(new_history, provider, model, context_budget))
}

pub(crate) fn build_summary_prompt(history: &[Message], instructions: &str) -> String {
    // Pre-size for the header plus every (non-empty) message body, avoiding
    // repeated reallocations while the summary prompt is assembled.
    let estimated_len =
        instructions.len() + history.iter().map(|m| m.content.as_text().len()).sum::<usize>() + history.len() * 16;
    let mut formatted = String::with_capacity(estimated_len);
    let now: DateTime<Utc> = Utc::now();
    let _ = writeln!(&mut formatted, "Summary requested at {}.\n{}", now.to_rfc3339(), instructions);

    for message in history {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        let content = message.content.as_text();
        if content.trim().is_empty() {
            continue;
        }
        let _ = writeln!(&mut formatted, "\n[{}]\n{}", role, content.trim());
    }

    formatted
}

fn compaction_history_budget(provider: &dyn LLMProvider, model: &str, context_budget: Option<usize>) -> Option<usize> {
    let context_size = context_budget
        .filter(|value| *value > 0)
        .unwrap_or_else(|| provider.effective_context_size(model));
    (context_size > 0).then(|| {
        context_size
            .saturating_sub(context_size / COMPACTION_CONTEXT_OVERHEAD_FRACTION_DENOMINATOR)
            .saturating_sub(COMPACTION_CONTEXT_FIXED_OVERHEAD_TOKENS)
    })
}

fn context_bounded_compaction_config(
    provider: &dyn LLMProvider,
    model: &str,
    history: &[Message],
    config: &CompactionConfig,
    context_budget: Option<usize>,
) -> CompactionConfig {
    let mut bounded = config.clone();
    if let Some(history_budget) = compaction_history_budget(provider, model, context_budget) {
        let tail_target = route_tail_target_tokens(provider, model, context_budget);
        let continuity_tokens = continuity_tail_with_target(history, tail_target)
            .iter()
            .map(Message::estimate_tokens)
            .sum::<usize>();
        bounded.retained_user_message_tokens = bounded
            .retained_user_message_tokens
            .min(history_budget.saturating_sub(continuity_tokens));
    }
    bounded
}

/// Bound a locally managed compacted history to the resolved context budget,
/// including any caller-supplied session ceiling. Standalone native responses
/// must bypass this helper: their retained items and opaque continuation state
/// are the provider's canonical next context window.
pub fn bound_compacted_history_to_context(
    compacted: Vec<Message>,
    provider: &dyn LLMProvider,
    model: &str,
    context_budget: Option<usize>,
) -> Vec<Message> {
    let Some(history_budget) = compaction_history_budget(provider, model, context_budget) else {
        return compacted;
    };
    if compacted.iter().map(Message::estimate_tokens).sum::<usize>() <= history_budget {
        return compacted;
    }

    let Some((tail_start, tail_end, _)) =
        continuity_tail_selection_with_target(&compacted, route_tail_target_tokens(provider, model, context_budget))
    else {
        return compacted
            .into_iter()
            .scan(history_budget, |remaining, message| {
                if *remaining < 4 {
                    return None;
                }
                let bounded = bounded_message_preview(&message, *remaining);
                let used = bounded.estimate_tokens();
                if used > *remaining {
                    return None;
                }
                *remaining -= used;
                Some(bounded)
            })
            .collect();
    };

    let raw_tail = &compacted[tail_start..tail_end];
    let raw_tail_tokens = raw_tail.iter().map(Message::estimate_tokens).sum::<usize>();
    let tail = if raw_tail_tokens > history_budget {
        bounded_protocol_group(raw_tail, history_budget.max(4))
    } else {
        raw_tail.to_vec()
    };
    let tail_tokens = tail.iter().map(Message::estimate_tokens).sum::<usize>();
    let mut remaining = history_budget.saturating_sub(tail_tokens);
    let mut bounded_prefix = Vec::new();

    for (index, message) in compacted[..tail_start].iter().enumerate() {
        if remaining < 4 {
            break;
        }
        // Keep the leading summary/envelope message readable, but cap it so
        // the newest complete protocol groups retain priority.
        let message_budget = if index == 0 { remaining.min(4_096) } else { remaining };
        let bounded = bounded_message_preview(message, message_budget);
        let used = bounded.estimate_tokens();
        if used > remaining {
            if index == 0 {
                let fallback = Message::system(truncate_to_token_limit(
                    message.content.as_text().as_ref(),
                    remaining.saturating_sub(4),
                ));
                let fallback_tokens = fallback.estimate_tokens();
                if fallback_tokens <= remaining {
                    bounded_prefix.push(fallback);
                }
            }
            break;
        }
        remaining -= used;
        bounded_prefix.push(bounded);
    }

    bounded_prefix.extend(tail);
    bounded_prefix
}

pub(crate) fn build_local_compacted_history(
    history: &[Message],
    summary: &str,
    retained_user_message_tokens: usize,
    retained_user_messages: usize,
    include_continuity_tail: bool,
    tail_target_tokens: usize,
) -> Vec<Message> {
    let (retention_history, continuity) = if include_continuity_tail {
        split_continuity_history_with_target(history, tail_target_tokens)
    } else {
        (history, Vec::new())
    };
    let retained_users =
        collect_retained_user_messages(retention_history, retained_user_message_tokens, retained_user_messages);
    let mut new_history = Vec::with_capacity(retained_users.len().saturating_add(1));
    new_history.push(Message::system(format!("{SUMMARY_PREFIX}{}", summary.trim())));
    new_history.extend(retained_users);

    // Continuity anchor: retain the newest complete protocol groups verbatim
    // within the route tail budget. Duplicate message text is still valid
    // across turns, so preserve the sequence by index rather than deduplicating
    // on role/content.
    if include_continuity_tail {
        for message in continuity {
            new_history.push(message);
        }
    }
    new_history
}

/// Return the newest complete user-anchored protocol groups that fit the fixed
/// continuity budget. The returned messages are owned because an oversized
/// individual group may need a bounded preview.
#[cfg(test)]
fn continuity_tail(history: &[Message]) -> Vec<Message> {
    continuity_tail_with_target(history, CONTINUITY_TAIL_TARGET_TOKENS)
}

/// [`continuity_tail`] with a route-scaled budget so small windows keep a
/// usable summary instead of a tail-only history.
fn continuity_tail_with_target(history: &[Message], tail_target_tokens: usize) -> Vec<Message> {
    let Some((start, end, oversized)) = continuity_tail_selection_with_target(history, tail_target_tokens) else {
        return Vec::new();
    };
    if oversized {
        bounded_protocol_group(&history[start..end], tail_target_tokens.max(4))
    } else {
        history[start..end].to_vec()
    }
}

/// Find one contiguous suffix of complete protocol groups. An incomplete
/// trailing assistant tool-call group is truncated at the assistant message,
/// preserving its user anchor while excluding the invalid protocol suffix.
fn continuity_tail_selection_with_target(
    history: &[Message],
    tail_target_tokens: usize,
) -> Option<(usize, usize, bool)> {
    if history.is_empty() {
        return None;
    }
    let group_starts: Vec<usize> = history
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == MessageRole::User).then_some(index))
        .collect();
    let last_group_index = group_starts.len().saturating_sub(1);
    let last_group_start = *group_starts.last()?;
    let last_group_end = history.len();
    let last_group = &history[last_group_start..last_group_end];
    let last_group_prefix_end = complete_protocol_group_prefix(last_group);
    let tail_end = last_group_start.saturating_add(last_group_prefix_end);

    let mut selected_start = None;
    let mut estimated_tokens = 0usize;
    for group_index in (0..group_starts.len()).rev() {
        let start = group_starts[group_index];
        let natural_end = group_starts.get(group_index + 1).copied().unwrap_or(history.len());
        let end = if group_index == last_group_index {
            tail_end
        } else {
            natural_end
        };
        if start == end {
            continue;
        }
        let group = &history[start..end];
        if group_index != last_group_index && !protocol_group_is_complete(group) {
            break;
        }
        let group_tokens = group.iter().map(Message::estimate_tokens).sum::<usize>();
        if selected_start.is_none() && group_tokens > tail_target_tokens {
            return Some((start, end, true));
        }
        if estimated_tokens.saturating_add(group_tokens) > tail_target_tokens {
            break;
        }
        selected_start = Some(start);
        estimated_tokens += group_tokens;
    }

    selected_start.map(|start| (start, tail_end, false))
}

fn protocol_group_is_complete(group: &[Message]) -> bool {
    complete_protocol_group_prefix(group) == group.len()
}

/// Return the length of the valid protocol prefix in a user-anchored group.
/// When a tool call is still pending, the prefix ends before the assistant
/// message that introduced it. This handles parallel tool calls and prevents
/// a partially answered group from entering the continuity tail.
fn complete_protocol_group_prefix(group: &[Message]) -> usize {
    if group.first().is_none_or(|message| message.role != MessageRole::User) {
        return 0;
    }

    let mut pending_tool_call_ids: Vec<&str> = Vec::new();
    let mut pending_origin = None;

    for (index, message) in group.iter().enumerate() {
        if !pending_tool_call_ids.is_empty() {
            if message.role != MessageRole::Tool {
                return pending_origin.unwrap_or(index);
            }
            let Some(tool_call_id) = message.tool_call_id.as_deref() else {
                return pending_origin.unwrap_or(index);
            };
            let Some(pending_index) = pending_tool_call_ids.iter().position(|pending_id| *pending_id == tool_call_id)
            else {
                return pending_origin.unwrap_or(index);
            };
            pending_tool_call_ids.swap_remove(pending_index);
            if pending_tool_call_ids.is_empty() {
                pending_origin = None;
            }
            continue;
        }

        // Older persisted histories may contain a tool result without the
        // assistant call metadata that introduced it. Preserve that legacy
        // pair rather than discarding an otherwise complete protocol group.
        if message.role == MessageRole::Tool {
            continue;
        }

        if message.role == MessageRole::Assistant
            && let Some(tool_calls) = message.tool_calls.as_ref()
            && !tool_calls.is_empty()
        {
            for (call_index, call) in tool_calls.iter().enumerate() {
                if call.id.is_empty() || tool_calls[..call_index].iter().any(|prior| prior.id == call.id) {
                    return index;
                }
            }
            pending_origin = Some(index);
            pending_tool_call_ids.extend(tool_calls.iter().map(|call| call.id.as_str()));
        }
    }

    pending_origin.unwrap_or(group.len())
}

fn split_continuity_history_with_target(history: &[Message], tail_target_tokens: usize) -> (&[Message], Vec<Message>) {
    let Some((tail_start, _, _)) = continuity_tail_selection_with_target(history, tail_target_tokens) else {
        return (history, Vec::new());
    };
    (&history[..tail_start], continuity_tail_with_target(history, tail_target_tokens))
}

fn response_compaction_detail(response: &LLMResponse) -> Option<Value> {
    response.reasoning_details.as_ref()?.iter().find_map(|detail| {
        let parsed = serde_json::from_str::<Value>(detail).ok()?;
        let parsed = match parsed {
            Value::String(serialized) => serde_json::from_str::<Value>(&serialized).ok()?,
            value => value,
        };
        let summary = parsed
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        (parsed.get("type").and_then(Value::as_str) == Some("compaction") && summary.is_some()).then_some(parsed)
    })
}

fn build_summary_compacted_history(
    history: &[Message],
    summary: impl AsRef<str>,
    config: &CompactionConfig,
    include_continuity_tail: bool,
    tail_target_tokens: usize,
) -> Vec<Message> {
    build_compacted_history_with_leading(
        history,
        Message::system(format!("{SUMMARY_PREFIX}{}", summary.as_ref().trim())),
        config,
        include_continuity_tail,
        tail_target_tokens,
        false,
    )
}

fn build_provider_compacted_history(
    history: &[Message],
    compaction_detail: Value,
    config: &CompactionConfig,
    include_continuity_tail: bool,
    tail_target_tokens: usize,
) -> Vec<Message> {
    let signed_compaction = compaction_detail
        .get("signature")
        .and_then(Value::as_str)
        .is_some_and(|signature| !signature.trim().is_empty());
    let history_without_provider_compaction = history
        .iter()
        .cloned()
        .filter_map(strip_provider_compaction_detail)
        .collect::<Vec<_>>();
    build_compacted_history_with_leading(
        &history_without_provider_compaction,
        Message::assistant(String::new()).with_reasoning_details(Some(vec![compaction_detail])),
        config,
        include_continuity_tail,
        tail_target_tokens,
        signed_compaction,
    )
}

fn build_compacted_history_with_leading(
    history: &[Message],
    leading: Message,
    config: &CompactionConfig,
    include_continuity_tail: bool,
    tail_target_tokens: usize,
    strip_pre_compaction_thinking: bool,
) -> Vec<Message> {
    let (retention_history, continuity) = split_continuity_history_with_target(history, tail_target_tokens);
    let retained_users = collect_retained_user_messages(
        retention_history,
        config.retained_user_message_tokens,
        config.retained_user_messages,
    );
    let retained_users = if strip_pre_compaction_thinking {
        retained_users.into_iter().map(strip_anthropic_thinking_details).collect()
    } else {
        retained_users
    };
    let mut compacted = Vec::with_capacity(retained_users.len().saturating_add(1));
    compacted.push(leading);
    compacted.extend(retained_users);
    if include_continuity_tail {
        for message in continuity {
            compacted.push(if strip_pre_compaction_thinking {
                strip_anthropic_thinking_details(message)
            } else {
                message
            });
        }
    }
    compacted
}

fn strip_anthropic_thinking_details(mut message: Message) -> Message {
    if message.role != MessageRole::Assistant {
        return message;
    }

    let Some(details) = message.reasoning_details.take() else {
        return message;
    };
    let retained = details
        .into_iter()
        .filter(|detail| !is_reasoning_detail_type(detail, "thinking"))
        .filter(|detail| !is_reasoning_detail_type(detail, "redacted_thinking"))
        .collect::<Vec<_>>();
    message.reasoning_details = (!retained.is_empty()).then_some(retained);
    message
}

fn strip_provider_compaction_detail(mut message: Message) -> Option<Message> {
    if message.role != MessageRole::Assistant {
        return Some(message);
    }

    let Some(details) = message.reasoning_details.take() else {
        return Some(message);
    };
    let retained = details
        .into_iter()
        .filter(|detail| !is_compaction_detail(detail))
        .collect::<Vec<_>>();
    message.reasoning_details = (!retained.is_empty()).then_some(retained);

    let has_content = !message.content.trim().is_empty();
    let has_reasoning = message.reasoning.as_ref().is_some_and(|reasoning| !reasoning.trim().is_empty());
    let has_tool_calls = message.tool_calls.as_ref().is_some_and(|tool_calls| !tool_calls.is_empty());
    let has_other_metadata = message.tool_call_id.is_some()
        || message.phase.is_some()
        || message.origin_tool.is_some()
        || message.metadata.is_some()
        || message.clear_at.is_some();
    (has_content || has_reasoning || message.reasoning_details.is_some() || has_tool_calls || has_other_metadata)
        .then_some(message)
}

fn is_compaction_detail(detail: &Value) -> bool {
    is_reasoning_detail_type(detail, "compaction")
}

fn is_reasoning_detail_type(detail: &Value, expected_type: &str) -> bool {
    let value = match detail {
        Value::String(serialized) => serde_json::from_str::<Value>(serialized).ok(),
        value => Some(value.clone()),
    };
    value.as_ref().and_then(|value| value.get("type")).and_then(Value::as_str) == Some(expected_type)
}

fn bounded_protocol_group(group: &[Message], token_budget: usize) -> Vec<Message> {
    let per_message_budget = (token_budget / group.len().max(1)).max(4);
    group
        .iter()
        .map(|message| bounded_message_preview(message, per_message_budget))
        .collect()
}

/// Per-message cap applied to tool outputs in summarizer inputs. Large dumps
/// (file reads, command output) otherwise evict whole protocol groups from the
/// bounded fork. Call IDs and pairing metadata are preserved by
/// [`bounded_message_preview`], so trimmed groups stay protocol-valid.
const TOOL_RESULT_PRUNE_TARGET_TOKENS: usize = 4_096;

/// Trim oversized tool outputs to previews before bounding the summarizer
/// input. Only `Tool` messages are touched; everything else passes through
/// verbatim (cloned). Under the cap this is a pure copy.
fn prune_oversized_tool_outputs(history: &[Message]) -> Vec<Message> {
    history
        .iter()
        .map(|message| {
            if message.role == MessageRole::Tool {
                bounded_message_preview(message, TOOL_RESULT_PRUNE_TARGET_TOKENS)
            } else {
                message.clone()
            }
        })
        .collect()
}

fn bounded_message_preview(message: &Message, token_budget: usize) -> Message {
    if message.estimate_tokens() <= token_budget {
        return message.clone();
    }
    let mut preview = message.clone();
    let available = token_budget.saturating_sub(4);
    let text = truncate_to_token_limit(message.content.as_text().as_ref(), available);
    preview.content = MessageContent::Text(text);

    // Tool-call arguments and provider metadata are part of the message's
    // token footprint even when `content` is empty. Preserve call IDs and
    // function names so the protocol group remains correlatable, but replace
    // oversized argument payloads with valid minimal JSON and drop optional
    // reasoning/signature metadata.
    preview.reasoning = preview
        .reasoning
        .map(|reasoning| truncate_to_token_limit(&reasoning, available.min(1_024)));
    preview.reasoning_details = None;
    preview.metadata = None;
    preview.origin_tool = None;
    if let Some(tool_calls) = preview.tool_calls.as_mut() {
        for call in tool_calls {
            if let Some(function) = call.function.as_mut()
                && function.arguments.len() > available.saturating_mul(4)
            {
                function.arguments = "{}".to_string();
            }
            if let Some(text) = call.text.as_mut() {
                *text = truncate_to_token_limit(text, available.min(1_024));
            }
            call.thought_signature = None;
        }
    }

    if preview.estimate_tokens() > token_budget {
        preview.content = MessageContent::Text(String::new());
        preview.reasoning = None;
        preview.reasoning_details = None;
        if let Some(tool_calls) = preview.tool_calls.as_mut() {
            for call in tool_calls {
                if let Some(function) = call.function.as_mut() {
                    function.arguments = "{}".to_string();
                }
                call.text = None;
                call.thought_signature = None;
            }
        }
    }
    preview
}

fn collect_retained_user_messages(history: &[Message], token_budget: usize, max_messages: usize) -> Vec<Message> {
    if token_budget == 0 || max_messages == 0 {
        return Vec::new();
    }

    // Phase 1: select up to `max_messages` user messages, scored by importance.
    let total = history.len();
    let mut user_scored: Vec<(usize, f64, &Message)> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == MessageRole::User && !m.content.trim().is_empty())
        .map(|(i, m)| {
            let score = score_message(m, i, total);
            (i, score, m)
        })
        .collect();
    user_scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut selected: Vec<(usize, Message)> = Vec::with_capacity(max_messages.min(history.len()));
    let mut remaining = token_budget;

    for (original_idx, _score, message) in &user_scored {
        if selected.len() >= max_messages {
            break;
        }
        let estimated = message.estimate_tokens();
        if estimated <= remaining {
            selected.push((*original_idx, (*message).clone()));
            remaining = remaining.saturating_sub(estimated);
            continue;
        }
        if let Some(truncated) = truncate_user_message(message, remaining) {
            selected.push((*original_idx, truncated));
        }
        break;
    }

    // Phase 2: if budget remains, add high-value non-user messages (tool
    // results, assistant tool calls) that fit within the remaining capacity.
    if selected.len() < max_messages && remaining > 0 {
        let mut non_user_scored: Vec<(usize, f64, &Message)> = history
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role != MessageRole::User && m.role != MessageRole::System && is_retainable_message(m))
            .map(|(i, m)| {
                let score = score_message(m, i, total);
                (i, score, m)
            })
            .collect();
        non_user_scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        for (original_idx, _score, message) in &non_user_scored {
            if selected.len() >= max_messages {
                break;
            }
            let estimated = message.estimate_tokens();
            if estimated <= remaining {
                selected.push((*original_idx, (*message).clone()));
                remaining = remaining.saturating_sub(estimated);
            }
        }
    }

    // Re-sort by original conversation order, then enforce tool-call/turn
    // coherence so the compacted history is valid to send back to a provider.
    selected.sort_by_key(|(idx, _)| *idx);
    coherence_tool_call_pairs(history, &selected)
        .into_iter()
        .map(|(_, msg)| msg)
        .collect()
}

/// Keep retained tool-call turns internally consistent.
///
/// A `Tool` message references a tool call the model must have seen, and an
/// `Assistant` message that still carries `tool_calls` must be followed by the
/// results those calls produced. Sending either without its counterpart is
/// invalid: providers reject unmatched tool calls, and orphaned tool results
/// reference a call the model never observed. This pass:
///
/// - **Force-keeps** the `Tool` messages immediately following any retained
///   `Assistant` that carries `tool_calls`, so the model observes each call's
///   return value. A complete turn ends with its results, which survive even
///   if they push past the soft `max_messages` cap.
/// - **Drops** a retained `Tool` message whose calling `Assistant` (the message
///   directly before it in `history`) was *not* retained — an orphaned result
///   the model cannot reconcile.
///
/// Tool results that follow a plain `Assistant` (no `tool_calls`) are ordinary
/// turn output and are kept exactly as selected.
fn coherence_tool_call_pairs(history: &[Message], selected: &[(usize, Message)]) -> Vec<(usize, Message)> {
    let mut keep: std::collections::HashSet<usize> = selected.iter().map(|(i, _)| *i).collect();

    for (idx, msg) in selected {
        if msg.role == MessageRole::Assistant && msg.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty()) {
            let mut j = *idx + 1;
            while let Some(next) = history.get(j) {
                if next.role == MessageRole::Tool {
                    keep.insert(j);
                    j += 1;
                } else {
                    break;
                }
            }
        }
    }

    // Emit every index in `keep` in original history order, dropping orphaned
    // Tool results whose calling Assistant turn was not retained.
    let mut indices: Vec<usize> = keep.iter().copied().collect();
    indices.sort_unstable();

    indices
        .into_iter()
        .filter(|idx| {
            let msg = &history[*idx];
            if msg.role != MessageRole::Tool {
                return true;
            }
            // Walk backward through this contiguous result run to decide
            // coherence against the calling assistant turn.
            let mut cursor = *idx;
            loop {
                match history.get(cursor) {
                    Some(m) if m.role == MessageRole::Tool => {
                        cursor = match cursor.checked_sub(1) {
                            Some(c) => c,
                            None => break,
                        };
                    }
                    Some(m)
                        if m.role == MessageRole::Assistant && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty()) =>
                    {
                        // Reached the calling assistant: coherent only if it
                        // was retained.
                        return keep.contains(&cursor);
                    }
                    _ => {
                        // Plain assistant or boundary: ordinary output.
                        return true;
                    }
                }
            }
            true
        })
        .map(|idx| (idx, history[idx].clone()))
        .collect()
}

/// Score a message for importance-weighted retention during compaction.
///
/// Uses a weighted combination of content importance and recency:
/// - Messages containing errors, corrections, or tool results score higher
/// - Recent messages get a recency bonus
/// - Assistant messages with tool calls are moderately important
fn score_message(message: &Message, index: usize, total: usize) -> f64 {
    let content = message.content.as_text();
    let content_lower = content.to_lowercase();

    // Importance weight based on content signals.
    let importance = match message.role {
        MessageRole::User => {
            if contains_error_signal(&content_lower) {
                3.0
            } else if contains_correction_signal(&content_lower) {
                2.5
            } else {
                1.0
            }
        }
        MessageRole::Tool => {
            // Tool results contain factual data the model may need.
            2.0
        }
        MessageRole::Assistant => {
            if message.tool_calls.is_some() {
                // Assistant messages with tool calls show action taken.
                0.5
            } else {
                0.1
            }
        }
        MessageRole::System => 0.0,
    };

    // Recency bonus: linear from 0.0 (oldest) to 1.0 (newest).
    let recency = if total > 0 { index as f64 / total as f64 } else { 0.0 };

    importance + recency
}

/// Check if content contains error or failure signals.
fn contains_error_signal(content: &str) -> bool {
    content.contains("error")
        || content.contains("failed")
        || content.contains("failure")
        || content.contains("panic")
        || content.contains("bug")
        || content.contains("broken")
        || content.contains("regression")
}

/// Check if content contains user correction signals.
fn contains_correction_signal(content: &str) -> bool {
    content.contains("no,")
        || content.contains("wrong")
        || content.contains("actually")
        || content.contains("fix")
        || content.contains("instead")
        || content.contains("should be")
        || content.contains("don't")
}

/// Whether a message is worth retaining during compaction.
fn is_retainable_message(message: &Message) -> bool {
    match message.role {
        MessageRole::User => !message.content.trim().is_empty(),
        MessageRole::Tool => !message.content.trim().is_empty(),
        MessageRole::Assistant => {
            // Retain assistant messages that contain tool calls (action history).
            message.tool_calls.is_some()
        }
        MessageRole::System => false,
    }
}

fn truncate_user_message(message: &Message, token_budget: usize) -> Option<Message> {
    if token_budget <= 4 {
        return None;
    }

    let available_content_tokens = token_budget.saturating_sub(4);
    let truncated = truncate_to_token_limit(message.content.as_text().as_ref(), available_content_tokens);
    let trimmed = truncated.trim();
    if trimmed.is_empty() {
        return None;
    }

    Some(Message::user(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{
        CompactionConfig, ManualCompactionOptions, compact_history, compact_history_manual,
        compact_history_manual_with_budget, continuity_tail, manual_compaction_strategy,
    };
    use crate::config::types::{ReasoningEffortLevel, VerbosityLevel};
    use crate::exec::events::CompactionMode;
    use crate::llm::provider::{
        LLMError, LLMNormalizedStream, LLMProvider, LLMRequest, LLMResponse, Message, MessageRole,
        NormalizedStreamEvent, ResponsesCompactionOptions,
    };
    use async_trait::async_trait;
    use futures::stream;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use vtcode_commons::llm::{FinishReason, ToolCall};

    struct StubProvider;

    struct NativeCompactionProvider;

    /// Provider that opts into the standalone manual-compaction path
    /// (`supports_manual_openai_compaction -> true`), e.g. OpenAI `/responses/compact`.
    struct ManualStandaloneProvider {
        last_options: Mutex<Option<ResponsesCompactionOptions>>,
        output: Vec<Message>,
    }

    /// Inline-compaction-capable provider (`supports_responses_compaction -> true`,
    /// `supports_manual_openai_compaction -> false`), e.g. Anthropic `compact_20260112`.
    /// Returns a `Pause` finish with a compaction block so the inline path succeeds.
    struct InlinePauseProvider {
        last_request: Mutex<Option<LLMRequest>>,
        include_compaction_detail: bool,
    }

    /// Inline provider that also reports `supports_context_edits`, e.g. Anthropic
    /// on a compaction-capable model. The inline request must carry the full
    /// clearing ladder (thinking, tool uses, compact) in documented order.
    struct ContextEditsInlineProvider {
        last_request: Mutex<Option<LLMRequest>>,
    }

    /// Local summarizer that rejects the first summary request with a
    /// context-capacity error and succeeds on retry. Exercises the halved-budget
    /// overflow retry in `summarize_locally`.
    struct CapacityFailOnceProvider {
        attempts: Mutex<usize>,
        request_tokens: Mutex<Vec<usize>>,
        /// Number of leading requests to reject before succeeding.
        failures: usize,
    }

    /// Local summarizer that only supports normalized streaming. Its
    /// non-streaming method deliberately returns an error so compaction tests
    /// prove the capability-aware collection path is used.
    struct StreamingOnlyCompactionProvider {
        generate_calls: Mutex<usize>,
        stream_calls: Mutex<usize>,
        stream_modes: Mutex<Vec<bool>>,
    }

    /// Unknown-window summarizer (`effective_context_size == 0`) that rejects
    /// the first summary request with a capacity error. Exercises the
    /// `None`-budget fallback so an unbounded first fork can still shrink.
    struct UnknownBudgetFailOnceProvider {
        attempts: Mutex<usize>,
        request_tokens: Mutex<Vec<usize>>,
    }

    /// Local summarizer that returns an empty summary body. An empty
    /// compaction summary must fail with a diagnostic instead of producing
    /// an empty `Previous conversation summary:` history.
    struct EmptySummaryProvider;

    /// Capturing provider with no native support; used to assert the Local summary
    /// request carries the manual options.
    struct CapturingProvider {
        last_request: Mutex<Option<LLMRequest>>,
    }

    /// Local summarizer that exposes only the lower reasoning levels. This
    /// exercises the compaction boundary's strict block and explicit
    /// downgrade behavior before any hierarchical summary request is sent.
    struct LimitedReasoningProvider {
        last_request: Mutex<Option<LLMRequest>>,
    }

    /// Inline-dispatched provider whose inline `generate` rejects the Anthropic
    /// `compact_20260112` edit. Models providers that report
    /// `supports_responses_compaction` but are not Anthropic-style inline
    /// compactors; the dispatch must fall back to Local rather than aborting.
    struct InlineRejectingProvider;

    /// Models an OpenAI-compatible custom endpoint (non-`api.openai.com` host or
    /// `provider_key_override`): it exposes the Responses API
    /// (`supports_responses_compaction == true`) but neither the standalone
    /// `/responses/compact` endpoint nor Anthropic inline compaction. The dispatch
    /// must pick `Local` rather than misrouting it to `NativeInline` (which would
    /// send an Anthropic `compact_20260112` edit only to be rejected).
    struct CompatibleEndpointProvider;

    #[async_trait]
    impl LLMProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }

        async fn generate(&self, _request: LLMRequest) -> Result<LLMResponse, LLMError> {
            Ok(LLMResponse::new("stub-model", "summary"))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn effective_context_size(&self, _model: &str) -> usize {
            32_768
        }
    }

    #[async_trait]
    impl LLMProvider for NativeCompactionProvider {
        fn name(&self) -> &str {
            "native"
        }

        async fn generate(&self, _request: LLMRequest) -> Result<LLMResponse, LLMError> {
            Ok(LLMResponse::new("stub-model", "summary"))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn supports_responses_compaction(&self, _model: &str) -> bool {
            true
        }

        async fn compact_history(&self, _model: &str, _history: &[Message]) -> Result<Vec<Message>, LLMError> {
            Ok(vec![Message::system("provider compacted".to_string())])
        }
    }

    #[async_trait]
    impl LLMProvider for ManualStandaloneProvider {
        fn name(&self) -> &str {
            "manual-standalone"
        }

        async fn generate(&self, _request: LLMRequest) -> Result<LLMResponse, LLMError> {
            Ok(LLMResponse::new("stub-model", "summary"))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn supports_manual_openai_compaction(&self, _model: &str) -> bool {
            true
        }

        fn supports_reasoning_effort(&self, _model: &str) -> bool {
            true
        }

        fn supported_reasoning_efforts(&self, _model: &str) -> &'static [&'static str] {
            &["minimal", "low", "medium", "high", "xhigh", "max"]
        }

        async fn compact_history_with_options(
            &self,
            _model: &str,
            _history: &[Message],
            options: &ResponsesCompactionOptions,
        ) -> Result<Vec<Message>, LLMError> {
            *self.last_options.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(options.clone());
            Ok(self.output.clone())
        }
    }

    #[async_trait]
    impl LLMProvider for InlinePauseProvider {
        fn name(&self) -> &str {
            "inline-pause"
        }

        async fn generate(&self, request: LLMRequest) -> Result<LLMResponse, LLMError> {
            *self.last_request.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request);
            let mut response = LLMResponse::new("stub-model", "compacted by provider");
            response.finish_reason = FinishReason::Pause;
            response.compaction = Some("provider compaction summary".to_string());
            if self.include_compaction_detail {
                response.reasoning_details = Some(vec![
                    json!({
                        "type": "compaction",
                        "content": "provider compaction summary",
                        "signature": "provider-signature",
                        "opaque_extension": "preserve-me",
                    })
                    .to_string(),
                ]);
            }
            Ok(response)
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn supports_responses_compaction(&self, _model: &str) -> bool {
            true
        }

        fn supports_native_inline_compaction(&self, _model: &str) -> bool {
            true
        }

        fn effective_context_size(&self, _model: &str) -> usize {
            200_000
        }
    }

    #[async_trait]
    impl LLMProvider for ContextEditsInlineProvider {
        fn name(&self) -> &str {
            "context-edits-inline"
        }

        async fn generate(&self, request: LLMRequest) -> Result<LLMResponse, LLMError> {
            *self.last_request.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request);
            let mut response = LLMResponse::new("stub-model", "compacted by provider");
            response.finish_reason = FinishReason::Pause;
            response.compaction = Some("provider compaction summary".to_string());
            response.reasoning_details = Some(vec![
                json!({
                    "type": "compaction",
                    "content": "provider compaction summary",
                    "signature": "provider-signature",
                })
                .to_string(),
            ]);
            Ok(response)
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn supports_responses_compaction(&self, _model: &str) -> bool {
            true
        }

        fn supports_context_edits(&self, _model: &str) -> bool {
            true
        }

        fn supports_native_inline_compaction(&self, _model: &str) -> bool {
            true
        }

        fn effective_context_size(&self, _model: &str) -> usize {
            200_000
        }
    }

    #[async_trait]
    impl LLMProvider for CapacityFailOnceProvider {
        fn name(&self) -> &str {
            "capacity-fail-once"
        }

        async fn generate(&self, request: LLMRequest) -> Result<LLMResponse, LLMError> {
            let mut attempts = self.attempts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.request_tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.messages.iter().map(Message::estimate_tokens).sum());
            *attempts += 1;
            if *attempts <= self.failures {
                return Err(LLMError::Provider {
                    message: "maximum context length exceeded".to_string(),
                    metadata: None,
                });
            }
            Ok(LLMResponse::new("stub-model", "retried summary"))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn effective_context_size(&self, _model: &str) -> usize {
            200_000
        }
    }

    #[async_trait]
    impl LLMProvider for StreamingOnlyCompactionProvider {
        fn name(&self) -> &str {
            "streaming-only-compaction"
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        fn supports_non_streaming(&self, _model: &str) -> bool {
            false
        }

        async fn generate(&self, _request: LLMRequest) -> Result<LLMResponse, LLMError> {
            *self.generate_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            Err(LLMError::Provider {
                message: "streaming-only provider cannot generate non-streaming responses".to_string(),
                metadata: None,
            })
        }

        async fn stream_normalized(&self, request: LLMRequest) -> Result<LLMNormalizedStream, LLMError> {
            *self.stream_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            self.stream_modes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.stream);
            Ok(Box::pin(stream::iter(vec![Ok(NormalizedStreamEvent::Done {
                response: Box::new(LLMResponse::new("stub-model", "streamed summary")),
            })])))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }
    }

    #[async_trait]
    impl LLMProvider for UnknownBudgetFailOnceProvider {
        fn name(&self) -> &str {
            "unknown-budget-fail-once"
        }

        async fn generate(&self, request: LLMRequest) -> Result<LLMResponse, LLMError> {
            let mut attempts = self.attempts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.request_tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.messages.iter().map(Message::estimate_tokens).sum());
            *attempts += 1;
            if *attempts == 1 {
                return Err(LLMError::Provider {
                    message: "maximum context length exceeded".to_string(),
                    metadata: None,
                });
            }
            Ok(LLMResponse::new("stub-model", "retried summary"))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn effective_context_size(&self, _model: &str) -> usize {
            0
        }
    }

    #[async_trait]
    impl LLMProvider for EmptySummaryProvider {
        fn name(&self) -> &str {
            "empty-summary"
        }

        async fn generate(&self, _request: LLMRequest) -> Result<LLMResponse, LLMError> {
            Ok(LLMResponse::new("stub-model", "   "))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn effective_context_size(&self, _model: &str) -> usize {
            200_000
        }
    }

    #[async_trait]
    impl LLMProvider for CapturingProvider {
        fn name(&self) -> &str {
            "capturing"
        }

        async fn generate(&self, request: LLMRequest) -> Result<LLMResponse, LLMError> {
            *self.last_request.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request);
            Ok(LLMResponse::new("stub-model", "summary"))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn supports_reasoning_effort(&self, _model: &str) -> bool {
            true
        }

        fn supported_reasoning_efforts(&self, _model: &str) -> &'static [&'static str] {
            &["minimal", "low", "medium", "high", "xhigh", "max"]
        }
    }

    #[async_trait]
    impl LLMProvider for LimitedReasoningProvider {
        fn name(&self) -> &str {
            "limited-reasoning"
        }

        async fn generate(&self, request: LLMRequest) -> Result<LLMResponse, LLMError> {
            *self.last_request.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request);
            Ok(LLMResponse::new("stub-model", "summary"))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn supports_reasoning_effort(&self, _model: &str) -> bool {
            true
        }

        fn supported_reasoning_efforts(&self, _model: &str) -> &'static [&'static str] {
            &["low", "medium", "high"]
        }
    }

    #[async_trait]
    impl LLMProvider for InlineRejectingProvider {
        fn name(&self) -> &str {
            "inline-rejecting"
        }

        async fn generate(&self, request: LLMRequest) -> Result<LLMResponse, LLMError> {
            // Reject only the inline compaction request (carries the Anthropic
            // `compact_20260112` edit); the Local summary request must succeed.
            if request.context_management.is_some() {
                return Err(LLMError::Provider {
                    message: "provider rejected inline compact edit".to_string(),
                    metadata: None,
                });
            }
            Ok(LLMResponse::new("stub-model", "summary"))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        fn supports_responses_compaction(&self, _model: &str) -> bool {
            true
        }

        fn supports_native_inline_compaction(&self, _model: &str) -> bool {
            true
        }
    }

    #[async_trait]
    impl LLMProvider for CompatibleEndpointProvider {
        fn name(&self) -> &str {
            "compatible-endpoint"
        }

        async fn generate(&self, _request: LLMRequest) -> Result<LLMResponse, LLMError> {
            Ok(LLMResponse::new("stub-model", "summary"))
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn validate_request(&self, _request: &LLMRequest) -> Result<(), LLMError> {
            Ok(())
        }

        // Reports Responses API support but neither standalone nor inline
        // compaction (defaults: supports_manual_openai_compaction and
        // supports_native_inline_compaction are both false).
        fn supports_responses_compaction(&self, _model: &str) -> bool {
            true
        }
    }

    fn sample_history() -> Vec<Message> {
        vec![
            Message::assistant("setup".to_string()),
            Message::user("first request".to_string()),
            Message::assistant("working".to_string()),
            Message::user("second request".to_string()),
        ]
    }

    fn canonical_standalone_output() -> Vec<Message> {
        vec![
            Message::user("retained by provider".to_string()),
            Message::assistant(String::new()).with_reasoning_details(Some(vec![json!({
                "type": "compaction",
                "id": "cmp_1",
                "encrypted_content": "opaque_state"
            })])),
        ]
    }

    #[test]
    fn signed_provider_compaction_strips_old_thinking_but_preserves_other_details() {
        let old_assistant = Message::assistant_with_tools(
            "old answer".to_string(),
            vec![ToolCall::function(
                "call-1".to_string(),
                "lookup".to_string(),
                "{}".to_string(),
            )],
        )
        .with_reasoning_details(Some(vec![
            json!({
                "type": "thinking",
                "thinking": "old trace",
                "signature": "old-signature"
            }),
            json!({"type": "provider_extension", "state": "keep"}),
        ]));
        let history = vec![
            Message::user("old request".to_string()),
            old_assistant,
            Message::tool_response("call-1".to_string(), "lookup result".to_string()),
            Message::user("latest request ".repeat(100_000)),
        ];

        let compacted = super::build_provider_compacted_history(
            &history,
            json!({
                "type": "compaction",
                "content": "summary",
                "signature": "new-signature"
            }),
            &CompactionConfig::default(),
            true,
            20_000,
        );

        let old = compacted
            .iter()
            .find(|message| message.content.as_text() == "old answer")
            .expect("retained action history should keep the old answer");
        let details = old.reasoning_details.as_ref().expect("opaque detail should survive");
        assert_eq!(details, &vec![json!({"type": "provider_extension", "state": "keep"})]);
    }

    #[test]
    fn provider_compaction_replaces_old_provider_marker_in_continuity_tail() {
        let old_marker = Message::assistant(String::new()).with_reasoning_details(Some(vec![json!({
            "type": "compaction",
            "content": "old summary",
            "signature": "old-signature",
        })]));
        let history = vec![
            Message::user("old request".to_string()),
            old_marker,
            Message::user("latest request".to_string()),
            Message::assistant("latest response".to_string()),
        ];

        let compacted = super::build_provider_compacted_history(
            &history,
            json!({
                "type": "compaction",
                "content": "new summary",
                "signature": "new-signature"
            }),
            &CompactionConfig::default(),
            true,
            20_000,
        );
        let markers = compacted
            .iter()
            .flat_map(|message| message.reasoning_details.as_deref().unwrap_or(&[]))
            .filter(|detail| detail.get("type").and_then(serde_json::Value::as_str) == Some("compaction"))
            .collect::<Vec<_>>();

        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0]["content"], "new summary");
    }

    /// Build an assistant message that carries a (single) pending tool call.
    fn assistant_with_calls(content: &str, call_id: &str) -> Message {
        let mut message = Message::assistant(content.to_string());
        message.tool_calls = Some(vec![ToolCall {
            id: call_id.to_string(),
            call_type: "function".to_string(),
            function: None,
            text: None,
            thought_signature: None,
        }]);
        message
    }

    #[test]
    fn collect_retained_keeps_tool_result_with_its_assistant() {
        // When the assistant tool-call turn is retained, its tool result must
        // survive so the turn stays coherent (the model sees each call's return).
        let history = vec![
            Message::user("u1".to_string()),
            assistant_with_calls("calling tool", "c1"),
            Message::tool_response("c1".to_string(), "r1".to_string()),
            Message::user("u2".to_string()),
        ];
        let retained = super::collect_retained_user_messages(&history, 20_000, 4);
        assert!(retained.iter().any(|m| m.content.as_text().contains("u1")));
        assert!(retained.iter().any(|m| m.content.as_text().contains("u2")));
        assert!(
            retained.iter().any(|m| m.content.as_text().contains("r1")),
            "tool result paired with its retained assistant must survive"
        );
    }

    #[test]
    fn collect_retained_drops_orphaned_tool_result() {
        // If the assistant tool-call turn is dropped (over the retention cap),
        // its tool result is orphaned — the model never saw the call — and must
        // not survive compaction, because an orphaned result is invalid to send
        // to a provider.
        let history = vec![
            Message::user("u1".to_string()),
            assistant_with_calls("calling tool", "c1"),
            Message::tool_response("c1".to_string(), "r1".to_string()),
            Message::user("u2".to_string()),
        ];
        let retained = super::collect_retained_user_messages(&history, 20_000, 3);
        assert!(retained.iter().any(|m| m.content.as_text().contains("u1")));
        assert!(retained.iter().any(|m| m.content.as_text().contains("u2")));
        assert!(!retained.iter().any(|m| m.content.as_text().contains("r1")), "orphaned tool result must be dropped");
    }

    #[tokio::test]
    async fn manual_compaction_strategy_picks_local_for_plain_provider() {
        assert_eq!(manual_compaction_strategy(&StubProvider, "stub-model"), super::CompactionStrategy::Local);
    }

    #[tokio::test]
    async fn manual_compaction_strategy_picks_native_standalone_for_manual_provider() {
        let provider = ManualStandaloneProvider {
            last_options: Mutex::new(None),
            output: canonical_standalone_output(),
        };
        assert_eq!(manual_compaction_strategy(&provider, "stub-model"), super::CompactionStrategy::NativeStandalone);
    }

    #[tokio::test]
    async fn manual_compaction_strategy_picks_native_inline_for_responses_capable_provider() {
        let provider = InlinePauseProvider {
            last_request: Mutex::new(None),
            include_compaction_detail: true,
        };
        assert_eq!(manual_compaction_strategy(&provider, "stub-model"), super::CompactionStrategy::NativeInline);
    }

    #[tokio::test]
    async fn manual_compaction_strategy_picks_local_for_compatible_endpoint() {
        // OpenAI-compatible custom endpoints report `supports_responses_compaction`
        // but cannot serve standalone `/responses/compact` or Anthropic inline
        // compaction; they must route to Local, not NativeInline.
        assert_eq!(
            manual_compaction_strategy(&CompatibleEndpointProvider, "stub-model"),
            super::CompactionStrategy::Local
        );
    }

    #[tokio::test]
    async fn compact_history_manual_uses_local_summary_for_plain_provider() {
        let history = sample_history();
        let config = CompactionConfig {
            always_summarize: true,
            ..CompactionConfig::default()
        };

        let (compacted, mode) =
            compact_history_manual(&StubProvider, "stub-model", &history, &config, &ManualCompactionOptions::default())
                .await
                .expect("manual compaction");

        assert_eq!(mode, CompactionMode::Local);
        assert_eq!(compacted.len(), 4);
        assert_eq!(compacted[0].content.as_text(), "Previous conversation summary:\nsummary");
        assert_eq!(compacted[1].content.as_text(), "first request");
        assert_eq!(compacted[2].content.as_text(), "working");
        assert_eq!(compacted[3].content.as_text(), "second request");
    }

    #[tokio::test]
    async fn local_compaction_collects_summary_from_streaming_only_provider() {
        let provider = StreamingOnlyCompactionProvider {
            generate_calls: Mutex::new(0),
            stream_calls: Mutex::new(0),
            stream_modes: Mutex::new(Vec::new()),
        };

        let (compacted, mode) = compact_history_manual(
            &provider,
            "stub-model",
            &sample_history(),
            &CompactionConfig::default(),
            &ManualCompactionOptions::default(),
        )
        .await
        .expect("streaming-only local compaction should succeed");

        assert_eq!(mode, CompactionMode::Local);
        assert_eq!(compacted[0].content.as_text(), "Previous conversation summary:\nstreamed summary");
        assert_eq!(*provider.generate_calls.lock().unwrap(), 0, "non-streaming generation must not be attempted");
        assert_eq!(*provider.stream_calls.lock().unwrap(), 1, "summary should be collected from one normalized stream");
        assert_eq!(*provider.stream_modes.lock().unwrap(), vec![true], "stream fallback must set the request mode");
    }

    #[tokio::test]
    async fn compact_history_manual_preserves_native_standalone_window() {
        let history = sample_history();
        let config = CompactionConfig::default();
        let provider = ManualStandaloneProvider {
            last_options: Mutex::new(None),
            output: canonical_standalone_output(),
        };

        let (compacted, mode) =
            compact_history_manual(&provider, "stub-model", &history, &config, &ManualCompactionOptions::default())
                .await
                .expect("manual compaction");

        assert_eq!(mode, CompactionMode::Provider);
        assert_eq!(compacted, canonical_standalone_output());
    }

    #[tokio::test]
    async fn native_compaction_preserves_canonical_window_over_session_budget() {
        let history = sample_history();
        let canonical = vec![
            Message::user("provider-retained ".repeat(20_000)),
            Message::assistant(String::new()).with_reasoning_details(Some(vec![json!({
                "type": "compaction",
                "id": "cmp_canonical",
                "encrypted_content": "opaque_state",
            })])),
        ];
        let provider = ManualStandaloneProvider {
            last_options: Mutex::new(None),
            output: canonical.clone(),
        };
        let (compacted, mode) = compact_history_manual_with_budget(
            &provider,
            "stub-model",
            &history,
            &CompactionConfig::default(),
            &ManualCompactionOptions::default(),
            Some(8_192),
        )
        .await
        .expect("native compaction");

        assert_eq!(mode, CompactionMode::Provider);
        assert_eq!(compacted, canonical, "standalone output is the canonical replay window");
        assert!(
            compacted.iter().map(Message::estimate_tokens).sum::<usize>() > 8_192,
            "fixture must prove that the local session bound was not applied"
        );
    }

    #[tokio::test]
    async fn compact_history_manual_passes_options_to_native_standalone() {
        let history = sample_history();
        let config = CompactionConfig::default();
        let provider = ManualStandaloneProvider {
            last_options: Mutex::new(None),
            output: canonical_standalone_output(),
        };
        let options = ManualCompactionOptions {
            instructions: Some("keep only decisions".to_string()),
            max_output_tokens: Some(256),
            reasoning_effort: Some(ReasoningEffortLevel::Minimal),
            verbosity: Some(VerbosityLevel::High),
            ..ManualCompactionOptions::default()
        };

        let (_compacted, mode) = compact_history_manual(&provider, "stub-model", &history, &config, &options)
            .await
            .expect("manual compaction");

        assert_eq!(mode, CompactionMode::Provider);
        let captured = provider.last_options.lock().unwrap().clone().expect("captured options");
        assert_eq!(captured.instructions.as_deref(), Some("keep only decisions"));
        assert_eq!(captured.max_output_tokens, Some(256));
        assert_eq!(captured.reasoning_effort, Some(ReasoningEffortLevel::Minimal));
        assert_eq!(captured.verbosity, Some(VerbosityLevel::High));
    }

    #[tokio::test]
    async fn compact_history_manual_uses_native_inline_when_pause_and_compaction_present() {
        let history = sample_history();
        let config = CompactionConfig::default();
        let provider = InlinePauseProvider {
            last_request: Mutex::new(None),
            include_compaction_detail: true,
        };

        let (compacted, mode) =
            compact_history_manual(&provider, "stub-model", &history, &config, &ManualCompactionOptions::default())
                .await
                .expect("manual compaction");

        assert_eq!(mode, CompactionMode::Provider);
        assert_eq!(compacted.len(), 4);
        assert_eq!(compacted[0].role, MessageRole::Assistant);
        assert!(compacted[0].content.as_text().is_empty());
        let detail = compacted[0]
            .reasoning_details
            .as_ref()
            .and_then(|details| details.first())
            .expect("provider compaction detail");
        let detail: serde_json::Value = serde_json::from_value(detail.clone()).expect("provider detail");
        assert_eq!(detail["signature"], "provider-signature");
        assert_eq!(detail["opaque_extension"], "preserve-me");
        assert_eq!(compacted[1].content.as_text(), "first request");
        assert_eq!(compacted[2].content.as_text(), "working");
        assert_eq!(compacted[3].content.as_text(), "second request");

        // The inline request must carry the `compact_20260112` edit with a forced
        // pause so the provider actually performs compaction on demand.
        let captured = provider.last_request.lock().unwrap().clone().expect("captured inline request");
        let context_management = captured
            .context_management
            .as_ref()
            .expect("context_management set on inline compaction request");
        let edit = &context_management["edits"][0];
        assert_eq!(edit["type"].as_str(), Some("compact_20260112"));
        assert_eq!(edit["pause_after_compaction"].as_bool(), Some(true));
        assert_eq!(edit["trigger"]["value"].as_u64(), Some(50_000));
    }

    #[tokio::test]
    async fn inline_summary_without_opaque_detail_falls_back_to_local_mode() {
        let history = sample_history();
        let provider = InlinePauseProvider {
            last_request: Mutex::new(None),
            include_compaction_detail: false,
        };

        let (compacted, mode) = compact_history_manual_with_budget(
            &provider,
            "stub-model",
            &history,
            &CompactionConfig::default(),
            &ManualCompactionOptions::default(),
            Some(8_192),
        )
        .await
        .expect("manual compaction should fall back to local mode");

        assert_eq!(mode, CompactionMode::Local);
        assert_eq!(compacted[0].role, MessageRole::System);
        assert!(compacted[0].content.as_text().starts_with("Previous conversation summary:"));
    }

    #[tokio::test]
    async fn compact_history_manual_inline_request_carries_instructions_when_provided() {
        let history = sample_history();
        let config = CompactionConfig::default();
        let provider = InlinePauseProvider {
            last_request: Mutex::new(None),
            include_compaction_detail: true,
        };
        let options = ManualCompactionOptions {
            instructions: Some("  keep only decisions  ".to_string()),
            ..ManualCompactionOptions::default()
        };

        let (_compacted, _mode) = compact_history_manual(&provider, "stub-model", &history, &config, &options)
            .await
            .expect("manual compaction");

        let captured = provider.last_request.lock().unwrap().clone().expect("captured inline request");
        let edit = &captured.context_management.as_ref().expect("context_management")["edits"][0];
        assert_eq!(edit["instructions"].as_str(), Some("keep only decisions"));
    }

    #[tokio::test]
    async fn native_inline_fork_reuses_parent_prefix_when_supplied() {
        use super::{CompactionParentContext, compact_history_manual_with_parent_context};

        let history = sample_history();
        let config = CompactionConfig::default();
        let options = ManualCompactionOptions::default();

        // Asymmetric arms on the same history: without a parent the inline
        // request carries no fork fields; with one it carries all three while
        // the compaction edit stays intact in both.
        let provider = InlinePauseProvider {
            last_request: Mutex::new(None),
            include_compaction_detail: true,
        };
        let (_compacted, mode) = compact_history_manual_with_parent_context(
            &provider,
            "stub-model",
            &history,
            &config,
            &options,
            None,
            None,
        )
        .await
        .expect("manual compaction");
        assert_eq!(mode, CompactionMode::Provider);
        let bare = provider.last_request.lock().unwrap().clone().expect("captured inline request");
        assert!(bare.system_prompt.is_none());
        assert!(bare.tools.is_none());
        assert!(bare.context_management.is_some());

        let provider = InlinePauseProvider {
            last_request: Mutex::new(None),
            include_compaction_detail: true,
        };
        let parent = CompactionParentContext {
            system_prompt: Some(Arc::from("parent system")),
            tools: None,
        };
        let (_compacted, mode) = compact_history_manual_with_parent_context(
            &provider,
            "stub-model",
            &history,
            &config,
            &options,
            None,
            Some(&parent),
        )
        .await
        .expect("manual compaction");
        assert_eq!(mode, CompactionMode::Provider);
        let forked = provider.last_request.lock().unwrap().clone().expect("captured inline request");
        assert_eq!(forked.system_prompt.as_deref(), Some("parent system"));
        assert!(forked.tools.is_none(), "empty parent catalog must stay off the wire");
        assert!(matches!(forked.tool_choice, Some(crate::llm::provider::ToolChoice::None)));
        let edit = &forked.context_management.as_ref().expect("context_management")["edits"][0];
        assert_eq!(edit["type"].as_str(), Some("compact_20260112"));
        assert_eq!(edit["pause_after_compaction"].as_bool(), Some(true));
    }

    #[test]
    fn inline_compaction_edits_follow_the_documented_ladder_order() {
        use super::anthropic_inline_compaction_edits;

        let edits = anthropic_inline_compaction_edits(Some("keep decisions"), true);
        let types: Vec<&str> = edits
            .iter()
            .filter_map(|edit| edit.get("type").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            types,
            vec![
                "clear_thinking_20251015",
                "clear_tool_uses_20250919",
                "compact_20260112"
            ],
            "thinking clears first, tool uses next, compaction last"
        );
        assert_eq!(edits[2]["pause_after_compaction"].as_bool(), Some(true));
        assert_eq!(edits[2]["instructions"].as_str(), Some("keep decisions"));

        let compact_only = anthropic_inline_compaction_edits(None, false);
        assert_eq!(compact_only.len(), 1);
        assert_eq!(compact_only[0]["type"].as_str(), Some("compact_20260112"));
    }

    #[tokio::test]
    async fn native_inline_request_carries_ladder_when_provider_supports_context_edits() {
        use super::compact_history_manual_with_parent_context;

        let history = sample_history();
        let config = CompactionConfig::default();
        let provider = ContextEditsInlineProvider { last_request: Mutex::new(None) };
        let (_compacted, mode) = compact_history_manual_with_parent_context(
            &provider,
            "stub-model",
            &history,
            &config,
            &ManualCompactionOptions::default(),
            None,
            None,
        )
        .await
        .expect("manual compaction");
        assert_eq!(mode, CompactionMode::Provider);
        let request = provider.last_request.lock().unwrap().clone().expect("captured inline request");
        let edits = request.context_management.as_ref().expect("context_management")["edits"]
            .as_array()
            .expect("edits array")
            .clone();
        let types: Vec<&str> = edits
            .iter()
            .filter_map(|edit| edit.get("type").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            types,
            vec![
                "clear_thinking_20251015",
                "clear_tool_uses_20250919",
                "compact_20260112"
            ]
        );
    }

    #[tokio::test]
    async fn local_summary_retries_once_on_context_capacity_error() {
        use super::compact_history_manual;

        // Over-budget history so the halved retry is strictly smaller than
        // the first attempt (the retry is skipped when it cannot shrink).
        // Each turn is ~100k tokens against the stub's ~174k summarizer
        // budget: the first fork keeps one turn, the retry degrades to
        // previews, and the compacted output still fits the summary.
        let history = vec![
            Message::user("old ".repeat(100_000)),
            Message::user("middle ".repeat(100_000)),
            Message::user("newest ".repeat(100_000)),
        ];
        let config = CompactionConfig {
            always_summarize: true,
            ..CompactionConfig::default()
        };
        let provider = CapacityFailOnceProvider {
            attempts: Mutex::new(0),
            request_tokens: Mutex::new(Vec::new()),
            failures: 1,
        };
        let (compacted, mode) =
            compact_history_manual(&provider, "stub-model", &history, &config, &ManualCompactionOptions::default())
                .await
                .expect("retry must recover the summary");
        assert_eq!(mode, CompactionMode::Local);
        assert_eq!(*provider.attempts.lock().unwrap(), 2, "exactly one retry");
        let tokens = provider.request_tokens.lock().unwrap().clone();
        assert_eq!(tokens.len(), 2);
        assert!(tokens[1] < tokens[0], "retry must shrink the fork");
        assert_eq!(compacted[0].content.as_text(), "Previous conversation summary:\nretried summary");
    }

    #[tokio::test]
    async fn local_summary_skips_retry_when_it_cannot_shrink() {
        use super::compact_history_manual;

        // Tiny history fits the budget verbatim, so a halved retry would
        // resend the identical fork. The capacity failure must propagate
        // without a wasted second call.
        let history = sample_history();
        let config = CompactionConfig {
            always_summarize: true,
            ..CompactionConfig::default()
        };
        let provider = CapacityFailOnceProvider {
            attempts: Mutex::new(0),
            request_tokens: Mutex::new(Vec::new()),
            failures: 1,
        };
        let error =
            compact_history_manual(&provider, "stub-model", &history, &config, &ManualCompactionOptions::default())
                .await
                .expect_err("identical retry must not be attempted");
        assert_eq!(*provider.attempts.lock().unwrap(), 1, "no second call");
        assert!(error.to_string().contains("Failed to generate compaction summary"));
    }

    #[tokio::test]
    async fn hierarchical_bands_retry_once_on_context_capacity_error() {
        use super::compact_history_manual_with_budget;

        // Small session budget forces over-budget bands; the failing abstract
        // pass must retry with a halved band and the detail pass must still
        // run, so exactly three generate calls happen.
        let mut history = Vec::new();
        for turn in ["first", "second", "third", "fourth", "fifth", "sixth"] {
            history.push(Message::user(format!("{turn} {}", "text ".repeat(1_500))));
        }
        history.push(Message::user(format!("latest {}", "big ".repeat(25_000))));
        let config = CompactionConfig {
            hierarchical: true,
            always_summarize: true,
            ..CompactionConfig::default()
        };
        let provider = CapacityFailOnceProvider {
            attempts: Mutex::new(0),
            request_tokens: Mutex::new(Vec::new()),
            failures: 1,
        };
        let (_compacted, mode) = compact_history_manual_with_budget(
            &provider,
            "stub-model",
            &history,
            &config,
            &ManualCompactionOptions::default(),
            Some(3_000),
        )
        .await
        .expect("hierarchical bands must recover from a capacity error");
        assert_eq!(mode, CompactionMode::Local);
        assert_eq!(*provider.attempts.lock().unwrap(), 3, "abstract retries once, detail runs once");
    }

    #[test]
    fn route_policy_defaults_to_status_quo_shape() {
        use super::CompactionRoutePolicy;

        let policy = CompactionRoutePolicy::resolve("unknown-provider", "unknown-model");
        assert_eq!(policy, CompactionRoutePolicy::default());
        assert!((policy.threshold_ratio - 1.0).abs() < f64::EPSILON);
        assert!((policy.retain_ratio - 0.16).abs() < f64::EPSILON);
        assert_eq!(policy.max_overflow_retries, 1);
    }

    #[test]
    fn route_tail_target_scales_with_window() {
        use super::{CONTINUITY_TAIL_TARGET_TOKENS, CompactionRoutePolicy};

        let policy = CompactionRoutePolicy::default();
        // Unknown windows keep the legacy constant.
        assert_eq!(policy.tail_target_tokens(0), CONTINUITY_TAIL_TARGET_TOKENS);
        // Large windows keep the legacy constant exactly.
        assert_eq!(policy.tail_target_tokens(1_000_000), CONTINUITY_TAIL_TARGET_TOKENS);
        assert_eq!(policy.tail_target_tokens(128_000), CONTINUITY_TAIL_TARGET_TOKENS);
        // Small windows scale down so the summary survives output bounding.
        assert_eq!(policy.tail_target_tokens(32_768), 5_242);
        // Tiny windows keep a meaningful floor instead of collapsing to zero.
        assert_eq!(policy.tail_target_tokens(4_096), 1_024);
    }

    #[test]
    fn compacted_history_shrinks_rejects_growth() {
        use super::compacted_history_shrinks;
        use crate::exec::events::CompactionMode;

        // Reported `86 -> 88` regression: a rebuild that keeps every message
        // plus framing must be discarded, accounting for the envelope message
        // Local persistence injects.
        assert!(!compacted_history_shrinks(86, 87, CompactionMode::Local));
        assert!(!compacted_history_shrinks(12, 12, CompactionMode::Local));
        assert!(!compacted_history_shrinks(0, 0, CompactionMode::Local));
        // Genuine compression passes, including the envelope slot. Note a
        // one-message reduction is still rejected for Local mode: the
        // envelope re-adds it, netting zero.
        assert!(compacted_history_shrinks(88, 11, CompactionMode::Local));
        assert!(compacted_history_shrinks(12, 10, CompactionMode::Local));
        assert!(!compacted_history_shrinks(12, 11, CompactionMode::Local));
        // Provider-native windows carry no envelope framing, so the raw
        // lengths compare directly.
        assert!(compacted_history_shrinks(12, 11, CompactionMode::Provider));
        assert!(!compacted_history_shrinks(12, 12, CompactionMode::Provider));
        assert!(!compacted_history_shrinks(12, 13, CompactionMode::Provider));
    }

    #[test]
    fn threshold_cap_only_fires_earlier() {
        use super::CompactionRoutePolicy;

        let policy = CompactionRoutePolicy::default();
        assert_eq!(policy.apply_threshold_cap(900_000, 1_000_000), 900_000);
        let eager = CompactionRoutePolicy {
            threshold_ratio: 0.7,
            ..CompactionRoutePolicy::default()
        };
        assert_eq!(eager.apply_threshold_cap(900_000, 1_000_000), 700_000);
        assert_eq!(eager.apply_threshold_cap(500_000, 1_000_000), 500_000);
        assert_eq!(eager.apply_threshold_cap(900_000, 0), 900_000);
    }

    #[test]
    fn prune_oversized_tool_outputs_trims_only_tool_dumps() {
        use super::{TOOL_RESULT_PRUNE_TARGET_TOKENS, prune_oversized_tool_outputs};

        let user = Message::user("do the thing".to_string());
        let mut assistant = Message::assistant("calling tool".to_string());
        assistant.tool_calls = Some(vec![ToolCall::function(
            "call-big".to_string(),
            "read_file".to_string(),
            "{}".to_string(),
        )]);
        let huge = {
            let mut message = Message::tool_response("call-big".to_string(), "data ".repeat(20_000));
            message.tool_call_id = Some("call-big".to_string());
            message
        };
        let small = Message::tool_response("call-small".to_string(), "ok".to_string());
        let history = vec![user.clone(), assistant.clone(), huge.clone(), small.clone()];

        let pruned = prune_oversized_tool_outputs(&history);
        assert_eq!(pruned.len(), history.len());
        // Untouched roles pass through verbatim.
        assert_eq!(pruned[0].content.as_text(), user.content.as_text());
        assert_eq!(pruned[1].content.as_text(), assistant.content.as_text());
        // The dump shrinks within budget while the small result is identical.
        // Previews keep the head: the start survives, the tail does not.
        let huge_text = huge.content.as_text();
        let pruned_text = pruned[2].content.as_text();
        assert!(pruned[2].estimate_tokens() < huge.estimate_tokens(), "oversized tool output must shrink");
        assert!(
            pruned[2].estimate_tokens() <= TOOL_RESULT_PRUNE_TARGET_TOKENS + 32,
            "pruned output must respect the cap, used {}",
            pruned[2].estimate_tokens()
        );
        assert!(pruned_text.len() < huge_text.len(), "preview must be shorter than the dump");
        assert!(huge_text.starts_with(pruned_text.trim_end_matches('.')), "preview must be a head truncation");
        assert_eq!(pruned[2].tool_call_id.as_deref(), Some("call-big"), "pairing ID must survive");
        assert_eq!(pruned[3].content.as_text(), small.content.as_text());
    }

    #[tokio::test]
    async fn capacity_retry_halves_progressively_until_recovery() {
        use super::generate_summary_with_capacity_retry;

        // Each turn is tens of thousands of tokens against a 30k budget: the
        // first fork keeps one turn, then two halved retries shrink strictly
        // before the third attempt succeeds.
        let history = vec![
            Message::user("a ".repeat(20_000)),
            Message::user("b ".repeat(20_000)),
            Message::user("c ".repeat(20_000)),
        ];
        let provider = CapacityFailOnceProvider {
            attempts: Mutex::new(0),
            request_tokens: Mutex::new(Vec::new()),
            failures: 2,
        };
        let summary = generate_summary_with_capacity_retry(
            &provider,
            "stub-model",
            &history,
            "test instructions",
            &history,
            Some(30_000),
            2,
            "Failed to generate compaction summary",
            |source| {
                super::compaction_summary_request("stub-model", source, "test instructions", None, None, None, None)
            },
        )
        .await
        .expect("two retries must recover the summary");
        assert_eq!(summary, "retried summary");
        assert_eq!(*provider.attempts.lock().unwrap(), 3, "two failures then success");
        let tokens = provider.request_tokens.lock().unwrap().clone();
        assert_eq!(tokens.len(), 3);
        assert!(tokens[0] > tokens[1] && tokens[1] > tokens[2], "each retry must shrink strictly, got {tokens:?}");
    }

    #[tokio::test]
    async fn capacity_retry_recovers_with_unknown_budget_fallback() {
        use super::generate_summary_with_capacity_retry;

        // Unknown window (`None` budget) sends the first fork verbatim. A
        // capacity rejection must still shrink via a fallback derived from
        // the failing attempt instead of resending the identical fork.
        // Asymmetric vs the verbatim/no-retry case: oldest vs newest text
        // differ so trimming the oldest is observable.
        let history = vec![
            Message::user(format!("oldest {}", "old ".repeat(20_000))),
            Message::user(format!("newest {}", "new ".repeat(20_000))),
        ];
        let provider = UnknownBudgetFailOnceProvider {
            attempts: Mutex::new(0),
            request_tokens: Mutex::new(Vec::new()),
        };
        let summary = generate_summary_with_capacity_retry(
            &provider,
            "stub-model",
            &history,
            "test instructions",
            &history,
            None,
            1,
            "Failed to generate compaction summary",
            |source| {
                super::compaction_summary_request("stub-model", source, "test instructions", None, None, None, None)
            },
        )
        .await
        .expect("unknown-budget capacity error must recover via fallback");
        assert_eq!(summary, "retried summary");
        assert_eq!(*provider.attempts.lock().unwrap(), 2, "one failure then success");
        let tokens = provider.request_tokens.lock().unwrap().clone();
        assert_eq!(tokens.len(), 2);
        assert!(tokens[1] < tokens[0], "fallback retry must shrink, got {tokens:?}");
    }

    #[tokio::test]
    async fn empty_summary_fails_with_diagnostic() {
        use super::generate_summary_with_capacity_retry;

        let history = sample_history();
        let provider = EmptySummaryProvider;
        let error = generate_summary_with_capacity_retry(
            &provider,
            "stub-model",
            &history,
            "test instructions",
            &history,
            Some(30_000),
            1,
            "Failed to generate compaction summary",
            |source| {
                super::compaction_summary_request("stub-model", source, "test instructions", None, None, None, None)
            },
        )
        .await
        .expect_err("empty provider summary must fail");
        let message = format!("{error:#}");
        assert!(message.contains("Failed to generate compaction summary"), "outer context preserved: {message}");
        assert!(message.contains("empty summary"), "empty cause surfaced: {message}");
        assert!(message.contains("empty-summary"), "provider identity surfaced: {message}");
    }

    #[tokio::test]
    async fn capacity_error_context_carries_route_diagnostics() {
        // Tiny fitting history with a capacity failure cannot shrink, so the
        // original failure propagates. The propagated error must carry the
        // route diagnostics needed to debug `/compact` without guessing.
        let history = sample_history();
        let provider = CapacityFailOnceProvider {
            attempts: Mutex::new(0),
            request_tokens: Mutex::new(Vec::new()),
            failures: 1,
        };
        let error = compact_history_manual(
            &provider,
            "stub-model",
            &history,
            &CompactionConfig {
                always_summarize: true,
                ..CompactionConfig::default()
            },
            &ManualCompactionOptions::default(),
        )
        .await
        .expect_err("identical retry must not be attempted");
        let message = format!("{error:#}");
        assert!(message.contains("Failed to generate compaction summary"), "outer context: {message}");
        assert!(message.contains("capacity-fail-once"), "provider name: {message}");
        assert!(message.contains("stub-model"), "model name: {message}");
        assert!(message.contains("input_tokens="), "token counts: {message}");
    }

    #[tokio::test]
    async fn compact_history_manual_falls_back_to_local_when_inline_compaction_not_fired() {
        let history = sample_history();
        let config = CompactionConfig::default();

        // NativeCompactionProvider is inline-capable but its `generate` returns a
        // normal `Stop` with no compaction block, so the inline attempt cannot
        // fire and the dispatch must transparently fall back to Local.
        let (compacted, mode) = compact_history_manual(
            &NativeCompactionProvider,
            "stub-model",
            &history,
            &config,
            &ManualCompactionOptions::default(),
        )
        .await
        .expect("manual compaction");

        assert_eq!(mode, CompactionMode::Local);
        assert_eq!(compacted.len(), 4);
        assert_eq!(compacted[0].content.as_text(), "Previous conversation summary:\nsummary");
    }

    #[tokio::test]
    async fn compact_history_manual_falls_back_to_local_when_inline_request_errors() {
        let history = sample_history();
        let config = CompactionConfig::default();

        // A provider dispatched to NativeInline that rejects the Anthropic
        // `compact_20260112` edit must not abort the whole command; the dispatch
        // falls back to Local summarization (the manual `/compact` contract:
        // always succeeds).
        let (compacted, mode) = compact_history_manual(
            &InlineRejectingProvider,
            "stub-model",
            &history,
            &config,
            &ManualCompactionOptions::default(),
        )
        .await
        .expect("manual compaction should fall back to local");

        assert_eq!(mode, CompactionMode::Local);
        assert_eq!(compacted.len(), 4);
        assert_eq!(compacted[0].content.as_text(), "Previous conversation summary:\nsummary");
    }

    #[tokio::test]
    async fn compact_history_manual_applies_options_to_local_summary_request() {
        let history = sample_history();
        let config = CompactionConfig {
            always_summarize: true,
            ..CompactionConfig::default()
        };
        let provider = CapturingProvider { last_request: Mutex::new(None) };
        let options = ManualCompactionOptions {
            instructions: Some("KEEP DECISIONS ONLY".to_string()),
            max_output_tokens: Some(128),
            reasoning_effort: Some(ReasoningEffortLevel::Minimal),
            verbosity: Some(VerbosityLevel::High),
            ..ManualCompactionOptions::default()
        };

        let (compacted, mode) = compact_history_manual(&provider, "stub-model", &history, &config, &options)
            .await
            .expect("manual compaction");

        assert_eq!(mode, CompactionMode::Local);
        let captured = provider.last_request.lock().unwrap().clone().expect("captured summary request");
        assert_eq!(captured.max_tokens, Some(128));
        assert_eq!(captured.reasoning_effort, Some(ReasoningEffortLevel::Minimal));
        assert_eq!(captured.verbosity, Some(VerbosityLevel::High));
        // Cache-safe forking: the parent history prefix is reused verbatim and
        // the custom instructions are appended as the only new turn.
        assert_eq!(captured.messages.len(), history.len() + 1);
        for (sent, original) in captured.messages.iter().zip(history.iter()) {
            assert_eq!(sent.content.as_text(), original.content.as_text());
        }
        let prompt = captured.messages.last().expect("compaction prompt").content.as_text();
        assert!(prompt.contains("KEEP DECISIONS ONLY"));
        assert!(!prompt.contains("acceptance criteria"));
        // The fork must not invite tool calls: same tools may be present for
        // prefix reuse, but the choice disables invocation.
        assert!(matches!(captured.tool_choice, Some(crate::llm::provider::ToolChoice::None)));
        assert_eq!(compacted[0].content.as_text(), "Previous conversation summary:\nsummary");
    }

    #[tokio::test]
    async fn compact_history_manual_blocks_unsupported_reasoning_before_summary() {
        let provider = LimitedReasoningProvider { last_request: Mutex::new(None) };
        let options = ManualCompactionOptions {
            reasoning_effort: Some(ReasoningEffortLevel::Max),
            ..ManualCompactionOptions::default()
        };
        let error = compact_history_manual(
            &provider,
            "stub-model",
            &sample_history(),
            &CompactionConfig {
                always_summarize: true,
                ..CompactionConfig::default()
            },
            &options,
        )
        .await
        .expect_err("unsupported compaction effort must block");

        assert!(
            error
                .downcast_ref::<crate::llm::reasoning_effort::ReasoningEffortUnsupported>()
                .is_some(),
            "strict compaction failure should preserve the capability diagnostic: {error:#}"
        );
        assert!(provider.last_request.lock().unwrap().is_none(), "provider must not receive a blocked request");
    }

    #[tokio::test]
    async fn compact_history_manual_downgrades_only_when_explicitly_enabled() {
        let provider = LimitedReasoningProvider { last_request: Mutex::new(None) };
        let options = ManualCompactionOptions {
            reasoning_effort: Some(ReasoningEffortLevel::Max),
            allow_reasoning_effort_downgrade: true,
            ..ManualCompactionOptions::default()
        };
        let config = CompactionConfig {
            always_summarize: true,
            hierarchical: true,
            ..CompactionConfig::default()
        };
        compact_history_manual(&provider, "stub-model", &sample_history(), &config, &options)
            .await
            .expect("explicit downgrade should permit compaction");

        let captured = provider.last_request.lock().unwrap().clone().expect("summary request captured");
        assert_eq!(captured.reasoning_effort, Some(ReasoningEffortLevel::High));
    }

    #[tokio::test]
    async fn compact_history_manual_returns_empty_for_empty_history() {
        let (compacted, mode) = compact_history_manual(
            &StubProvider,
            "stub-model",
            &[],
            &CompactionConfig::default(),
            &ManualCompactionOptions::default(),
        )
        .await
        .expect("manual compaction");

        assert!(compacted.is_empty());
        assert_eq!(mode, CompactionMode::Local);
    }

    #[tokio::test]
    async fn compact_history_rebuilds_history_around_summary_and_important_messages() {
        let history = vec![
            Message::assistant("setup".to_string()),
            Message::user("first request".to_string()),
            Message::assistant("working".to_string()),
            Message::tool_response("call-1".to_string(), "done".to_string()),
            Message::user("second request".to_string()),
            Message::assistant("final reply".to_string()),
        ];
        let config = CompactionConfig {
            always_summarize: true,
            ..CompactionConfig::default()
        };

        let compacted = compact_history(&StubProvider, "stub-model", &history, &config)
            .await
            .expect("compacted history");

        // Summary plus the complete newest protocol groups. The assistant/tool
        // messages remain paired with their user anchors in the continuity
        // tail.
        assert_eq!(compacted.len(), 6);
        assert_eq!(compacted[0].content.as_text(), "Previous conversation summary:\nsummary");
        assert_eq!(compacted[1].content.as_text(), "first request");
        assert_eq!(compacted[2].content.as_text(), "working");
        assert_eq!(compacted[3].content.as_text(), "done");
        assert_eq!(compacted[4].content.as_text(), "second request");
        assert_eq!(compacted[5].content.as_text(), "final reply");
    }

    #[tokio::test]
    async fn legacy_compaction_uses_local_for_responses_only_capability() {
        let config = CompactionConfig {
            keep_last_messages: 0,
            ..CompactionConfig::default()
        };

        // Responses capability alone is not enough for this legacy entry point:
        // the provider's `compact_history` method is only valid for standalone
        // compaction endpoints. The universal local path must remain usable.
        let compacted = compact_history(&CompatibleEndpointProvider, "stub-model", &sample_history(), &config)
            .await
            .expect("local compaction should handle responses-only providers");

        assert_eq!(compacted[0].content.as_text(), "Previous conversation summary:\nsummary");
    }

    #[tokio::test]
    async fn compact_history_preserves_continuity_tail_over_retention_budget() {
        let history = vec![
            Message::user("alpha beta gamma delta epsilon zeta".to_string()),
            Message::assistant("ack".to_string()),
            Message::user("newest request".to_string()),
        ];
        let config = CompactionConfig {
            always_summarize: true,
            retained_user_message_tokens: 8,
            ..CompactionConfig::default()
        };

        let compacted = compact_history(&StubProvider, "stub-model", &history, &config)
            .await
            .expect("compacted history");

        assert_eq!(compacted.len(), 4);
        assert_eq!(compacted[1].content.as_text(), "alpha beta gamma delta epsilon zeta");
        assert_eq!(compacted[2].content.as_text(), "ack");
        assert_eq!(compacted[3].content.as_text(), "newest request");
    }

    #[tokio::test]
    async fn compacted_history_respects_model_context_budget() {
        let mut history = Vec::new();
        for index in 0..24 {
            history.push(Message::user(format!("request-{index} {}", "context ".repeat(1_200))));
            history.push(Message::assistant(format!("completed request {index}")));
        }
        let config = CompactionConfig {
            always_summarize: true,
            ..CompactionConfig::default()
        };

        let compacted = compact_history(&StubProvider, "stub-model", &history, &config)
            .await
            .expect("compacted history");
        let estimated_tokens = compacted.iter().map(Message::estimate_tokens).sum::<usize>();

        assert!(estimated_tokens <= 32_768 - 512);
    }

    #[tokio::test]
    async fn compact_history_caps_retained_user_message_count() {
        let history = vec![
            Message::user("first request".to_string()),
            Message::assistant("ack".to_string()),
            Message::user("second request".to_string()),
            Message::assistant("ack".to_string()),
            Message::user("third request".to_string()),
            Message::assistant("ack".to_string()),
            Message::user("fourth request".to_string()),
            Message::assistant("ack".to_string()),
            Message::user("fifth request".to_string()),
        ];
        let config = CompactionConfig {
            always_summarize: true,
            retained_user_messages: 4,
            ..CompactionConfig::default()
        };

        let compacted = compact_history(&StubProvider, "stub-model", &history, &config)
            .await
            .expect("compacted history");

        let continuity_tail = compacted
            .iter()
            .skip(1)
            .map(|message| message.content.as_text().to_string())
            .collect::<Vec<_>>();
        assert_eq!(continuity_tail.len(), history.len());
        assert_eq!(continuity_tail[0], "first request");
        assert_eq!(continuity_tail[8], "fifth request");
    }

    #[tokio::test]
    async fn compact_history_forces_local_summary_when_always_summarize_is_enabled() {
        let history = vec![
            Message::user("first request".to_string()),
            Message::assistant("working".to_string()),
            Message::user("second request".to_string()),
        ];
        let config = CompactionConfig {
            always_summarize: true,
            ..CompactionConfig::default()
        };

        let compacted = compact_history(&NativeCompactionProvider, "stub-model", &history, &config)
            .await
            .expect("compacted history");

        assert_eq!(compacted.len(), 4);
        assert_eq!(compacted[0].content.as_text(), "Previous conversation summary:\nsummary");
        assert_eq!(compacted[1].content.as_text(), "first request");
        assert_eq!(compacted[2].content.as_text(), "working");
        assert_eq!(compacted[3].content.as_text(), "second request");
    }

    #[test]
    fn default_summary_prompt_preserves_required_compaction_context() {
        let prompt = CompactionConfig::default().summary_prompt;

        assert!(prompt.contains("acceptance criteria"));
        assert!(prompt.contains("file paths that were read or modified"));
        assert!(prompt.contains("test results and error messages"));
        assert!(prompt.contains("decisions with their reasoning"));
    }

    #[test]
    fn continuity_tail_keeps_complete_turn_but_drops_unmatched_tool_call() {
        // A completed turn: user -> assistant(tool call) -> tool result. The
        // tail must keep the whole turn intact (the tool result makes the
        // trailing assistant tool call valid to send).
        let complete = vec![
            Message::user("do the thing".into()),
            {
                let mut m = Message::assistant("calling".into());
                m.tool_calls = Some(vec![ToolCall::function("c1".into(), "run".into(), "{}".into())]);
                m
            },
            Message::tool_response("c1".into(), "ran".into()),
        ];
        assert_eq!(continuity_tail(&complete).len(), 3);

        // An interrupted turn: user -> assistant(tool call) with no tool
        // result. Sending the trailing assistant message to a provider is
        // invalid, so the tail must drop it and keep only the user message.
        let interrupted = vec![Message::user("do the thing".into()), {
            let mut m = Message::assistant("calling".into());
            m.tool_calls = Some(vec![ToolCall::function("c1".into(), "run".into(), "{}".into())]);
            m
        }];
        let tail = continuity_tail(&interrupted);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].role, MessageRole::User);

        let parallel_complete = vec![
            Message::user("run both".into()),
            Message::assistant_with_tools(
                "calling".into(),
                vec![
                    ToolCall::function("c1".into(), "run".into(), "{}".into()),
                    ToolCall::function("c2".into(), "run".into(), "{}".into()),
                ],
            ),
            Message::tool_response("c1".into(), "first result".into()),
            Message::tool_response("c2".into(), "second result".into()),
        ];
        assert_eq!(continuity_tail(&parallel_complete).len(), 4);

        let parallel_interrupted = parallel_complete[..3].to_vec();
        let tail = continuity_tail(&parallel_interrupted);
        assert_eq!(tail.len(), 1, "a missing parallel result drops the whole assistant call");
        assert_eq!(tail[0].content.as_text(), "run both");
    }

    #[test]
    fn continuity_tail_keeps_newest_complete_groups_within_budget() {
        let mut history = Vec::new();
        for group_index in 0..12 {
            history.push(Message::user(format!("group-{group_index} {}", "alpha beta gamma delta ".repeat(2_000))));
            history.push(Message::assistant(format!("completed group {group_index}")));
        }

        let tail = continuity_tail(&history);
        let estimated_tokens = tail.iter().map(Message::estimate_tokens).sum::<usize>();

        assert!(estimated_tokens <= super::CONTINUITY_TAIL_TARGET_TOKENS);
        assert_eq!(tail.first().map(|message| message.role), Some(MessageRole::User));
        assert!(tail.iter().any(|message| message.content.as_text().contains("group-11")));
        assert!(!tail.iter().any(|message| message.content.as_text().contains("group-0 ")));
        assert_eq!(tail.len() % 2, 0, "protocol groups must remain atomic");
    }

    #[test]
    fn continuity_tail_bounds_an_oversized_newest_group() {
        let history = vec![
            Message::user("u".repeat(100_000)),
            Message::assistant("a".repeat(100_000)),
        ];

        let tail = continuity_tail(&history);

        assert_eq!(tail.len(), 2);
        assert!(tail.iter().map(Message::estimate_tokens).sum::<usize>() <= super::CONTINUITY_TAIL_TARGET_TOKENS);
    }

    #[test]
    fn continuity_tail_bounds_tool_call_metadata_without_breaking_correlation() {
        let mut assistant = Message::assistant(String::new());
        assistant.tool_calls = Some(vec![ToolCall::function(
            "call-large".into(),
            "run_command".into(),
            format!("{{\"command\":\"{}\"}}", "x".repeat(300_000)),
        )]);
        let history = vec![
            Message::user("run the command".into()),
            assistant,
            Message::tool_response("call-large".into(), "completed".into()),
        ];

        let tail = continuity_tail(&history);
        assert!(tail.iter().map(Message::estimate_tokens).sum::<usize>() <= super::CONTINUITY_TAIL_TARGET_TOKENS);
        let call = tail[1].tool_calls.as_ref().expect("tool call should remain").first().unwrap();
        assert_eq!(call.id, "call-large");
        assert_eq!(call.function.as_ref().unwrap().arguments, "{}");
        assert_eq!(tail[2].tool_call_id.as_deref(), Some("call-large"));
    }

    /// History-growth verify-item: the local summarization prompt must exclude
    /// `Message.reasoning` / `reasoning_details` and serialize only the visible
    /// text content (`content.as_text()`). Reasoning traces are large and
    /// ephemeral; including them in every compaction summary would bloat the
    /// post-compaction context and re-inject stale chain-of-thought. The
    /// continuity tail preserves provider-protocol reasoning (Anthropic
    /// thinking signatures, OpenAI reasoning items) separately and is NOT
    /// summarized, so stripping reasoning here is safe and correct. This test
    /// pins that invariant so a change to `build_summary_prompt` cannot
    /// accidentally start including reasoning.
    #[test]
    fn build_summary_prompt_excludes_reasoning_traces() {
        use super::build_summary_prompt;

        let instructions = "Summarize the conversation.";
        // Assistant message carrying a reasoning trace alongside its visible
        // content. If `build_summary_prompt` ever reads `reasoning`, the
        // summary would contain "SECRET_REASONING" and "raw-only reasoning".
        let history = vec![
            Message::user("What is 2+2?".to_string()),
            Message::assistant("The answer is 4.".to_string())
                .with_reasoning(Some("SECRET_REASONING: I computed 2+2=4.".to_string())),
        ];

        let prompt = build_summary_prompt(&history, instructions);

        assert!(prompt.contains("The answer is 4."), "visible assistant text must appear in the summary prompt");
        assert!(
            !prompt.contains("SECRET_REASONING"),
            "Message.reasoning must NOT be included in the summary prompt -- \
             it would bloat every compaction pass with ephemeral chain-of-thought"
        );
        assert!(prompt.contains("Summarize the conversation."), "instructions must appear in the summary prompt");
    }

    #[test]
    fn coherence_force_keeps_tool_results_not_in_selection() {
        // Regression: `coherence_tool_call_pairs` force-keeps the Tool results
        // that follow a retained Assistant-with-tool_calls, but the previous
        // implementation derived its output from `selected` only, so those
        // force-kept messages silently vanished. A compacted history can then
        // carry an Assistant tool-call with no result, which providers reject.
        use super::coherence_tool_call_pairs;

        let history = vec![
            Message::user("check the code".to_string()),
            Message::assistant("Looking...".to_string()).with_tool_calls(vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: Some(crate::llm::provider::FunctionCall {
                    namespace: None,
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }),
                text: None,
                thought_signature: None,
            }]),
            Message::tool_response("call_1".to_string(), "tool result that must survive".to_string()),
            Message::assistant("Done.".to_string()),
        ];

        // Selection keeps only the assistant-with-tool-calls turn, dropping the
        // following Tool result from the budget-driven selection.
        let selected = vec![(1, history[1].clone())];
        let result = coherence_tool_call_pairs(&history, &selected);

        let tool_results = result
            .iter()
            .filter(|(_, m)| m.role == MessageRole::Tool)
            .map(|(idx, m)| (*idx, m.content.as_text().to_string()))
            .collect::<Vec<_>>();

        assert_eq!(
            tool_results,
            vec![(2usize, "tool result that must survive".to_string())],
            "force-kept tool result must survive compaction even when not selected"
        );
        // History order must be preserved with the assistant preceding its result.
        let indices = result.iter().map(|(idx, _)| *idx).collect::<Vec<_>>();
        assert_eq!(indices, vec![1, 2]);
    }

    #[test]
    fn coherence_drops_orphaned_tool_results_of_unretained_assistant() {
        use super::coherence_tool_call_pairs;

        let history = vec![
            Message::user("check".to_string()),
            Message::assistant("Looking...".to_string()).with_tool_calls(vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: Some(crate::llm::provider::FunctionCall {
                    namespace: None,
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }),
                text: None,
                thought_signature: None,
            }]),
            Message::tool_response("call_1".to_string(), "orphan result".to_string()),
        ];

        // The calling assistant was NOT retained; its orphaned Tool result must
        // be dropped so we never emit a result the model never saw a call for.
        let selected = vec![(0, history[0].clone())];
        let result = coherence_tool_call_pairs(&history, &selected);

        assert_eq!(result.len(), 1, "orphaned tool result must be dropped");
        assert_eq!(result[0].0, 0);
    }

    #[test]
    fn cache_safe_fork_reuses_parent_prefix_with_appended_prompt() {
        use super::{CompactionParentContext, build_cache_safe_compaction_history, compaction_summary_request};

        let history = sample_history();
        let forked = build_cache_safe_compaction_history(&history, "Summarize now.");
        assert_eq!(forked.len(), history.len() + 1);
        for (forked_msg, original) in forked.iter().zip(history.iter()) {
            assert_eq!(forked_msg.content.as_text(), original.content.as_text());
            assert_eq!(forked_msg.role, original.role);
        }
        let last = forked.last().expect("appended compaction prompt");
        assert_eq!(last.role, MessageRole::User);
        assert_eq!(last.content.as_text(), "Summarize now.");

        // Parent prefix reuse: same system/tools, tool calls disabled.
        let parent = CompactionParentContext {
            system_prompt: Some(Arc::from("stable system")),
            tools: Some(Arc::new(vec![crate::llm::provider::ToolDefinition::function(
                "read".to_string(),
                "read".to_string(),
                serde_json::json!({"type": "object"}),
            )])),
        };
        let request =
            compaction_summary_request("stub-model", &history, "Summarize now.", None, None, None, Some(&parent));
        assert_eq!(request.system_prompt.as_deref(), Some("stable system"));
        assert_eq!(request.tools.as_deref().map(Vec::len), Some(1));
        assert_eq!(request.messages.len(), history.len() + 1);
        assert!(matches!(request.tool_choice, Some(crate::llm::provider::ToolChoice::None)));
        assert!(!parent.is_empty());
        assert!(CompactionParentContext::default().is_empty());
    }

    #[tokio::test]
    async fn manual_compaction_with_parent_context_reuses_prefix() {
        use super::{CompactionParentContext, compact_history_manual_with_parent_context};

        let history = sample_history();
        let config = CompactionConfig {
            always_summarize: true,
            ..CompactionConfig::default()
        };
        let provider = CapturingProvider { last_request: Mutex::new(None) };
        let parent = CompactionParentContext {
            system_prompt: Some(Arc::from("parent system")),
            tools: None,
        };
        let (compacted, mode) = compact_history_manual_with_parent_context(
            &provider,
            "stub-model",
            &history,
            &config,
            &ManualCompactionOptions::default(),
            None,
            Some(&parent),
        )
        .await
        .expect("parent-aware compaction");
        assert_eq!(mode, CompactionMode::Local);
        let captured = provider.last_request.lock().unwrap().clone().expect("captured request");
        assert_eq!(captured.system_prompt.as_deref(), Some("parent system"));
        assert_eq!(captured.messages.len(), history.len() + 1);
        assert_eq!(compacted[0].content.as_text(), "Previous conversation summary:\nsummary");
    }

    #[tokio::test]
    async fn hierarchical_bands_reuse_parent_prefix_without_tool_calls() {
        use super::{CompactionParentContext, compact_history_manual_with_parent_context};

        let history = (0..12)
            .map(|index| Message::user(format!("hierarchical request {index}")))
            .collect::<Vec<_>>();
        let config = CompactionConfig {
            always_summarize: true,
            hierarchical: true,
            ..CompactionConfig::default()
        };
        let provider = CapturingProvider { last_request: Mutex::new(None) };
        let parent = CompactionParentContext {
            system_prompt: Some(Arc::from("parent system")),
            tools: None,
        };
        let (compacted, mode) = compact_history_manual_with_parent_context(
            &provider,
            "stub-model",
            &history,
            &config,
            &ManualCompactionOptions::default(),
            None,
            Some(&parent),
        )
        .await
        .expect("hierarchical compaction");
        assert_eq!(mode, CompactionMode::Local);
        // CapturingProvider keeps the last request, which is the detail band.
        let captured = provider.last_request.lock().unwrap().clone().expect("captured detail request");
        assert_eq!(captured.system_prompt.as_deref(), Some("parent system"));
        assert!(matches!(captured.tool_choice, Some(crate::llm::provider::ToolChoice::None)));
        assert!(!compacted.is_empty());
    }
}
