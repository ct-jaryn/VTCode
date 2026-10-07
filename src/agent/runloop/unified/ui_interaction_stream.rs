use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::{Notify, mpsc};

use crate::agent::runloop::unified::plan_blocks::{
    ProposedPlanStreamParser, extract_any_plan, strip_plan_persistence_policy_line,
};
use crate::agent::runloop::unified::stream_sanitization::StreamSanitizer;
use vtcode_commons::formatting::compact_reasoning_text;
use vtcode_core::copilot::CopilotRuntimeRequest;
use vtcode_core::llm::error_display;
use vtcode_core::llm::provider::{self as uni, LLMStreamEvent, NormalizedStreamEvent};
use vtcode_core::utils::ansi::{AnsiRenderer, MessageStyle};

use super::state::CtrlCState;
use super::ui_interaction::{PlaceholderSpinner, StreamProgressEvent, StreamSpinnerOptions};
use super::ui_interaction_stream_helpers::{common_prefix_len, map_render_error, reasoning_matches_content};

#[derive(Clone, Copy)]
pub(crate) struct FirstProgressTimeout {
    pub(crate) deadline: tokio::time::Instant,
    budget: Duration,
}

impl FirstProgressTimeout {
    pub(crate) fn starting_now(budget: Duration) -> Self {
        Self {
            deadline: tokio::time::Instant::now() + budget,
            budget,
        }
    }
}

fn first_progress_timeout_error(provider_name: &str, budget: Duration) -> uni::LLMError {
    uni::LLMError::Provider {
        message: error_display::format_llm_error(
            provider_name,
            &format!("LLM first token timed out after {} seconds", budget.as_secs()),
        ),
        metadata: None,
    }
}

#[derive(Default)]
struct StreamingReasoningState {
    buffered: String,
    /// Number of chars of `buffered`'s compacted form already rendered.
    printed_chars: usize,
    render_output: bool,
    defer_rendering: bool,
    rendered_any: bool,
}

impl StreamingReasoningState {
    fn new(inline_enabled: bool) -> Self {
        Self {
            buffered: String::new(),
            printed_chars: 0,
            render_output: true,
            defer_rendering: !inline_enabled,
            rendered_any: false,
        }
    }

    /// Append a reasoning delta and emit any newly-stabilized, compacted tail.
    ///
    /// Each chunk is printed exactly once and blank-line spam from the model is
    /// collapsed, so the on-screen chain-of-thought stays compact. When rendering
    /// is deferred (non-streaming), the delta is buffered and emitted at
    /// [`Self::finalize`].
    fn handle_delta(&mut self, renderer: &mut AnsiRenderer, delta: &str) -> Result<bool> {
        if !self.render_output {
            return Ok(false);
        }
        self.buffered.push_str(delta);
        if self.defer_rendering {
            return Ok(false);
        }
        self.render_new_tail(renderer)
    }

    /// Render the newly-stabilized tail of the compacted reasoning buffer.
    fn render_new_tail(&mut self, renderer: &mut AnsiRenderer) -> Result<bool> {
        if self.buffered.is_empty() {
            return Ok(self.rendered_any);
        }

        let compact = compact_reasoning_text(&self.buffered);
        let compact_len = compact.chars().count();
        if self.printed_chars > compact_len {
            self.printed_chars = compact_len;
        }
        let tail: String = compact.chars().skip(self.printed_chars).collect();
        if tail.is_empty() {
            return Ok(self.rendered_any);
        }

        let inline = renderer.writes_to_inline_sink();
        for line in tail.split('\n') {
            let is_blank = line.trim().is_empty();
            if is_blank {
                if !inline {
                    renderer.line(MessageStyle::Reasoning, "")?;
                }
                // Inline sink: drop blank lines entirely to avoid blank-line spam.
                continue;
            }
            let style = if super::reasoning::is_decision_or_tool_line(line) {
                MessageStyle::ReasoningEmphasis
            } else {
                MessageStyle::Reasoning
            };
            if inline {
                // Preserve line breaks while dropping blank-line spam: each
                // non-blank line is streamed with a trailing newline.
                let mut piece = String::with_capacity(line.len() + 1);
                piece.push_str(line);
                piece.push('\n');
                renderer.inline_with_style(style, &piece)?;
            } else {
                renderer.line(style, line)?;
            }
        }
        self.printed_chars = compact_len;
        self.rendered_any = true;
        Ok(true)
    }

    fn flush_pending(&mut self, renderer: &mut AnsiRenderer) -> Result<bool> {
        if !self.render_output {
            self.buffered.clear();
            self.printed_chars = 0;
            return Ok(false);
        }
        self.render_new_tail(renderer)
    }

    fn finalize(
        &mut self,
        renderer: &mut AnsiRenderer,
        final_reasoning: Option<&str>,
        reasoning_already_emitted: bool,
        suppress_reasoning: bool,
    ) -> Result<()> {
        if !self.render_output {
            self.buffered.clear();
            self.printed_chars = 0;
            return Ok(());
        }
        if suppress_reasoning {
            self.buffered.clear();
            self.printed_chars = 0;
            return Ok(());
        }

        self.flush_pending(renderer)?;
        if self.rendered_any {
            return Ok(());
        }

        if !reasoning_already_emitted
            && let Some(reasoning_text) = final_reasoning
            && !reasoning_text.trim().is_empty()
        {
            let compact = compact_reasoning_text(reasoning_text);
            if !compact.trim().is_empty() {
                use crate::agent::runloop::unified::ui_interaction_stream_helpers::render_compact_reasoning_block;
                render_compact_reasoning_block(renderer, reasoning_text)?;
                self.rendered_any = true;

                use super::reasoning::analyze_reasoning;
                let analysis = analyze_reasoning(&compact);
                if analysis.has_concerns() {
                    tracing::debug!(
                        concern = ?analysis.priority_concern(),
                        "Reasoning concern detected in CoT output"
                    );
                }
            }
        }
        Ok(())
    }

    fn handle_stream_failure(&mut self, _renderer: &mut AnsiRenderer) -> Result<()> {
        self.buffered.clear();
        Ok(())
    }

    fn rendered_reasoning(&self) -> bool {
        self.rendered_any
    }

    fn is_deferred(&self) -> bool {
        self.defer_rendering
    }
}

fn flush_pending_reasoning_delta(
    provider_name: &str,
    renderer: &mut AnsiRenderer,
    reasoning_state: &mut StreamingReasoningState,
    on_progress: &mut Option<&mut (dyn FnMut(StreamProgressEvent) + Send)>,
    pending_delta: &mut String,
) -> Result<bool, uni::LLMError> {
    if pending_delta.is_empty() {
        return Ok(false);
    }

    let delta = std::mem::take(pending_delta);
    if let Some(callback) = on_progress.as_deref_mut() {
        callback(StreamProgressEvent::ReasoningDelta(delta.clone()));
    }

    reasoning_state
        .handle_delta(renderer, &delta)
        .map_err(|err| map_render_error(provider_name, err))
}

fn flush_pending_reasoning(
    provider_name: &str,
    renderer: &mut AnsiRenderer,
    reasoning_state: &mut StreamingReasoningState,
    on_progress: &mut Option<&mut (dyn FnMut(StreamProgressEvent) + Send)>,
    pending_delta: &mut String,
    pending_render_bytes: &mut usize,
    last_render_at: &mut Instant,
    reasoning_emitted: &mut bool,
) -> Result<(), uni::LLMError> {
    let rendered = flush_pending_reasoning_delta(provider_name, renderer, reasoning_state, on_progress, pending_delta)?;
    if rendered {
        *reasoning_emitted = true;
    }
    *pending_render_bytes = 0;
    *last_render_at = Instant::now();
    Ok(())
}

fn stream_markdown_with_provider_error(
    provider_name: &str,
    renderer: &mut AnsiRenderer,
    text: &str,
    previous_line_count: usize,
) -> Result<usize, uni::LLMError> {
    renderer
        .stream_markdown_response(text, previous_line_count)
        .map_err(|err| map_render_error(provider_name, err))
}

// Provider-noise and harmony-control-token sanitization has been extracted to
// `stream_sanitization::StreamSanitizer`. The constants and helper functions
// that previously lived here (`HARMONY_MARKERS`, `contains_harmony_marker`,
// `sanitize_harmony_stream_text`, `sanitize_harmony_final_text`, etc.) now
// reside in that module, alongside MiniMax flat-noise stripping.

#[async_trait]
pub(crate) trait CopilotRuntimeRequestHandler: Send {
    async fn handle_runtime_request(
        &mut self,
        renderer: &mut AnsiRenderer,
        request: CopilotRuntimeRequest,
    ) -> Result<(), uni::LLMError>;
}

fn normalized_to_legacy_stream(
    mut stream: uni::LLMNormalizedStream,
) -> (uni::LLMStream, mpsc::Receiver<StreamProgressEvent>) {
    let (progress_tx, progress_rx) = mpsc::channel(256);
    let stream = try_stream! {
        let mut pending_usage = None;
        let mut provider_activity_reported = false;

        while let Some(event) = stream.next().await {
            match event? {
                NormalizedStreamEvent::TextDelta { delta } => {
                    yield LLMStreamEvent::Token { delta };
                }
                NormalizedStreamEvent::ReasoningDelta { delta, source } if source.is_public_summary() => {
                    yield LLMStreamEvent::Reasoning { delta };
                }
                NormalizedStreamEvent::ReasoningDelta { .. } => {
                    if !provider_activity_reported {
                        provider_activity_reported = true;
                        let _ = progress_tx.send(StreamProgressEvent::ProviderActivity).await;
                    }
                }
                NormalizedStreamEvent::ReasoningStage { stage } => {
                    yield LLMStreamEvent::ReasoningStage { stage };
                }
                NormalizedStreamEvent::ToolCallStart { call_id, name } => {
                    let _ = progress_tx.send(StreamProgressEvent::ToolCallStarted { call_id, name }).await;
                }
                NormalizedStreamEvent::ToolCallDelta { call_id, delta } => {
                    let _ = progress_tx.send(StreamProgressEvent::ToolCallDelta { call_id, delta }).await;
                }
                NormalizedStreamEvent::Usage { usage } => {
                    pending_usage = Some(usage);
                }
                NormalizedStreamEvent::Done { response } => {
                    let mut response = *response;
                    if response.usage.is_none() {
                        response.usage = pending_usage.take();
                    }
                    yield LLMStreamEvent::Completed {
                        response: Box::new(response),
                    };
                }
            }
        }
    };

    (Box::pin(stream), progress_rx)
}

#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]
pub(crate) async fn stream_and_render_response_with_options_impl(
    provider: &dyn uni::LLMProvider,
    request: uni::LLMRequest,
    spinner: &PlaceholderSpinner,
    renderer: &mut AnsiRenderer,
    ctrl_c_state: &Arc<CtrlCState>,
    ctrl_c_notify: &Arc<Notify>,
    options: StreamSpinnerOptions,
    on_progress: Option<&mut (dyn FnMut(StreamProgressEvent) + Send)>,
) -> Result<(uni::LLMResponse, bool), uni::LLMError> {
    stream_and_render_response_with_options_impl_first_progress_timeout(
        provider,
        request,
        None,
        spinner,
        renderer,
        ctrl_c_state,
        ctrl_c_notify,
        options,
        on_progress,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]
pub(crate) async fn stream_and_render_response_with_options_impl_first_progress_timeout(
    provider: &dyn uni::LLMProvider,
    request: uni::LLMRequest,
    first_progress_timeout: Option<Duration>,
    spinner: &PlaceholderSpinner,
    renderer: &mut AnsiRenderer,
    ctrl_c_state: &Arc<CtrlCState>,
    ctrl_c_notify: &Arc<Notify>,
    options: StreamSpinnerOptions,
    on_progress: Option<&mut (dyn FnMut(StreamProgressEvent) + Send)>,
) -> Result<(uni::LLMResponse, bool), uni::LLMError> {
    let provider_name = provider.name();

    if ctrl_c_state.is_cancel_requested() {
        spinner.finish_with_restore(true);
        return Err(uni::LLMError::Provider {
            message: error_display::format_llm_error(provider_name, "Interrupted by user"),
            metadata: None,
        });
    }

    let stream_future = provider.stream_normalized(request);
    tokio::pin!(stream_future);
    let first_progress_timeout = first_progress_timeout.map(FirstProgressTimeout::starting_now);

    if ctrl_c_state.is_cancel_requested() || ctrl_c_state.is_exit_requested() {
        spinner.finish_with_restore(true);
        return Err(uni::LLMError::Provider {
            message: error_display::format_llm_error(provider_name, "Interrupted by user"),
            metadata: None,
        });
    }

    let normalized_stream = tokio::select! {
        biased;
        _ = ctrl_c_notify.notified() => {
            spinner.finish_with_restore(true);
            return Err(uni::LLMError::Provider { message: error_display::format_llm_error(provider_name, "Interrupted by user"), metadata: None });
        }
        _ = async {
            match first_progress_timeout {
                Some(timeout) => tokio::time::sleep_until(timeout.deadline).await,
                None => std::future::pending().await,
            }
        } => {
            spinner.finish_with_restore(true);
            return Err(first_progress_timeout_error(
                provider_name,
                first_progress_timeout.map_or(Duration::ZERO, |timeout| timeout.budget),
            ));
        }
        result = stream_future => result?,
    };
    let (mut stream, mut progress_events) = normalized_to_legacy_stream(normalized_stream);

    render_stream_with_options_and_progress_impl(
        provider_name,
        &mut stream,
        Some(&mut progress_events),
        first_progress_timeout,
        spinner,
        renderer,
        ctrl_c_state,
        ctrl_c_notify,
        options,
        on_progress,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]
pub(crate) async fn render_stream_with_options_and_progress_impl(
    provider_name: &str,
    stream: &mut uni::BorrowedLLMStream<'_>,
    progress_events: Option<&mut mpsc::Receiver<StreamProgressEvent>>,
    first_progress_timeout: Option<FirstProgressTimeout>,
    spinner: &PlaceholderSpinner,
    renderer: &mut AnsiRenderer,
    ctrl_c_state: &Arc<CtrlCState>,
    ctrl_c_notify: &Arc<Notify>,
    options: StreamSpinnerOptions,
    on_progress: Option<&mut (dyn FnMut(StreamProgressEvent) + Send)>,
) -> Result<(uni::LLMResponse, bool), uni::LLMError> {
    render_stream_with_options_and_copilot_runtime_impl(
        provider_name,
        stream,
        progress_events,
        None,
        None,
        first_progress_timeout,
        spinner,
        renderer,
        ctrl_c_state,
        ctrl_c_notify,
        options,
        on_progress,
    )
    .await
}

fn is_output_suppressed(options: &StreamSpinnerOptions) -> bool {
    options.suppress_output
        || options
            .suppress_output_signal
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::Acquire))
}

/// Forward a plan collected from the stream into the completed response so
/// response processing can validate and persist it through the normal planning
/// workflow. Streaming removes plan markup before rendering, while providers
/// may omit the same text from their completion payload; the semantic handoff
/// must therefore happen before any rendering-suppressed return.
fn merge_streamed_plan_into_response(
    response: &mut uni::LLMResponse,
    streamed_plan_text: Option<String>,
    streamed_visible_content: &str,
) {
    let Some(plan_text) = streamed_plan_text.filter(|text| !text.trim().is_empty()) else {
        return;
    };

    let mut content = response
        .content
        .take()
        .filter(|text| !text.trim().is_empty())
        .or_else(|| (!streamed_visible_content.trim().is_empty()).then(|| streamed_visible_content.to_string()))
        .unwrap_or_default();

    if extract_any_plan(&content).plan_text.is_some() {
        response.content = Some(content);
        return;
    }

    if !content.trim().is_empty() {
        content.push_str("\n\n");
    }
    content.push_str("<proposed_plan>\n");
    content.push_str(plan_text.trim());
    content.push_str("\n</proposed_plan>");
    response.content = Some(content);
}

#[expect(
    clippy::too_many_arguments,
    reason = "Intentional compatibility, platform, test, or API-shape suppression."
)]
pub(crate) async fn render_stream_with_options_and_copilot_runtime_impl(
    provider_name: &str,
    stream: &mut uni::BorrowedLLMStream<'_>,
    progress_events: Option<&mut mpsc::Receiver<StreamProgressEvent>>,
    runtime_requests: Option<&mut mpsc::UnboundedReceiver<CopilotRuntimeRequest>>,
    mut runtime_handler: Option<&mut dyn CopilotRuntimeRequestHandler>,
    first_progress_timeout: Option<FirstProgressTimeout>,
    spinner: &PlaceholderSpinner,
    renderer: &mut AnsiRenderer,
    ctrl_c_state: &Arc<CtrlCState>,
    ctrl_c_notify: &Arc<Notify>,
    options: StreamSpinnerOptions,
    mut on_progress: Option<&mut (dyn FnMut(StreamProgressEvent) + Send)>,
) -> Result<(uni::LLMResponse, bool), uni::LLMError> {
    if ctrl_c_state.is_cancel_requested() {
        spinner.finish_with_restore(true);
        return Err(uni::LLMError::Provider {
            message: error_display::format_llm_error(provider_name, "Interrupted by user"),
            metadata: None,
        });
    }

    let supports_streaming_markdown = renderer.supports_streaming_markdown();
    let stream_reasoning_deltas = supports_streaming_markdown && renderer.reasoning_visible();
    let mut final_response: Option<uni::LLMResponse> = None;
    let mut aggregated = String::new();
    let mut spinner_active = true;
    let mut progress_events = progress_events;
    let mut runtime_requests = runtime_requests;
    let mut rendered_line_count = 0usize;
    let finish_spinner = |active: &mut bool, force: bool| {
        if *active {
            if force {
                spinner.finish_with_restore(true);
                *active = false;
            } else if !options.defer_finish {
                spinner.finish();
                *active = false;
            }
        }
    };
    let mut emitted_tokens = false;
    let mut visible_output_observed = false;
    let mut reasoning_state = StreamingReasoningState::new(stream_reasoning_deltas);
    let mut spinner_message_updated = false;
    let mut reasoning_accumulated = String::new();
    let mut pending_content = String::new();
    let mut content_suppressed = false;
    const MAX_PENDING_CONTENT_BYTES: usize = 4_096;
    const STREAM_RENDER_MIN_INTERVAL: Duration = Duration::from_millis(16);
    const STREAM_RENDER_MAX_BYTES: usize = 384;
    const REASONING_RENDER_MAX_BYTES: usize = 256;
    let mut pending_render_bytes = 0usize;
    let mut last_render_at = Instant::now();
    let mut pending_reasoning_delta = String::new();
    let mut pending_reasoning_render_bytes = 0usize;
    let mut last_reasoning_render_at = Instant::now();

    let mut suppress_reasoning_due_to_duplication = false;
    let mut plan_parser = options.strip_proposed_plan_blocks.then(ProposedPlanStreamParser::new);
    let mut streamed_plan_text = None;

    let mut token_count = 0;
    let mut reasoning_token_count = 0;
    let mut last_progress_update = Instant::now();
    let mut reasoning_emitted = false;
    let mut stream_sanitizer = StreamSanitizer::new();
    let mut first_progress_timeout = first_progress_timeout;
    let mut first_wire_event_reported = false;

    loop {
        if ctrl_c_state.is_cancel_requested() || ctrl_c_state.is_exit_requested() {
            finish_spinner(&mut spinner_active, true);
            reasoning_state
                .handle_stream_failure(renderer)
                .map_err(|err| map_render_error(provider_name, err))?;
            return Err(uni::LLMError::Provider {
                message: error_display::format_llm_error(provider_name, "Interrupted by user"),
                metadata: None,
            });
        }

        let maybe_event = tokio::select! {
            biased;
            _ = ctrl_c_notify.notified() => {
                finish_spinner(&mut spinner_active, true);
                reasoning_state
                    .handle_stream_failure(renderer)
                    .map_err(|err| map_render_error(provider_name, err))?;
                return Err(uni::LLMError::Provider { message: error_display::format_llm_error(provider_name, "Interrupted by user"), metadata: None });
            }
            _ = async {
                match first_progress_timeout {
                    Some(timeout) => tokio::time::sleep_until(timeout.deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                finish_spinner(&mut spinner_active, true);
                reasoning_state
                    .handle_stream_failure(renderer)
                    .map_err(|err| map_render_error(provider_name, err))?;
                return Err(first_progress_timeout_error(
                    provider_name,
                    first_progress_timeout.map_or(Duration::ZERO, |timeout| timeout.budget),
                ));
            }
            request = async {
                match runtime_requests.as_deref_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match request {
                    Some(request) => {
                        first_progress_timeout = None;
                        finish_spinner(&mut spinner_active, true);
                        let Some(handler) = runtime_handler.as_deref_mut() else {
                            return Err(uni::LLMError::Provider {
                                message: error_display::format_llm_error(
                                    provider_name,
                                    "Copilot runtime request arrived without a VT Code handler",
                                ),
                                metadata: None,
                            });
                        };
                        if let Some(callback) = on_progress.as_deref_mut() {
                            callback(StreamProgressEvent::ProviderActivity);
                        }
                        handler.handle_runtime_request(renderer, request).await?;
                        // Return status mapping to the caller after tool/approval work.
                        if let Some(callback) = on_progress.as_deref_mut() {
                            callback(StreamProgressEvent::ProviderActivity);
                        }
                        continue;
                    }
                    None => {
                        runtime_requests = None;
                        continue;
                    }
                }
            }
            progress_event = async {
                match progress_events.as_deref_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match progress_event {
                    Some(StreamProgressEvent::ToolCallStarted { call_id, name }) => {
                        first_progress_timeout = None;
                        finish_spinner(&mut spinner_active, false);
                        if let Some(tool_name) = name.as_deref().filter(|value| !value.is_empty()) {
                            spinner.update_message(format!("Preparing tool call: {tool_name}"));
                            spinner_message_updated = true;
                        }
                        if let Some(callback) = on_progress.as_deref_mut() {
                            callback(StreamProgressEvent::ToolCallStarted { call_id, name });
                        }
                        continue;
                    }
                    Some(StreamProgressEvent::ToolCallDelta { call_id, delta }) => {
                        first_progress_timeout = None;
                        if let Some(callback) = on_progress.as_deref_mut() {
                            callback(StreamProgressEvent::ToolCallDelta { call_id, delta });
                        }
                        continue;
                    }
                    Some(
                        StreamProgressEvent::OutputDelta(_)
                        | StreamProgressEvent::ReasoningDelta(_)
                        | StreamProgressEvent::ReasoningStage(_),
                    ) => {
                        first_progress_timeout = None;
                        continue;
                    }
                    Some(StreamProgressEvent::ProviderActivity) => {
                        first_progress_timeout = None;
                        if let Some(callback) = on_progress.as_deref_mut() {
                            callback(StreamProgressEvent::ProviderActivity);
                        }
                        continue;
                    }
                    None => {
                        progress_events = None;
                        continue;
                    }
                }
            }
            event = stream.next() => event,
        };

        let Some(event_result) = maybe_event else {
            break;
        };

        if event_result.is_ok() && !first_wire_event_reported {
            first_wire_event_reported = true;
            if let Some(callback) = on_progress.as_deref_mut() {
                callback(StreamProgressEvent::ProviderActivity);
            }
        }
        match event_result {
            Ok(LLMStreamEvent::Token { delta }) => {
                first_progress_timeout = None;
                token_count += 1;
                let output_suppressed = is_output_suppressed(&options);
                let mut visible_delta = if let Some(parser) = plan_parser.as_mut() {
                    parser.consume(&delta)
                } else {
                    delta
                };

                let sanitized = stream_sanitizer.process_delta(&visible_delta);
                visible_delta = sanitized.visible_delta;
                let has_aggregated_override = sanitized.aggregated_override.is_some();
                if let Some(cleaned_aggregated) = sanitized.aggregated_override {
                    aggregated = cleaned_aggregated;
                }

                if stream_reasoning_deltas && !output_suppressed && !pending_reasoning_delta.is_empty() {
                    flush_pending_reasoning(
                        provider_name,
                        renderer,
                        &mut reasoning_state,
                        &mut on_progress,
                        &mut pending_reasoning_delta,
                        &mut pending_reasoning_render_bytes,
                        &mut last_reasoning_render_at,
                        &mut reasoning_emitted,
                    )?;
                }
                if !output_suppressed
                    && !reasoning_emitted
                    && reasoning_token_count > 0
                    && !reasoning_state.is_deferred()
                {
                    let rendered = reasoning_state
                        .flush_pending(renderer)
                        .map_err(|err| map_render_error(provider_name, err))?;
                    if rendered {
                        reasoning_emitted = true;
                    }
                }

                let is_drafting_plan = plan_parser.as_ref().is_some_and(|parser| parser.is_drafting_plan());

                if !spinner_message_updated {
                    if is_drafting_plan {
                        let len = plan_parser.as_ref().map(|parser| parser.drafted_plan_len()).unwrap_or(0);
                        spinner.update_message(format!("Drafting plan... ({len} chars)"));
                    } else {
                        spinner.update_message("Receiving response...");
                    }
                    spinner_message_updated = true;
                } else if is_drafting_plan {
                    if last_progress_update.elapsed() >= Duration::from_millis(200) {
                        let len = plan_parser.as_ref().map(|parser| parser.drafted_plan_len()).unwrap_or(0);
                        spinner.update_message(format!("Drafting plan... ({len} chars)"));
                        last_progress_update = Instant::now();
                    }
                } else if last_progress_update.elapsed() >= Duration::from_millis(500) {
                    spinner.update_message(format!("Receiving response... ({token_count} tokens)"));
                    last_progress_update = Instant::now();
                }
                finish_spinner(&mut spinner_active, false);
                if visible_delta.is_empty() {
                    continue;
                }
                if output_suppressed {
                    if !has_aggregated_override {
                        aggregated.push_str(&visible_delta);
                    }
                    continue;
                }
                if let Some(callback) = on_progress.as_deref_mut() {
                    callback(StreamProgressEvent::OutputDelta(visible_delta.clone()));
                    visible_output_observed |= !visible_delta.trim().is_empty();
                }
                if !supports_streaming_markdown && !reasoning_accumulated.trim().is_empty() && !emitted_tokens {
                    pending_content.push_str(&visible_delta);
                    if pending_content.len() >= MAX_PENDING_CONTENT_BYTES {
                        aggregated.push_str(&pending_content);
                        pending_content.clear();
                    }
                    continue;
                }

                if !has_aggregated_override {
                    aggregated.push_str(&visible_delta);
                }
                if supports_streaming_markdown {
                    pending_render_bytes = pending_render_bytes.saturating_add(visible_delta.len());
                    let should_render_now = !emitted_tokens
                        || visible_delta.contains('\n')
                        || pending_render_bytes >= STREAM_RENDER_MAX_BYTES
                        || last_render_at.elapsed() >= STREAM_RENDER_MIN_INTERVAL;
                    if should_render_now {
                        rendered_line_count = stream_markdown_with_provider_error(
                            provider_name,
                            renderer,
                            &aggregated,
                            rendered_line_count,
                        )?;
                        emitted_tokens = true;
                        pending_render_bytes = 0;
                        last_render_at = Instant::now();
                    }
                }
            }
            Ok(LLMStreamEvent::Reasoning { delta }) => {
                first_progress_timeout = None;
                reasoning_token_count += 1;
                let output_suppressed = is_output_suppressed(&options);
                if !spinner_message_updated {
                    spinner.update_message("Processing reasoning...");
                    spinner_message_updated = true;
                } else if last_progress_update.elapsed() >= Duration::from_millis(500) {
                    spinner.update_message(format!("Processing reasoning... ({reasoning_token_count} tokens)"));
                    last_progress_update = Instant::now();
                }
                finish_spinner(&mut spinner_active, false);
                reasoning_accumulated.push_str(&delta);
                if stream_reasoning_deltas && !output_suppressed {
                    pending_reasoning_delta.push_str(&delta);
                    pending_reasoning_render_bytes = pending_reasoning_render_bytes.saturating_add(delta.len());
                    let should_render_now = !reasoning_emitted
                        || delta.contains('\n')
                        || pending_reasoning_render_bytes >= REASONING_RENDER_MAX_BYTES
                        || last_reasoning_render_at.elapsed() >= STREAM_RENDER_MIN_INTERVAL;
                    if should_render_now {
                        flush_pending_reasoning(
                            provider_name,
                            renderer,
                            &mut reasoning_state,
                            &mut on_progress,
                            &mut pending_reasoning_delta,
                            &mut pending_reasoning_render_bytes,
                            &mut last_reasoning_render_at,
                            &mut reasoning_emitted,
                        )?;
                    }
                }
            }
            Ok(LLMStreamEvent::ReasoningStage { stage }) => {
                first_progress_timeout = None;
                if stream_reasoning_deltas && !is_output_suppressed(&options) && !pending_reasoning_delta.is_empty() {
                    flush_pending_reasoning(
                        provider_name,
                        renderer,
                        &mut reasoning_state,
                        &mut on_progress,
                        &mut pending_reasoning_delta,
                        &mut pending_reasoning_render_bytes,
                        &mut last_reasoning_render_at,
                        &mut reasoning_emitted,
                    )?;
                }
                if stream_reasoning_deltas {
                    if !is_output_suppressed(&options) {
                        if let Some(callback) = on_progress.as_deref_mut() {
                            callback(StreamProgressEvent::ReasoningStage(stage.clone()));
                        }
                        spinner.set_reasoning_stage(Some(stage));
                    }
                }
            }
            Ok(LLMStreamEvent::ReasoningSignature { .. }) => {
                // Signature field not currently processed in UI stream
            }
            Ok(LLMStreamEvent::Completed { response }) => {
                final_response = Some(*response);
            }
            Err(err) => {
                finish_spinner(&mut spinner_active, true);
                reasoning_state
                    .handle_stream_failure(renderer)
                    .map_err(|render_err| map_render_error(provider_name, render_err))?;
                return Err(err);
            }
        }
    }

    finish_spinner(&mut spinner_active, false);

    if !is_output_suppressed(&options) && stream_reasoning_deltas && !pending_reasoning_delta.is_empty() {
        let rendered = flush_pending_reasoning_delta(
            provider_name,
            renderer,
            &mut reasoning_state,
            &mut on_progress,
            &mut pending_reasoning_delta,
        )?;
        if rendered {
            reasoning_emitted = true;
        }
    }

    if let Some(parser) = plan_parser.as_mut() {
        let trailing_plan_parse = parser.finish();
        streamed_plan_text = trailing_plan_parse.plan_text;
        if !is_output_suppressed(&options) && !trailing_plan_parse.stripped_text.is_empty() {
            if let Some(callback) = on_progress.as_deref_mut() {
                callback(StreamProgressEvent::OutputDelta(trailing_plan_parse.stripped_text.clone()));
                visible_output_observed |= !trailing_plan_parse.stripped_text.trim().is_empty();
            }
            if !supports_streaming_markdown && !reasoning_accumulated.trim().is_empty() && !emitted_tokens {
                pending_content.push_str(&trailing_plan_parse.stripped_text);
                if pending_content.len() >= MAX_PENDING_CONTENT_BYTES {
                    aggregated.push_str(&pending_content);
                    pending_content.clear();
                }
            } else {
                aggregated.push_str(&trailing_plan_parse.stripped_text);
                if supports_streaming_markdown {
                    rendered_line_count =
                        stream_markdown_with_provider_error(provider_name, renderer, &aggregated, rendered_line_count)?;
                    emitted_tokens = true;
                }
            }
        }
    }

    if !is_output_suppressed(&options) && supports_streaming_markdown && pending_render_bytes > 0 {
        rendered_line_count =
            stream_markdown_with_provider_error(provider_name, renderer, &aggregated, rendered_line_count)?;
        emitted_tokens = true;
    }

    let mut response = match final_response {
        Some(response) => response,
        None => {
            reasoning_state
                .handle_stream_failure(renderer)
                .map_err(|err| map_render_error(provider_name, err))?;
            finish_spinner(&mut spinner_active, true);
            let formatted_error =
                error_display::format_llm_error(provider_name, "Stream ended without a completion event");
            return Err(uni::LLMError::Provider { message: formatted_error, metadata: None });
        }
    };

    let streamed_visible_content = if pending_content.is_empty() {
        Cow::Borrowed(aggregated.as_str())
    } else {
        let mut visible_content = String::with_capacity(aggregated.len() + pending_content.len());
        visible_content.push_str(&aggregated);
        visible_content.push_str(&pending_content);
        Cow::Owned(visible_content)
    };
    merge_streamed_plan_into_response(&mut response, streamed_plan_text, streamed_visible_content.as_ref());

    if is_output_suppressed(&options) {
        return Ok((response, false));
    }

    if !pending_content.is_empty() && !content_suppressed {
        let reasoning_for_compare = reasoning_accumulated.as_str();
        if !reasoning_for_compare.trim().is_empty()
            && reasoning_matches_content(reasoning_for_compare, &pending_content)
        {
            suppress_reasoning_due_to_duplication = true;
        }
    }

    if !pending_content.is_empty() && !content_suppressed {
        let prefix_len = common_prefix_len(&reasoning_accumulated, &pending_content);
        let reasoning_prefix = !reasoning_accumulated.is_empty() && prefix_len == reasoning_accumulated.len();
        let pending = std::mem::take(&mut pending_content);
        let render_text = if reasoning_prefix {
            pending.get(prefix_len..).unwrap_or("").to_string()
        } else {
            pending
        };

        if reasoning_prefix && render_text.is_empty() && (reasoning_state.rendered_reasoning() || reasoning_emitted) {
            content_suppressed = true;
        } else {
            aggregated.push_str(&render_text);
            if supports_streaming_markdown {
                let prev_count = if aggregated.trim().is_empty() {
                    0
                } else {
                    rendered_line_count
                };
                let _ = stream_markdown_with_provider_error(provider_name, renderer, &aggregated, prev_count)?;
                emitted_tokens = true;
            }
            if reasoning_prefix && (reasoning_state.rendered_reasoning() || reasoning_emitted) {
                content_suppressed = true;
            }
        }
    }

    let content_for_render = if options.strip_proposed_plan_blocks {
        response
            .content
            .as_deref()
            .map(extract_any_plan)
            .map(|extraction| strip_plan_persistence_policy_line(&extraction.stripped_text))
    } else {
        response.content.clone()
    };
    let content_for_render = content_for_render.map(|text| stream_sanitizer.finalize(&text));
    let has_renderable_content = content_for_render
        .as_deref()
        .map(|content| !content.trim().is_empty())
        .unwrap_or(false);

    if !content_suppressed && let Some(content) = content_for_render.as_deref() {
        let content_trimmed = content.trim();
        if !content_trimmed.is_empty() {
            let reasoning_dupes_content = reasoning_matches_content(reasoning_accumulated.as_str(), content);

            if reasoning_dupes_content {
                suppress_reasoning_due_to_duplication = true;
            }

            let already_rendered = supports_streaming_markdown
                && emitted_tokens
                && !aggregated.trim().is_empty()
                && aggregated.trim() == content_trimmed;

            reasoning_state
                .finalize(
                    renderer,
                    Some(reasoning_accumulated.as_str()),
                    reasoning_emitted,
                    suppress_reasoning_due_to_duplication,
                )
                .map_err(|err| map_render_error(provider_name, err))?;

            if !already_rendered {
                if supports_streaming_markdown {
                    let prev_count = if aggregated.trim().is_empty() {
                        0
                    } else {
                        rendered_line_count
                    };
                    let _ = stream_markdown_with_provider_error(provider_name, renderer, content, prev_count)?;
                } else {
                    renderer
                        .line(MessageStyle::Response, content)
                        .map_err(|err| map_render_error(provider_name, err))?;
                }
                if !visible_output_observed && let Some(callback) = on_progress {
                    callback(StreamProgressEvent::OutputDelta(content.to_owned()));
                }
                emitted_tokens = true;
                aggregated = content.to_string();
            }
        }
    }

    if !has_renderable_content || aggregated.trim().is_empty() || suppress_reasoning_due_to_duplication {
        let suppress_reasoning = suppress_reasoning_due_to_duplication;
        reasoning_state
            .finalize(renderer, Some(reasoning_accumulated.as_str()), reasoning_emitted, suppress_reasoning)
            .map_err(|err| map_render_error(provider_name, err))?;
    }

    let response_rendered = emitted_tokens || reasoning_emitted || reasoning_state.rendered_reasoning();

    Ok((response, response_rendered))
}

#[cfg(test)]
mod tests {
    use super::{
        CopilotRuntimeRequestHandler, FirstProgressTimeout, merge_streamed_plan_into_response,
        normalized_to_legacy_stream, render_stream_with_options_and_copilot_runtime_impl,
    };
    use crate::agent::runloop::unified::state::CtrlCState;
    use crate::agent::runloop::unified::ui_interaction::{
        PlaceholderSpinner, StreamProgressEvent, StreamSpinnerOptions,
    };
    use async_trait::async_trait;
    use futures::{StreamExt, stream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::{Notify, mpsc};
    use vtcode_core::copilot::{CopilotObservedToolCall, CopilotObservedToolCallStatus, CopilotRuntimeRequest};
    use vtcode_core::llm::provider::{self as uni, FinishReason, LLMResponse, LLMStreamEvent, ReasoningSource};
    use vtcode_core::utils::ansi::AnsiRenderer;
    use vtcode_ui::tui::app::{InlineCommand, InlineHandle};

    struct SleepingRuntimeHandler {
        sleep_for: Duration,
    }

    struct ToggleSuppressionRuntimeHandler {
        signal: Arc<AtomicBool>,
    }

    struct EnableSuppressionRuntimeHandler {
        signal: Arc<AtomicBool>,
    }

    #[async_trait]
    impl CopilotRuntimeRequestHandler for SleepingRuntimeHandler {
        async fn handle_runtime_request(
            &mut self,
            _renderer: &mut AnsiRenderer,
            _request: CopilotRuntimeRequest,
        ) -> Result<(), uni::LLMError> {
            tokio::time::sleep(self.sleep_for).await;
            Ok(())
        }
    }

    #[async_trait]
    impl CopilotRuntimeRequestHandler for ToggleSuppressionRuntimeHandler {
        async fn handle_runtime_request(
            &mut self,
            _renderer: &mut AnsiRenderer,
            _request: CopilotRuntimeRequest,
        ) -> Result<(), uni::LLMError> {
            self.signal.store(false, Ordering::Release);
            Ok(())
        }
    }

    #[async_trait]
    impl CopilotRuntimeRequestHandler for EnableSuppressionRuntimeHandler {
        async fn handle_runtime_request(
            &mut self,
            _renderer: &mut AnsiRenderer,
            _request: CopilotRuntimeRequest,
        ) -> Result<(), uni::LLMError> {
            self.signal.store(true, Ordering::Release);
            Ok(())
        }
    }

    fn build_spinner() -> PlaceholderSpinner {
        let (tx, _rx) = mpsc::unbounded_channel::<InlineCommand>();
        let handle = InlineHandle::new_for_tests(tx);
        PlaceholderSpinner::new(&handle, None, None, "")
    }

    fn completed_response_with_content(content: Option<&str>) -> LLMResponse {
        LLMResponse {
            content: content.map(str::to_string),
            model: "mock-model".to_string(),
            tool_calls: None,
            usage: None,
            finish_reason: FinishReason::Stop,
            reasoning: None,
            reasoning_details: None,
            organization_id: None,
            request_id: None,
            tool_references: vec![],
            compaction: None,
        }
    }

    #[tokio::test]
    async fn normalized_ui_stream_forwards_only_public_reasoning_summaries() {
        let normalized: uni::LLMNormalizedStream = Box::pin(stream::iter(vec![
            Ok(uni::NormalizedStreamEvent::ReasoningDelta {
                delta: "raw reasoning".to_string(),
                source: ReasoningSource::Raw,
            }),
            Ok(uni::NormalizedStreamEvent::ReasoningDelta {
                delta: "public summary".to_string(),
                source: ReasoningSource::ProviderSummary,
            }),
            Ok(uni::NormalizedStreamEvent::ReasoningDelta {
                delta: "continuation detail".to_string(),
                source: ReasoningSource::Continuation,
            }),
            Ok(uni::NormalizedStreamEvent::Done {
                response: Box::new(completed_response_with_content(None)),
            }),
        ]));
        let (mut legacy, _progress) = normalized_to_legacy_stream(normalized);
        let mut events = Vec::new();
        while let Some(event) = legacy.next().await {
            events.push(event.expect("normalized event should convert"));
        }

        assert!(matches!(
            events.as_slice(),
            [LLMStreamEvent::Reasoning { delta }, LLMStreamEvent::Completed { .. }]
                if delta == "public summary"
        ));
    }

    #[tokio::test]
    async fn hidden_reasoning_counts_as_first_progress_without_being_rendered() {
        let normalized: uni::LLMNormalizedStream = Box::pin(async_stream::stream! {
            yield Ok(uni::NormalizedStreamEvent::ReasoningDelta {
                delta: "private trace".to_string(),
                source: ReasoningSource::Raw,
            });
            tokio::time::sleep(Duration::from_millis(40)).await;
            yield Ok(uni::NormalizedStreamEvent::Done {
                response: Box::new(completed_response_with_content(Some("answer"))),
            });
        });
        let (mut legacy, mut progress) = normalized_to_legacy_stream(normalized);
        let spinner = build_spinner();
        let mut renderer = AnsiRenderer::stdout();
        let ctrl_c_state = Arc::new(CtrlCState::new());
        let ctrl_c_notify = Arc::new(Notify::new());

        let mut observed = Vec::new();
        let mut on_progress = |event| observed.push(event);
        let result = render_stream_with_options_and_copilot_runtime_impl(
            "mock",
            &mut legacy,
            Some(&mut progress),
            None,
            None,
            Some(FirstProgressTimeout::starting_now(Duration::from_millis(5))),
            &spinner,
            &mut renderer,
            &ctrl_c_state,
            &ctrl_c_notify,
            StreamSpinnerOptions::default(),
            Some(&mut on_progress),
        )
        .await;

        assert!(result.is_ok(), "hidden provider progress should clear the first-progress timeout");
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, StreamProgressEvent::ProviderActivity))
        );
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event, StreamProgressEvent::ReasoningDelta(_))),
            "private reasoning must never become visible output"
        );
    }

    fn completed_response(content: &str) -> LLMResponse {
        completed_response_with_content(Some(content))
    }

    const STREAMED_PLAN: &str = "- Step 1";

    fn count_plan_blocks(text: &str) -> usize {
        text.matches("<proposed_plan>").count()
    }

    fn collect_rendered_inline_text(command_rx: &mut mpsc::UnboundedReceiver<InlineCommand>) -> String {
        std::iter::from_fn(|| command_rx.try_recv().ok())
            .filter_map(|command| match command {
                InlineCommand::AppendLine { segments, .. } => {
                    Some(segments.into_iter().map(|segment| segment.text).collect::<String>())
                }
                InlineCommand::Inline { segment, .. } => Some(segment.text),
                InlineCommand::ReplaceLast { lines, .. } => Some(
                    lines
                        .into_iter()
                        .flat_map(|line| line.into_iter().map(|segment| segment.text))
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect::<String>()
    }

    #[tokio::test]
    async fn completion_output_is_observed_once_and_respects_suppression() {
        for inline in [false, true] {
            for streamed in [false, true] {
                for suppressed in [false, true] {
                    for content in ["answer", " \n", ""] {
                        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
                        let handle = InlineHandle::new_for_tests(command_tx);
                        let spinner = PlaceholderSpinner::new(&handle, None, None, "");
                        let mut renderer = if inline {
                            AnsiRenderer::with_inline_ui(handle, Default::default())
                        } else {
                            AnsiRenderer::stdout()
                        };
                        let mut events = Vec::new();
                        if streamed {
                            events.push(Ok(LLMStreamEvent::Token { delta: content.into() }));
                        }
                        events.push(Ok(LLMStreamEvent::Completed { response: Box::new(completed_response(content)) }));
                        let mut stream: uni::LLMStream = Box::pin(stream::iter(events));
                        let mut observed = Vec::new();
                        let mut on_progress = |event| {
                            if let StreamProgressEvent::OutputDelta(delta) = event {
                                observed.push(delta);
                            }
                        };
                        let (_, rendered) = render_stream_with_options_and_copilot_runtime_impl(
                            "mock",
                            &mut stream,
                            None,
                            None,
                            None,
                            None,
                            &spinner,
                            &mut renderer,
                            &Arc::new(CtrlCState::new()),
                            &Arc::new(Notify::new()),
                            StreamSpinnerOptions {
                                suppress_output: suppressed,
                                ..StreamSpinnerOptions::default()
                            },
                            Some(&mut on_progress),
                        )
                        .await
                        .unwrap();
                        let visible: Vec<_> = observed
                            .iter()
                            .map(String::as_str)
                            .filter(|text| !text.trim().is_empty())
                            .collect();
                        if content == "answer" && !suppressed {
                            assert_eq!(visible, ["answer"], "no duplicate completion observation after tokens");
                            assert!(rendered);
                            if inline {
                                assert!(collect_rendered_inline_text(&mut command_rx).contains("answer"));
                            }
                        } else {
                            assert!(visible.is_empty(), "blank or suppressed content is not visible output");
                        }
                        if suppressed {
                            assert!(!rendered);
                            assert!(observed.is_empty());
                            assert!(!collect_rendered_inline_text(&mut command_rx).contains("answer"));
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn completion_only_plan_observation_contains_only_visible_prose() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let handle = InlineHandle::new_for_tests(command_tx);
        let spinner = PlaceholderSpinner::new(&handle, None, None, "");
        let mut renderer = AnsiRenderer::with_inline_ui(handle, Default::default());
        let mut stream: uni::LLMStream = Box::pin(stream::iter([Ok(LLMStreamEvent::Completed {
            response: Box::new(completed_response("answer\n<proposed_plan>\n- hidden plan\n</proposed_plan>")),
        })]));
        let mut observed = Vec::new();
        let mut on_progress = |event| {
            if let StreamProgressEvent::OutputDelta(delta) = event {
                observed.push(delta);
            }
        };
        render_stream_with_options_and_copilot_runtime_impl(
            "mock",
            &mut stream,
            None,
            None,
            None,
            None,
            &spinner,
            &mut renderer,
            &Arc::new(CtrlCState::new()),
            &Arc::new(Notify::new()),
            StreamSpinnerOptions {
                strip_proposed_plan_blocks: true,
                ..StreamSpinnerOptions::default()
            },
            Some(&mut on_progress),
        )
        .await
        .unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].trim(), "answer");
        let rendered = collect_rendered_inline_text(&mut command_rx);
        assert!(rendered.contains("answer"));
        assert!(!rendered.contains("hidden plan"));
    }

    #[tokio::test]
    async fn split_streamed_plan_is_forwarded_into_completed_response() {
        let spinner = build_spinner();
        let mut renderer = AnsiRenderer::stdout();
        let ctrl_c_state = Arc::new(CtrlCState::new());
        let ctrl_c_notify = Arc::new(Notify::new());
        let mut stream: uni::LLMStream = Box::pin(async_stream::stream! {
            yield Ok(LLMStreamEvent::Token { delta: "Intro\n<propo".to_string() });
            yield Ok(LLMStreamEvent::Token {
                delta: "sed_plan>\n- Step 1\n</proposed_plan>\nOutro".to_string(),
            });
            yield Ok(LLMStreamEvent::Completed {
                response: Box::new(completed_response_with_content(None)),
            });
        });

        let (response, _) = render_stream_with_options_and_copilot_runtime_impl(
            "mock",
            &mut stream,
            None,
            None,
            None,
            None,
            &spinner,
            &mut renderer,
            &ctrl_c_state,
            &ctrl_c_notify,
            StreamSpinnerOptions {
                strip_proposed_plan_blocks: true,
                ..StreamSpinnerOptions::default()
            },
            None,
        )
        .await
        .expect("stream should return its completed response");

        let response_content = response
            .content
            .as_deref()
            .expect("streamed plan should be merged into content");
        assert_eq!(count_plan_blocks(response_content), 1);
        assert!(response_content.contains("Intro"));
        assert!(response_content.contains(STREAMED_PLAN));
        assert!(response_content.contains("</proposed_plan>"));
    }

    #[tokio::test]
    async fn split_streamed_alternate_plan_is_forwarded_as_canonical_plan() {
        let spinner = build_spinner();
        let mut renderer = AnsiRenderer::stdout();
        let ctrl_c_state = Arc::new(CtrlCState::new());
        let ctrl_c_notify = Arc::new(Notify::new());
        let mut stream: uni::LLMStream = Box::pin(async_stream::stream! {
            yield Ok(LLMStreamEvent::Token { delta: "Intro\n<pl".to_string() });
            yield Ok(LLMStreamEvent::Token {
                delta: "an>\n- Step 1\n</plan>\nOutro".to_string(),
            });
            yield Ok(LLMStreamEvent::Completed {
                response: Box::new(completed_response_with_content(None)),
            });
        });

        let (response, _) = render_stream_with_options_and_copilot_runtime_impl(
            "mock",
            &mut stream,
            None,
            None,
            None,
            None,
            &spinner,
            &mut renderer,
            &ctrl_c_state,
            &ctrl_c_notify,
            StreamSpinnerOptions {
                strip_proposed_plan_blocks: true,
                ..StreamSpinnerOptions::default()
            },
            None,
        )
        .await
        .expect("stream should return its completed response");

        let response_content = response
            .content
            .as_deref()
            .expect("alternate streamed plan should be merged into content");
        assert_eq!(count_plan_blocks(response_content), 1);
        assert!(!response_content.contains("<plan>"));
        assert!(response_content.contains(STREAMED_PLAN));
        assert!(response_content.contains("</proposed_plan>"));
    }

    #[tokio::test]
    async fn suppressed_stream_still_forwards_streamed_plan_without_rendering() {
        let spinner = build_spinner();
        let mut renderer = AnsiRenderer::stdout();
        let ctrl_c_state = Arc::new(CtrlCState::new());
        let ctrl_c_notify = Arc::new(Notify::new());
        let mut stream: uni::LLMStream = Box::pin(async_stream::stream! {
            yield Ok(LLMStreamEvent::Token {
                delta: "<proposed_plan>\n- Step 1\n</proposed_plan>".to_string(),
            });
            yield Ok(LLMStreamEvent::Completed {
                response: Box::new(completed_response_with_content(None)),
            });
        });

        let (response, rendered) = render_stream_with_options_and_copilot_runtime_impl(
            "mock",
            &mut stream,
            None,
            None,
            None,
            None,
            &spinner,
            &mut renderer,
            &ctrl_c_state,
            &ctrl_c_notify,
            StreamSpinnerOptions {
                strip_proposed_plan_blocks: true,
                suppress_output: true,
                ..StreamSpinnerOptions::default()
            },
            None,
        )
        .await
        .expect("suppressed stream should return its completed response");

        assert!(!rendered, "suppression must still prevent rendering");
        let response_content = response.content.as_deref().expect("suppressed plan should be forwarded");
        assert_eq!(count_plan_blocks(response_content), 1);
        assert!(response_content.contains(STREAMED_PLAN));
    }

    #[tokio::test]
    async fn streamed_prose_remains_visible_without_plan_markup() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let handle = InlineHandle::new_for_tests(command_tx);
        let spinner = PlaceholderSpinner::new(&handle, None, None, "");
        let mut renderer = AnsiRenderer::with_inline_ui(handle, Default::default());
        let ctrl_c_state = Arc::new(CtrlCState::new());
        let ctrl_c_notify = Arc::new(Notify::new());
        let mut stream: uni::LLMStream = Box::pin(async_stream::stream! {
            yield Ok(LLMStreamEvent::Token {
                delta: "Intro\n<proposed_plan>\n- Step 1\n</proposed_plan>\nOutro".to_string(),
            });
            yield Ok(LLMStreamEvent::Completed {
                response: Box::new(completed_response_with_content(Some("Intro\n\nOutro"))),
            });
        });

        let (response, rendered) = render_stream_with_options_and_copilot_runtime_impl(
            "mock",
            &mut stream,
            None,
            None,
            None,
            None,
            &spinner,
            &mut renderer,
            &ctrl_c_state,
            &ctrl_c_notify,
            StreamSpinnerOptions {
                strip_proposed_plan_blocks: true,
                ..StreamSpinnerOptions::default()
            },
            None,
        )
        .await
        .expect("stream should render its completed response");

        assert!(rendered);
        let response_content = response.content.as_deref().expect("response content should be present");
        assert_eq!(count_plan_blocks(response_content), 1);
        let rendered_text = collect_rendered_inline_text(&mut command_rx);
        assert!(rendered_text.contains("Intro"), "rendered text: {rendered_text:?}");
        assert!(rendered_text.contains("Outro"), "rendered text: {rendered_text:?}");
        assert!(!rendered_text.contains("<proposed_plan>"));
        assert!(!rendered_text.contains(STREAMED_PLAN));
    }

    #[test]
    fn completed_plan_block_is_not_duplicated_during_stream_handoff() {
        let mut response =
            completed_response_with_content(Some("Intro\n<proposed_plan>\n- Completed step\n</proposed_plan>\nOutro"));

        merge_streamed_plan_into_response(&mut response, Some(STREAMED_PLAN.to_string()), "Intro\n\nOutro");

        let content = response.content.expect("completed response content should be preserved");
        assert_eq!(count_plan_blocks(&content), 1);
        assert!(content.contains("- Completed step"));
        assert!(!content.contains(STREAMED_PLAN));
    }

    #[test]
    fn completed_content_takes_precedence_and_alternate_plan_is_not_duplicated() {
        let mut response = completed_response_with_content(Some("completed prose"));
        merge_streamed_plan_into_response(&mut response, Some(STREAMED_PLAN.to_string()), "streamed prose");
        let content = response.content.expect("completed content should be preserved");
        assert!(content.starts_with("completed prose"));
        assert!(!content.contains("streamed prose"));
        assert_eq!(count_plan_blocks(&content), 1);

        let mut response = completed_response_with_content(Some("Intro\n<plan>\n- Existing step\n</plan>\nOutro"));
        merge_streamed_plan_into_response(&mut response, Some(STREAMED_PLAN.to_string()), "visible prose");
        let content = response.content.expect("alternate plan content should be preserved");
        assert_eq!(content.matches("<plan>").count(), 1);
        assert_eq!(count_plan_blocks(&content), 0);
        assert!(content.contains("- Existing step"));
        assert!(!content.contains(STREAMED_PLAN));
    }

    #[tokio::test]
    async fn copilot_runtime_request_counts_as_first_progress() {
        let spinner = build_spinner();
        let mut renderer = AnsiRenderer::stdout();
        let ctrl_c_state = Arc::new(CtrlCState::new());
        let ctrl_c_notify = Arc::new(Notify::new());

        let mut stream: uni::LLMStream = Box::pin(stream::once(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(LLMStreamEvent::Completed { response: Box::new(completed_response("ok")) })
        }));

        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        runtime_tx
            .send(CopilotRuntimeRequest::ObservedToolCall(CopilotObservedToolCall {
                tool_call_id: "call_1".to_string(),
                tool_name: "copilot_tool".to_string(),
                status: CopilotObservedToolCallStatus::Pending,
                arguments: None,
                output: None,
                terminal_id: None,
            }))
            .expect("send runtime request");
        drop(runtime_tx);

        let mut handler = SleepingRuntimeHandler { sleep_for: Duration::from_millis(40) };

        let result = render_stream_with_options_and_copilot_runtime_impl(
            "copilot",
            &mut stream,
            None,
            Some(&mut runtime_rx),
            Some(&mut handler),
            Some(FirstProgressTimeout::starting_now(Duration::from_millis(20))),
            &spinner,
            &mut renderer,
            &ctrl_c_state,
            &ctrl_c_notify,
            StreamSpinnerOptions::default(),
            None,
        )
        .await;

        assert!(result.is_ok(), "runtime request should clear the first-progress timeout");
    }

    #[tokio::test]
    async fn stream_times_out_before_first_progress() {
        let spinner = build_spinner();
        let mut renderer = AnsiRenderer::stdout();
        let ctrl_c_state = Arc::new(CtrlCState::new());
        let ctrl_c_notify = Arc::new(Notify::new());

        let mut stream: uni::LLMStream = Box::pin(stream::once(async {
            tokio::time::sleep(Duration::from_millis(40)).await;
            Ok(LLMStreamEvent::Completed { response: Box::new(completed_response("ok")) })
        }));

        let result = render_stream_with_options_and_copilot_runtime_impl(
            "mock",
            &mut stream,
            None,
            None,
            None,
            Some(FirstProgressTimeout::starting_now(Duration::from_millis(5))),
            &spinner,
            &mut renderer,
            &ctrl_c_state,
            &ctrl_c_notify,
            StreamSpinnerOptions::default(),
            None,
        )
        .await;

        let err = result.expect_err("stream should time out before first progress");
        assert!(err.to_string().contains("first token timed out"));
    }

    #[tokio::test]
    async fn dynamic_suppression_hides_output_until_copilot_verification_clears_it() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let handle = InlineHandle::new_for_tests(command_tx);
        let spinner = PlaceholderSpinner::new(&handle, None, None, "");
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        let ctrl_c_state = Arc::new(CtrlCState::new());
        let ctrl_c_notify = Arc::new(Notify::new());
        let suppress_output_signal = Arc::new(AtomicBool::new(true));
        let handler_signal = suppress_output_signal.clone();

        let mut stream: uni::LLMStream = Box::pin(async_stream::stream! {
            yield Ok(LLMStreamEvent::Token { delta: "hidden before verification".to_string() });
            tokio::time::sleep(Duration::from_millis(20)).await;
            yield Ok(LLMStreamEvent::Completed { response: Box::new(completed_response("final response")) });
        });
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            runtime_tx
                .send(CopilotRuntimeRequest::ObservedToolCall(CopilotObservedToolCall {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "apply_patch".to_string(),
                    status: CopilotObservedToolCallStatus::Completed,
                    arguments: None,
                    output: None,
                    terminal_id: None,
                }))
                .expect("send runtime request");
        });

        let mut handler = ToggleSuppressionRuntimeHandler { signal: handler_signal };
        let mut output_deltas = Vec::new();
        let mut on_progress = |event| {
            if let StreamProgressEvent::OutputDelta(delta) = event {
                output_deltas.push(delta);
            }
        };

        let (response, rendered) = render_stream_with_options_and_copilot_runtime_impl(
            "copilot",
            &mut stream,
            None,
            Some(&mut runtime_rx),
            Some(&mut handler),
            None,
            &spinner,
            &mut renderer,
            &ctrl_c_state,
            &ctrl_c_notify,
            StreamSpinnerOptions {
                suppress_output_signal: Some(suppress_output_signal),
                ..StreamSpinnerOptions::default()
            },
            Some(&mut on_progress),
        )
        .await
        .expect("Copilot stream should return its final response");

        assert_eq!(response.content.as_deref(), Some("final response"));
        assert!(rendered);
        assert_eq!(output_deltas, ["final response"], "only the visible completion may trigger OutputDelta");

        let rendered_text = std::iter::from_fn(|| command_rx.try_recv().ok())
            .filter_map(|command| match command {
                InlineCommand::AppendLine { segments, .. } => {
                    Some(segments.into_iter().map(|segment| segment.text).collect::<String>())
                }
                InlineCommand::Inline { segment, .. } => Some(segment.text),
                InlineCommand::ReplaceLast { lines, .. } => Some(
                    lines
                        .into_iter()
                        .flat_map(|line| line.into_iter().map(|segment| segment.text))
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect::<String>();
        assert!(rendered_text.contains("final response"));
        assert!(!rendered_text.contains("hidden before verification"));
    }

    #[tokio::test]
    async fn dynamic_suppression_does_not_flush_pending_reasoning_after_mutation() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let handle = InlineHandle::new_for_tests(command_tx);
        let spinner = PlaceholderSpinner::new(&handle, None, None, "");
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        renderer.set_reasoning_visible(true);
        let ctrl_c_state = Arc::new(CtrlCState::new());
        let ctrl_c_notify = Arc::new(Notify::new());
        let suppress_output_signal = Arc::new(AtomicBool::new(false));
        let handler_signal = suppress_output_signal.clone();

        let mut stream: uni::LLMStream = Box::pin(async_stream::stream! {
            yield Ok(LLMStreamEvent::Reasoning { delta: "visible reasoning".to_string() });
            yield Ok(LLMStreamEvent::Reasoning { delta: "hidden pending reasoning".to_string() });
            tokio::time::sleep(Duration::from_millis(20)).await;
            yield Ok(LLMStreamEvent::ReasoningStage { stage: "tool execution".to_string() });
            yield Ok(LLMStreamEvent::Completed { response: Box::new(completed_response("final response")) });
        });
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            runtime_tx
                .send(CopilotRuntimeRequest::ObservedToolCall(CopilotObservedToolCall {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "apply_patch".to_string(),
                    status: CopilotObservedToolCallStatus::Completed,
                    arguments: None,
                    output: None,
                    terminal_id: None,
                }))
                .expect("send runtime request");
        });

        let mut handler = EnableSuppressionRuntimeHandler { signal: handler_signal };
        let (response, rendered) = render_stream_with_options_and_copilot_runtime_impl(
            "copilot",
            &mut stream,
            None,
            Some(&mut runtime_rx),
            Some(&mut handler),
            None,
            &spinner,
            &mut renderer,
            &ctrl_c_state,
            &ctrl_c_notify,
            StreamSpinnerOptions {
                suppress_output_signal: Some(suppress_output_signal),
                ..StreamSpinnerOptions::default()
            },
            None,
        )
        .await
        .expect("Copilot stream should return its final response");

        assert_eq!(response.content.as_deref(), Some("final response"));
        assert!(!rendered, "the response must remain suppressed while verification is pending");

        let rendered_text = std::iter::from_fn(|| command_rx.try_recv().ok())
            .filter_map(|command| match command {
                InlineCommand::AppendLine { segments, .. } => {
                    Some(segments.into_iter().map(|segment| segment.text).collect::<String>())
                }
                InlineCommand::Inline { segment, .. } => Some(segment.text),
                InlineCommand::ReplaceLast { lines, .. } => Some(
                    lines
                        .into_iter()
                        .flat_map(|line| line.into_iter().map(|segment| segment.text))
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect::<String>();
        assert!(rendered_text.contains("visible reasoning"));
        assert!(!rendered_text.contains("hidden pending reasoning"));
        assert!(!rendered_text.contains("final response"));
    }
}
