use super::{
    CompactionConfig, ManualCompactionOptions, compact_history, compact_history_manual,
    compact_history_manual_with_budget, continuity_tail, manual_compaction_strategy,
};
use crate::config::types::{ReasoningEffortLevel, VerbosityLevel};
use crate::exec::events::CompactionMode;
use crate::llm::provider::{
    LLMError, LLMNormalizedStream, LLMProvider, LLMRequest, LLMResponse, Message, MessageRole, NormalizedStreamEvent,
    ResponsesCompactionOptions,
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
    let (_compacted, mode) =
        compact_history_manual_with_parent_context(&provider, "stub-model", &history, &config, &options, None, None)
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
    use super::native_inline::anthropic_inline_compaction_edits;

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
    let error = compact_history_manual(&provider, "stub-model", &history, &config, &ManualCompactionOptions::default())
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
            super::compaction_summary_request("stub-model", source, "test instructions", None, None, None, true, None)
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
            super::compaction_summary_request("stub-model", source, "test instructions", None, None, None, true, None)
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
            super::compaction_summary_request("stub-model", source, "test instructions", None, None, None, true, None)
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
        compaction_summary_request("stub-model", &history, "Summarize now.", None, None, None, true, Some(&parent));
    assert_eq!(request.system_prompt.as_deref(), Some("stable system"));
    assert_eq!(request.tools.as_deref().map(Vec::len), Some(1));
    assert_eq!(request.messages.len(), history.len() + 1);
    assert!(matches!(request.tool_choice, Some(crate::llm::provider::ToolChoice::None)));
    assert!(!parent.is_empty());
    assert!(CompactionParentContext::default().is_empty());
}

#[test]
fn compaction_summary_request_strips_turn_scoped_without_native_support() {
    use super::compaction_summary_request;

    let history = vec![
        Message::user("do the thing".to_string()),
        Message::turn_scoped_system("collapsed output notice".to_string()),
    ];
    let request = compaction_summary_request("stub-model", &history, "Summarize now.", None, None, None, false, None);
    let scoped = request
        .messages
        .iter()
        .find(|message| message.content.as_text().as_ref() == "collapsed output notice")
        .expect("collapsed notice text must survive sanitization");
    assert!(scoped.clear_at.is_none(), "merge-gateway wire must not carry clear_at");
    assert_eq!(request.messages.len(), history.len() + 1);
}

#[test]
fn compaction_summary_request_preserves_turn_scoped_with_native_support() {
    use super::compaction_summary_request;

    let history = vec![
        Message::user("do the thing".to_string()),
        Message::turn_scoped_system("collapsed output notice".to_string()),
    ];
    let request = compaction_summary_request("stub-model", &history, "Summarize now.", None, None, None, true, None);
    let scoped = request
        .messages
        .iter()
        .find(|message| message.content.as_text().as_ref() == "collapsed output notice")
        .expect("collapsed notice must be present");
    assert!(scoped.clear_at.is_some(), "anthropic wire must keep clear_at");
}

#[tokio::test]
async fn local_summary_fork_omits_clear_at_for_gateway_routes() {
    use super::compact_history_manual_with_parent_context;

    let history = vec![
        Message::user("do the thing".to_string()),
        Message::turn_scoped_system("collapsed output notice".to_string()),
        Message::user("continue".to_string()),
    ];
    let config = CompactionConfig {
        always_summarize: true,
        ..CompactionConfig::default()
    };
    // CapturingProvider uses the trait default
    // `supports_turn_scoped_system_messages == false`, like merge-gateway.
    let provider = CapturingProvider { last_request: Mutex::new(None) };
    let (compacted, mode) = compact_history_manual_with_parent_context(
        &provider,
        "openai/gpt-6-luna",
        &history,
        &config,
        &ManualCompactionOptions::default(),
        None,
        None,
    )
    .await
    .expect("gateway local compaction must succeed");
    assert_eq!(mode, CompactionMode::Local);
    let captured = provider.last_request.lock().unwrap().clone().expect("captured request");
    assert!(
        captured.messages.iter().all(|message| message.clear_at.is_none()),
        "gateway summary fork must not carry clear_at"
    );
    assert!(
        captured
            .messages
            .iter()
            .any(|message| message.content.as_text().as_ref() == "collapsed output notice"),
        "collapsed notice text must survive as ordinary directive"
    );
    assert!(!compacted.is_empty());
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
