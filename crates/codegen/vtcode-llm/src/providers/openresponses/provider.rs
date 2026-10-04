use crate::error_display;
use crate::provider::{
    FinishReason, LLMError, LLMNormalizedStream, LLMProvider, LLMRequest, LLMResponse, LLMStream, LLMStreamEvent,
    Message, ResponsesCompactionOptions, ToolCall,
};
use crate::providers::common::{
    append_normalized_reasoning_detail_items, chat_completions_url, serialize_message_content_openai,
};
use crate::providers::shared::{
    ResponsesNormalizedStreamOptions, Utf8StreamDecoder, collect_tool_references_from_tool_search_output,
    create_responses_normalized_stream, function_output_value_from_message_content, parse_compacted_output_messages,
};
use anyhow::Result;
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::{Value, json};
use vtcode_config::TimeoutsConfig;
use vtcode_config::constants::{env_vars, models, urls};
use vtcode_config::core::{AnthropicConfig, ModelConfig, PromptCachingConfig};

use super::super::common::{override_base_url, resolve_model};
use super::super::error_handling::{format_network_error, format_parse_error};

pub struct OpenResponsesProvider {
    http_client: HttpClient,
    base_url: String,
    model: String,
    api_key: String,
    model_behavior: Option<ModelConfig>,
}

/// Borrowed fields used by the native Responses SSE hot path.
///
/// Responses events carry a discriminator and a small set of fields consumed
/// by this provider. Deserializing into `Value` first allocates the complete
/// event tree, including fields that are ignored by the streaming loop.
#[derive(Debug, Deserialize)]
struct NativeStreamEventWire<'a> {
    #[serde(rename = "type")]
    event_type: Option<&'a str>,
    #[serde(borrow)]
    delta: Option<&'a str>,
    #[serde(borrow)]
    item_id: Option<&'a str>,
    item: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionStreamEventWire<'a> {
    #[serde(borrow)]
    choices: Option<Vec<ChatCompletionChoiceWire<'a>>>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChoiceWire<'a> {
    #[serde(borrow)]
    delta: Option<ChatCompletionDeltaWire<'a>>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionDeltaWire<'a> {
    #[serde(borrow)]
    content: Option<&'a str>,
    tool_calls: Option<Vec<Value>>,
}

impl OpenResponsesProvider {
    fn parse_native_response_payload(json: Value, model: String) -> Result<LLMResponse, LLMError> {
        let output = json
            .get("output")
            .and_then(|o| o.as_array())
            .ok_or_else(|| LLMError::Provider {
                message: "Invalid response from OpenResponses: missing output".to_string(),
                metadata: None,
            })?;

        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut reasoning = None;
        let mut tool_references = Vec::new();
        let mut replay_items = Vec::new();

        for item_val in output {
            let item_type = item_val.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match item_type {
                "message" => {
                    if let Some(content_parts) = item_val.get("content").and_then(|c| c.as_array()) {
                        for part in content_parts {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                content.push_str(text);
                            }
                        }
                    }
                }
                "reasoning" => {
                    replay_items.push(item_val.clone());
                    if let Some(text) = item_val.get("content").and_then(|t| t.as_str()) {
                        reasoning = Some(text.to_string());
                    }
                }
                "function_call" => {
                    let id = item_val.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = item_val.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let arguments = item_val
                        .get("arguments")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    let namespace = item_val.get("namespace").and_then(|v| v.as_str()).map(ToOwned::to_owned);
                    tool_calls.push(ToolCall::function_with_namespace(id, namespace, name, arguments));
                }
                "tool_search_output" => {
                    collect_tool_references_from_tool_search_output(item_val, &mut tool_references);
                }
                _ if !item_type.is_empty() => replay_items.push(item_val.clone()),
                _ => {}
            }
        }

        let mut reasoning_details = if replay_items.is_empty() {
            None
        } else {
            Some(replay_items.into_iter().map(|item| item.to_string()).collect())
        };
        let (final_reasoning, final_content) = if reasoning.is_none() && !content.is_empty() {
            let (reasoning_parts, cleaned_content) = crate::utils::extract_reasoning_content(&content);
            if reasoning_parts.is_empty() {
                (None, Some(content))
            } else {
                crate::providers::common::preserve_interleaved_content_in_reasoning_details(
                    &mut reasoning_details,
                    &content,
                );
                (Some(reasoning_parts.join("\n\n")), cleaned_content.or(Some(content)))
            }
        } else {
            (reasoning, Some(content))
        };

        let finish_reason = match json.get("status").and_then(|s| s.as_str()) {
            Some("completed") => FinishReason::Stop,
            Some("incomplete") => FinishReason::Length,
            _ => FinishReason::Stop,
        };

        Ok(LLMResponse {
            content: final_content.filter(|c| !c.is_empty()),
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            model,
            usage: None,
            finish_reason,
            reasoning: final_reasoning,
            reasoning_details,
            tool_references,
            request_id: json.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()),
            organization_id: None,
            compaction: None,
        })
    }

    fn output_item_to_value(item: crate::open_responses::OutputItem) -> Result<Value, LLMError> {
        serde_json::to_value(item).map_err(|e| LLMError::Provider {
            message: format!("Failed to serialize Open Responses input item: {e}"),
            metadata: None,
        })
    }

    pub fn new(api_key: String) -> Self {
        Self::with_model(api_key, models::openresponses::DEFAULT_MODEL.to_string())
    }

    pub fn with_model(api_key: String, model: String) -> Self {
        Self::with_model_internal(model, None, api_key, TimeoutsConfig::default(), None)
    }

    fn new_with_client(
        api_key: String,
        model: String,
        http_client: reqwest::Client,
        base_url: String,
        _timeouts: TimeoutsConfig,
    ) -> Self {
        Self {
            http_client,
            base_url,
            model,
            api_key,
            model_behavior: None,
        }
    }

    pub fn from_config(
        api_key: Option<String>,
        model: Option<String>,
        base_url: Option<String>,
        _prompt_cache: Option<PromptCachingConfig>,
        timeouts: Option<TimeoutsConfig>,
        _anthropic: Option<AnthropicConfig>,
        model_behavior: Option<ModelConfig>,
    ) -> Self {
        let api_key_value = api_key.unwrap_or_default();
        let resolved_model = resolve_model(model, models::openresponses::DEFAULT_MODEL);
        Self::with_model_internal(resolved_model, base_url, api_key_value, timeouts.unwrap_or_default(), model_behavior)
    }

    fn with_model_internal(
        model: String,
        base_url: Option<String>,
        api_key: String,
        timeouts: TimeoutsConfig,
        model_behavior: Option<ModelConfig>,
    ) -> Self {
        use crate::http_client::HttpClientFactory;

        Self {
            http_client: HttpClientFactory::for_llm(&timeouts),
            base_url: override_base_url(urls::OPENRESPONSES_API_BASE, base_url, Some(env_vars::OPENRESPONSES_BASE_URL)),
            model,
            api_key,
            model_behavior,
        }
    }

    fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    fn responses_compact_url(&self) -> String {
        format!("{}/responses/compact", self.base_url.trim_end_matches('/'))
    }

    /// Client for driving another endpoint's OpenAI-compatible
    /// `/responses/compact` surface (e.g. Vercel AI Gateway OpenAI routes, xAI
    /// Grok models) with this provider's transport and response parsing.
    /// Capability gating stays with the caller: only construct this for routes
    /// whose upstream documents the endpoint.
    pub(crate) fn compact_endpoint_client(configured_model: &str, base_url: &str, api_key: &str, model: &str) -> Self {
        let resolved = if model.trim().is_empty() {
            configured_model.to_string()
        } else {
            model.to_string()
        };
        Self::from_config(Some(api_key.to_string()), Some(resolved), Some(base_url.to_string()), None, None, None, None)
    }

    fn supports_compaction_endpoint(&self) -> bool {
        self.base_url.contains("api.openai.com") || self.base_url.contains("api.openresponses.com")
    }

    pub(crate) async fn compact_history_request(
        &self,
        model: &str,
        history: &[Message],
    ) -> Result<Vec<Message>, LLMError> {
        self.compact_history_request_with_options(model, history, &ResponsesCompactionOptions::default())
            .await
    }

    pub(crate) async fn compact_history_request_with_options(
        &self,
        model: &str,
        history: &[Message],
        options: &ResponsesCompactionOptions,
    ) -> Result<Vec<Message>, LLMError> {
        let resolved_model = if model.trim().is_empty() {
            self.model.clone()
        } else {
            model.trim().to_string()
        };
        let request = LLMRequest {
            model: resolved_model.clone(),
            messages: std::sync::Arc::new(history.to_vec()),
            ..Default::default()
        };
        let native_payload = self.build_native_payload(&request, false)?;
        let input = native_payload.get("input").cloned().unwrap_or_else(|| json!([]));
        let mut compact_payload = json!({
            "model": resolved_model,
            "input": input,
        });
        if let Some(map) = compact_payload.as_object_mut() {
            if let Some(instructions) = options.instructions.as_deref().map(str::trim).filter(|value| !value.is_empty())
            {
                map.insert("instructions".to_string(), json!(instructions));
            }
            if let Some(service_tier) = options.service_tier.as_deref().map(str::trim).filter(|value| !value.is_empty())
            {
                map.insert("service_tier".to_string(), json!(service_tier));
            }
            if let Some(prompt_cache_key) = options
                .prompt_cache_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                map.insert("prompt_cache_key".to_string(), json!(prompt_cache_key));
            }
        }

        let response = self
            .http_client
            .post(self.responses_compact_url())
            .bearer_auth(&self.api_key)
            .json(&compact_payload)
            .send()
            .await
            .map_err(|e| format_network_error("OpenResponses", &e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = crate::providers::common::read_provider_error_body(response).await;
            let formatted_error = error_display::format_llm_error(
                "OpenResponses",
                &format!("Compaction endpoint error (HTTP {status}): {body}"),
            );
            return Err(LLMError::Provider { message: formatted_error, metadata: None });
        }

        let json: Value = response.json().await.map_err(|e| format_parse_error("OpenResponses", &e))?;
        let output = json
            .get("output")
            .and_then(|value| value.as_array())
            .ok_or_else(|| LLMError::Provider {
                message: "Invalid response from OpenResponses compact endpoint: missing output array".to_string(),
                metadata: None,
            })?;

        let compacted = parse_compacted_output_messages(output);
        if compacted.is_empty() {
            return Err(LLMError::Provider {
                message: "Compaction response contained no reusable messages".to_string(),
                metadata: None,
            });
        }

        Ok(compacted)
    }

    fn build_native_payload(&self, request: &LLMRequest, stream: bool) -> Result<Value, LLMError> {
        use crate::open_responses::{
            ContentPart, ImageDetail, InputFileContent, InputImageContent, MessageRole, OutputItem, Request,
        };

        let mut input: Vec<Value> = Vec::new();

        if let Some(system) = &request.system_prompt {
            input.push(Self::output_item_to_value(OutputItem::completed_message(
                "msg_system",
                MessageRole::System,
                vec![ContentPart::input_text(system.as_ref())],
            ))?);
        }

        for (i, message) in request.messages.iter().enumerate() {
            if let Some(reasoning_details) = &message.reasoning_details {
                append_normalized_reasoning_detail_items(&mut input, reasoning_details);
            }

            let role = match message.role.as_generic_str() {
                "user" => Some(MessageRole::User),
                "assistant" => Some(MessageRole::Assistant),
                "system" => Some(MessageRole::System),
                // Tool responses are represented by function_call_output items below.
                "tool" => None,
                _ => Some(MessageRole::User),
            };

            if let Some(role) = role {
                let id = format!("msg_{i}");
                let mut content = Vec::new();
                match &message.content {
                    crate::provider::MessageContent::Text(text) => {
                        if !text.trim().is_empty() {
                            content.push(ContentPart::input_text(text.as_str()));
                        }
                    }
                    crate::provider::MessageContent::Parts(parts) => {
                        for part in parts {
                            match part {
                                crate::provider::ContentPart::Text { text } => {
                                    if !text.trim().is_empty() {
                                        content.push(ContentPart::input_text(text.as_str()));
                                    }
                                }
                                crate::provider::ContentPart::Image { data, mime_type, .. } => {
                                    // Providers accept only JPEG/PNG/GIF/WebP. Anything else
                                    // (notably SVG auto-attached from quoted paths in diffs)
                                    // fails the whole request with 400, so drop it here
                                    // instead of letting one bad part poison the turn.
                                    if !vtcode_commons::image::is_supported_image_mime_type(mime_type) {
                                        tracing::warn!(
                                            mime_type = %mime_type,
                                            "dropping unsupported image MIME type for Responses API"
                                        );
                                        continue;
                                    }
                                    content.push(ContentPart::InputImage(InputImageContent {
                                        image_url: format!("data:{mime_type};base64,{data}"),
                                        detail: Some(ImageDetail::Auto),
                                    }));
                                }
                                crate::provider::ContentPart::File {
                                    filename, file_id, file_data, file_url, ..
                                } => {
                                    content.push(ContentPart::InputFile(InputFileContent {
                                        filename: filename.clone(),
                                        file_id: file_id.clone(),
                                        file_data: file_data.clone(),
                                        file_url: file_url.clone(),
                                    }));
                                }
                            }
                        }
                    }
                }
                if content.is_empty() {
                    let content_text = message.content.as_text();
                    if !content_text.trim().is_empty() {
                        content.push(ContentPart::input_text(content_text.to_string()));
                    }
                }
                if !content.is_empty() {
                    input.push(Self::output_item_to_value(OutputItem::completed_message(id, role, content))?);
                }
            }

            // Handle tool calls and outputs if present in message history
            if let Some(tool_calls) = &message.tool_calls {
                for (j, tc) in tool_calls.iter().enumerate() {
                    if let Some(f) = &tc.function {
                        input.push(Self::output_item_to_value(OutputItem::function_call(
                            format!("fc_{i}_{j}"),
                            &f.name,
                            tc.parsed_arguments().unwrap_or(Value::Null),
                        ))?);
                    }
                }
            }

            if let Some(tool_call_id) = &message.tool_call_id {
                // If this message is a tool output, add it as FunctionCallOutput
                input.push(json!({
                    "type": "function_call_output",
                    "id": format!("fco_{i}"),
                    "status": "completed",
                    "call_id": tool_call_id,
                    "output": function_output_value_from_message_content(&message.content),
                }));
            }
        }

        let mut req = Request::new(&request.model, Vec::new());
        req.stream = stream;
        req.temperature = request.temperature.map(crate::providers::common::sampling_param_f64);
        req.max_output_tokens = request.max_tokens.map(|t| t as u64);
        req.previous_response_id = request
            .previous_response_id
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        req.store = request.response_store;
        req.include = request.responses_include.as_ref().and_then(|fields| {
            let values: Vec<String> = fields
                .iter()
                .map(|field| field.trim())
                .filter(|field| !field.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            if values.is_empty() { None } else { Some(values) }
        });

        if let Some(tools) = &request.tools {
            req.tools = Some(tools.as_ref().clone());
        }

        let mut payload = serde_json::to_value(req).map_err(|e| LLMError::Provider {
            message: format!("Failed to serialize Open Responses request: {e}"),
            metadata: None,
        })?;
        if let Some(map) = payload.as_object_mut() {
            map.insert("input".to_string(), Value::Array(input));
        }

        if let Some(context_management) = &request.context_management
            && let Some(map) = payload.as_object_mut()
        {
            map.insert("context_management".to_string(), context_management.clone());
        }

        Ok(payload)
    }

    fn build_payload(&self, request: &LLMRequest, stream: bool) -> Result<Value, LLMError> {
        let mut messages = Vec::new();

        if let Some(system) = &request.system_prompt {
            messages.push(json!({
                "role": "system",
                "content": system
            }));
        }

        for message in request.messages.iter() {
            let role = message.role.as_generic_str();
            let mut message_obj = json!({
                "role": role,
                "content": serialize_message_content_openai(&message.content)
            });

            if let Some(tool_calls) = &message.tool_calls {
                let tool_calls_json: Vec<Value> = tool_calls
                    .iter()
                    .filter_map(|tc| {
                        tc.function.as_ref().map(|f| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": f.name,
                                    "arguments": f.arguments
                                }
                            })
                        })
                    })
                    .collect();
                message_obj["tool_calls"] = json!(tool_calls_json);
            }

            if let Some(tool_call_id) = &message.tool_call_id {
                message_obj["tool_call_id"] = json!(tool_call_id);
            }

            messages.push(message_obj);
        }

        let mut payload = json!({
            "model": request.model,
            "messages": messages,
            "stream": stream
        });

        if let Some(max_tokens) = request.max_tokens {
            payload["max_tokens"] = json!(max_tokens);
        }

        if let Some(temp) = request.temperature {
            payload["temperature"] = json!(crate::providers::common::sampling_param_f64(temp));
        }

        if let Some(tools) = &request.tools {
            let tools_json: Vec<Value> = tools
                .iter()
                .filter_map(|t| {
                    t.function.as_ref().map(|f| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": f.name,
                                "description": f.description,
                                "parameters": f.parameters
                            }
                        })
                    })
                })
                .collect();
            payload["tools"] = json!(tools_json);
        }

        Ok(payload)
    }

    async fn generate_fallback(&self, request: LLMRequest) -> Result<LLMResponse, LLMError> {
        let model = request.model.clone();
        let payload = self.build_payload(&request, false)?;
        let url = chat_completions_url(&self.base_url);

        let response = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format_network_error("OpenResponses", &e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = crate::providers::common::read_provider_error_body(response).await;
            let formatted_error = error_display::format_llm_error("OpenResponses", &format!("HTTP {status}: {body}"));
            return Err(LLMError::Provider { message: formatted_error, metadata: None });
        }

        let json: Value = response.json().await.map_err(|e| format_parse_error("OpenResponses", &e))?;

        let choice = json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .ok_or_else(|| LLMError::Provider {
                message: "Invalid response from OpenResponses: missing choices".to_string(),
                metadata: None,
            })?;

        let message = choice.get("message").ok_or_else(|| LLMError::Provider {
            message: "Invalid response from OpenResponses: missing message".to_string(),
            metadata: None,
        })?;

        let content = message.get("content").and_then(|c| c.as_str()).map(|s| s.to_string());

        let tool_calls = message
            .get("tool_calls")
            .and_then(|tc| tc.as_array())
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| {
                        let id = call.get("id").and_then(|v| v.as_str())?;
                        let function = call.get("function")?;
                        let namespace = call
                            .get("namespace")
                            .and_then(|v| v.as_str())
                            .or_else(|| function.get("namespace").and_then(|v| v.as_str()))
                            .map(ToOwned::to_owned);
                        let name = function.get("name").and_then(|v| v.as_str())?;
                        let arguments = function.get("arguments").and_then(|v| v.as_str())?;
                        Some(ToolCall::function_with_namespace(
                            id.to_string(),
                            namespace,
                            name.to_string(),
                            arguments.to_string(),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|calls| !calls.is_empty());

        let finish_reason = choice
            .get("finish_reason")
            .and_then(|fr| fr.as_str())
            .map(|fr| match fr {
                "stop" => FinishReason::Stop,
                "length" => FinishReason::Length,
                "tool_calls" => FinishReason::ToolCalls,
                other => FinishReason::Error(other.to_string()),
            })
            .unwrap_or(FinishReason::Stop);

        Ok(LLMResponse {
            content,
            tool_calls,
            model,
            usage: None,
            finish_reason,
            reasoning: None,
            reasoning_details: None,
            tool_references: Vec::new(),
            request_id: json.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()),
            organization_id: None,
            compaction: None,
        })
    }

    async fn stream_fallback(&self, request: LLMRequest) -> Result<LLMStream, LLMError> {
        let model = request.model.clone();
        let payload = self.build_payload(&request, true)?;
        let url = chat_completions_url(&self.base_url);

        let response = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format_network_error("OpenResponses", &e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = crate::providers::common::read_provider_error_body(response).await;
            let formatted_error = error_display::format_llm_error("OpenResponses", &format!("HTTP {status}: {body}"));
            return Err(LLMError::Provider { message: formatted_error, metadata: None });
        }

        let stream = try_stream! {
            let mut body_stream = response.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut offset = 0usize;
            let mut decoder = Utf8StreamDecoder::new();
            let mut aggregator = crate::providers::shared::StreamAggregator::new(model);

            while let Some(chunk_result) = body_stream.next().await {
                let chunk = chunk_result.map_err(|e| format_network_error("OpenResponses", &e))?;
                decoder.push_bytes(&chunk, &mut buf);

                while let Some(event) =
                    crate::providers::shared::next_sse_event(&buf, &mut offset).expect("valid utf-8 stream data")
                {

                    if let Some(data_payload) = crate::providers::shared::extract_data_payload(event) {
                        let trimmed = data_payload.trim();
                        if trimmed.is_empty() || trimmed == "[DONE]" {
                            continue;
                        }

                        if let Ok(payload) = serde_json::from_str::<ChatCompletionStreamEventWire<'_>>(trimmed)
                            && let Some(delta) = payload.choices.and_then(|choices| choices.into_iter().next()).and_then(|choice| choice.delta)
                        {
                            if let Some(content) = delta.content {
                                for ev in aggregator.handle_content(content) {
                                    yield ev;
                                }
                            }

                            if let Some(tool_calls) = delta.tool_calls.as_deref() {
                                aggregator.handle_tool_calls(tool_calls);
                            }
                        }
                    }
                }

                // Keep `buf` bounded to the unprocessed tail rather than
                // growing for the entire stream.
                crate::providers::shared::drain_consumed_sse(&mut buf, &mut offset);
            }

            yield LLMStreamEvent::Completed { response: Box::new(aggregator.finalize()) };
        };

        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl LLMProvider for OpenResponsesProvider {
    fn name(&self) -> &str {
        "openresponses"
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_non_streaming(&self, _model: &str) -> bool {
        // Pinned so the stream-timeout fallback cannot silently regress.
        true
    }

    fn supports_reasoning(&self, _model: &str) -> bool {
        self.model_behavior
            .as_ref()
            .and_then(|b| b.model_supports_reasoning)
            .unwrap_or(true) // Open Responses usually implies reasoning support
    }

    fn supports_reasoning_effort(&self, _model: &str) -> bool {
        self.model_behavior
            .as_ref()
            .and_then(|b| b.model_supports_reasoning_effort)
            .unwrap_or(true)
    }

    fn supports_responses_compaction(&self, _model: &str) -> bool {
        self.supports_compaction_endpoint()
    }

    // OpenResponses exposes the same standalone `/responses/compact` endpoint as
    // the native OpenAI provider, so it opts into the NativeStandalone dispatch
    // (`compact_history_with_options`). Without this, the unified dispatch would
    // route it to NativeInline (Anthropic `compact_20260112` via `generate`),
    // which OpenResponses cannot serve, silently downgrading auto/recovery
    // compaction to the local summarization fallback.
    fn supports_manual_openai_compaction(&self, _model: &str) -> bool {
        self.supports_compaction_endpoint()
    }

    async fn compact_history(&self, model: &str, history: &[Message]) -> Result<Vec<Message>, LLMError> {
        if !self.supports_compaction_endpoint() {
            return Err(LLMError::Provider {
                message: "OpenResponses compact endpoint is not supported for this configured base URL".to_string(),
                metadata: None,
            });
        }

        self.compact_history_request(model, history).await
    }

    // The compact endpoint accepts the common instruction and routing fields;
    // output-shaping options are intentionally omitted because this endpoint
    // does not expose them in its request schema.
    async fn compact_history_with_options(
        &self,
        model: &str,
        history: &[Message],
        options: &ResponsesCompactionOptions,
    ) -> Result<Vec<Message>, LLMError> {
        if !self.supports_compaction_endpoint() {
            return Err(LLMError::Provider {
                message: "OpenResponses compact endpoint is not supported for this configured base URL".to_string(),
                metadata: None,
            });
        }

        self.compact_history_request_with_options(model, history, options).await
    }

    fn supported_models(&self) -> Vec<String> {
        use vtcode_config::constants::models::openresponses::SUPPORTED_MODELS;
        SUPPORTED_MODELS.iter().map(|s| s.to_string()).collect()
    }

    fn validate_request(&self, request: &LLMRequest) -> Result<(), LLMError> {
        if request.model.is_empty() {
            return Err(LLMError::Provider {
                message: "Model is required for OpenResponses provider".to_string(),
                metadata: None,
            });
        }

        let supported = self.supported_models();
        if !supported.contains(&request.model) {
            return Err(LLMError::Provider {
                message: format!(
                    "Model '{}' is not supported by OpenResponses provider. Supported models: {}",
                    request.model,
                    supported.join(", ")
                ),
                metadata: None,
            });
        }

        Ok(())
    }

    async fn generate(&self, mut request: LLMRequest) -> Result<LLMResponse, LLMError> {
        if request.model.is_empty() {
            request.model = self.model.clone();
        }
        let model = request.model.clone();

        // Try native Open Responses endpoint first
        let payload = self.build_native_payload(&request, false)?;
        let url = self.responses_url();

        let response = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format_network_error("OpenResponses", &e))?;

        // If native endpoint fails with 404, fallback to chat/completions
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return self.generate_fallback(request).await;
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = crate::providers::common::read_provider_error_body(response).await;
            let formatted_error = error_display::format_llm_error("OpenResponses", &format!("HTTP {status}: {body}"));
            return Err(LLMError::Provider { message: formatted_error, metadata: None });
        }

        let json: Value = response.json().await.map_err(|e| format_parse_error("OpenResponses", &e))?;

        Self::parse_native_response_payload(json, model)
    }

    async fn stream(&self, mut request: LLMRequest) -> Result<LLMStream, LLMError> {
        if request.model.is_empty() {
            request.model = self.model.clone();
        }
        let model = request.model.clone();

        let payload = self.build_native_payload(&request, true)?;
        let url = self.responses_url();

        let response = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format_network_error("OpenResponses", &e))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return self.stream_fallback(request).await;
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = crate::providers::common::read_provider_error_body(response).await;
            let formatted_error = error_display::format_llm_error("OpenResponses", &format!("HTTP {status}: {body}"));
            return Err(LLMError::Provider { message: formatted_error, metadata: None });
        }

        let stream = try_stream! {
            let mut body_stream = response.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut offset = 0usize;
            let mut decoder = Utf8StreamDecoder::new();
            let mut aggregator = crate::providers::shared::StreamAggregator::new(model);

            while let Some(chunk_result) = body_stream.next().await {
                let chunk = chunk_result.map_err(|e| format_network_error("OpenResponses", &e))?;
                decoder.push_bytes(&chunk, &mut buf);

                while let Some(event) =
                    crate::providers::shared::next_sse_event(&buf, &mut offset).expect("valid utf-8 stream data")
                {

                    if let Some(data_payload) = crate::providers::shared::extract_data_payload(event) {
                        let trimmed = data_payload.trim();
                        if trimmed.is_empty() || trimmed == "[DONE]" {
                            continue;
                        }

                        if let Ok(event) = serde_json::from_str::<NativeStreamEventWire<'_>>(trimmed) {
                            let event_type = event.event_type.unwrap_or("");

                            match event_type {
                                "response.output_text.delta" => {
                                    if let Some(delta) = event.delta {
                                        // Use aggregator's sanitizer to extract reasoning tags from content
                                        for ev in aggregator.handle_content(delta) {
                                            yield ev;
                                        }
                                    }
                                }
                                "response.function_call_arguments.delta" => {
                                    if let Some(delta) = event.delta {
                                        let tc_json = json!([{
                                            "index": 0,
                                            "id": event.item_id,
                                            "function": { "arguments": delta }
                                        }]);
                                        if let Some(tool_calls) = tc_json.as_array() {
                                            aggregator.handle_tool_calls(tool_calls);
                                        }
                                    }
                                }
                                "response.reasoning.delta" => {
                                    // Legacy/simple reasoning event
                                    if let Some(delta) = event.delta {
                                        yield LLMStreamEvent::Reasoning { delta: delta.to_string() };
                                    }
                                }
                                "response.reasoning_content.delta" => {
                                    // Raw reasoning traces (preferred)
                                    if let Some(delta) = event.delta {
                                        yield LLMStreamEvent::Reasoning { delta: delta.to_string() };
                                    }
                                }
                                "response.reasoning_summary_text.delta" => {
                                    // Summary reasoning (fallback when raw not available)
                                    if let Some(delta) = event.delta {
                                        yield LLMStreamEvent::Reasoning { delta: delta.to_string() };
                                    }
                                }
                                // The added event may contain an incomplete
                                // opaque item. Only the done snapshot is safe
                                // to replay on the next request.
                                "response.output_item.done" => {
                                    if let Some(item) = event.item.as_ref()
                                        && item.get("type").and_then(Value::as_str) == Some("compaction")
                                    {
                                        aggregator.append_reasoning_detail(item);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                // Keep `buf` bounded to the unprocessed tail rather than
                // growing for the entire stream.
                crate::providers::shared::drain_consumed_sse(&mut buf, &mut offset);
            }

            yield LLMStreamEvent::Completed { response: Box::new(aggregator.finalize()) };
        };

        Ok(Box::pin(stream))
    }

    async fn stream_normalized(&self, mut request: LLMRequest) -> Result<LLMNormalizedStream, LLMError> {
        if request.model.is_empty() {
            request.model = self.model.clone();
        }
        let model = request.model.clone();

        let payload = self.build_native_payload(&request, true)?;
        let url = self.responses_url();

        let response = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format_network_error("OpenResponses", &e))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            let mut legacy_stream = self.stream_fallback(request).await?;
            let stream = try_stream! {
                while let Some(event) = legacy_stream.next().await {
                    for normalized in event?.into_normalized() {
                        yield normalized;
                    }
                }
            };
            return Ok(Box::pin(stream));
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = crate::providers::common::read_provider_error_body(response).await;
            let formatted_error = error_display::format_llm_error("OpenResponses", &format!("HTTP {status}: {body}"));
            return Err(LLMError::Provider { message: formatted_error, metadata: None });
        }

        let emit_reasoning = self.supports_reasoning(&model);
        Ok(create_responses_normalized_stream(
            response,
            ResponsesNormalizedStreamOptions {
                provider_name: "OpenResponses",
                model: model.clone(),
                emit_reasoning,
                include_cached_prompt_metrics: false,
            },
            move |value| Self::parse_native_response_payload(value, model.clone()),
        ))
    }
}

#[cfg(test)]
mod tests;
