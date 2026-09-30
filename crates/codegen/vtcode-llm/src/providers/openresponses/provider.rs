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

                while let Some((split_idx, delimiter_len)) = crate::providers::shared::find_sse_boundary_bytes(&buf, offset) {
                    let event = std::str::from_utf8(&buf[offset..split_idx]).expect("valid utf-8 stream data");
                    offset = split_idx + delimiter_len;

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

                // Drain the consumed prefix so `buf` stays bounded to the
                // unprocessed tail rather than growing for the entire stream.
                if offset > 0 {
                    buf.drain(..offset);
                    offset = 0;
                }
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

                while let Some((split_idx, delimiter_len)) = crate::providers::shared::find_sse_boundary_bytes(&buf, offset) {
                    let event = std::str::from_utf8(&buf[offset..split_idx]).expect("valid utf-8 stream data");
                    offset = split_idx + delimiter_len;

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

                // Drain the consumed prefix so `buf` stays bounded to the
                // unprocessed tail rather than growing for the entire stream.
                if offset > 0 {
                    buf.drain(..offset);
                    offset = 0;
                }
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
mod tests {
    use super::*;
    use crate::provider::NormalizedStreamEvent;
    use futures::StreamExt;
    use wiremock::matchers::{body_json, body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        if let Some(message) = payload.downcast_ref::<String>() {
            return message.clone();
        }
        if let Some(message) = payload.downcast_ref::<&str>() {
            return (*message).to_string();
        }
        "unknown panic".to_string()
    }

    async fn start_mock_server_or_skip() -> Option<MockServer> {
        match tokio::spawn(async { MockServer::start().await }).await {
            Ok(server) => Some(server),
            Err(err) if err.is_panic() => {
                let message = panic_message(err.into_panic());
                if message.contains("Operation not permitted") || message.contains("PermissionDenied") {
                    return None;
                }
                panic!("mock server should start: {message}");
            }
            Err(err) => panic!("mock server task should complete: {err}"),
        }
    }

    fn test_provider(base_url: &str) -> OpenResponsesProvider {
        let http_client = reqwest::Client::builder().no_proxy().build().expect("test client should build");
        OpenResponsesProvider::new_with_client(
            String::new(),
            "gpt-5".to_string(),
            http_client,
            base_url.to_string(),
            TimeoutsConfig::default(),
        )
    }

    #[test]
    fn native_payload_includes_responses_continuity_fields() {
        let provider = test_provider("https://api.openresponses.com/v1");
        let mut request = LLMRequest {
            model: "gpt-5".to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };
        request.previous_response_id = Some("resp_prev_1".to_string());
        request.response_store = Some(false);
        request.responses_include = Some(vec![
            "reasoning.encrypted_content".to_string(),
            "output_text.annotations".to_string(),
        ]);

        let payload = provider
            .build_native_payload(&request, false)
            .expect("native payload should serialize");

        assert_eq!(payload.get("previous_response_id").and_then(Value::as_str), Some("resp_prev_1"));
        assert_eq!(payload.get("store").and_then(Value::as_bool), Some(false));
        let include = payload.get("include").and_then(Value::as_array).expect("include must exist");
        assert_eq!(include.len(), 2);
    }

    #[test]
    fn native_payload_serializes_compact_temperature() {
        let provider = test_provider("https://api.openresponses.com/v1");
        let request = LLMRequest {
            model: "gpt-5".to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            temperature: Some(0.7),
            ..Default::default()
        };

        let payload = provider
            .build_native_payload(&request, false)
            .expect("native payload should serialize");

        assert_eq!(payload.get("temperature").and_then(Value::as_f64), Some(0.7));
        assert_eq!(
            payload.get("temperature").expect("temperature present").to_string(),
            "0.7",
            "wire form must be compact, not the f32->f64 widening tail"
        );
    }

    #[test]
    fn native_payload_includes_context_management() {
        let provider = test_provider("https://api.openresponses.com/v1");
        let mut request = LLMRequest {
            model: "gpt-5".to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };
        request.context_management = Some(serde_json::json!([{
            "type": "compaction",
            "compact_threshold": 200000
        }]));

        let payload = provider
            .build_native_payload(&request, false)
            .expect("native payload should serialize");
        let management = payload
            .get("context_management")
            .and_then(Value::as_array)
            .expect("context management should exist");
        assert_eq!(management.len(), 1);
    }

    #[test]
    fn openresponses_provider_reports_compaction_support() {
        let provider = test_provider("https://api.openresponses.com/v1");
        assert!(provider.supports_responses_compaction("gpt-5"));
    }

    #[test]
    fn openresponses_provider_disables_compaction_for_unknown_endpoint() {
        let provider = test_provider("https://api.example.com/v1");
        assert!(!provider.supports_responses_compaction("gpt-5"));
    }

    #[tokio::test]
    async fn compact_history_request_matches_openresponses_compact_schema() {
        let Some(server) = start_mock_server_or_skip().await else {
            return;
        };

        Mock::given(method("POST"))
            .and(path("/v1/responses/compact"))
            .and(body_json(serde_json::json!({
                "model": "gpt-5",
                "input": [{
                    "type": "message",
                    "id": "msg_0",
                    "status": "completed",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "compact this"}]
                }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmp_streaming",
                "object": "response.compaction",
                "output": [{
                    "type": "compaction",
                    "id": "cmp_1",
                    "encrypted_content": "opaque_state"
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = test_provider(&format!("{}/v1", server.uri()));
        let compacted = provider
            .compact_history_request("gpt-5", &[Message::user("compact this".to_string())])
            .await
            .expect("compaction request should succeed");

        let preserved_type = compacted[0]
            .reasoning_details
            .as_ref()
            .and_then(|items| items.first())
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str);
        assert_eq!(preserved_type, Some("compaction"));
    }

    #[tokio::test]
    async fn compact_history_request_forwards_supported_options() {
        let Some(server) = start_mock_server_or_skip().await else {
            return;
        };

        Mock::given(method("POST"))
            .and(path("/v1/responses/compact"))
            .and(body_partial_json(json!({
                "instructions": "keep decisions",
                "service_tier": "priority",
                "prompt_cache_key": "session-1"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cmp_options",
                "object": "response.compaction",
                "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "compacted"}]}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = test_provider(&format!("{}/v1", server.uri()));
        provider
            .compact_history_request_with_options(
                "gpt-5",
                &[Message::user("compact this".to_string())],
                &ResponsesCompactionOptions {
                    instructions: Some("  keep decisions  ".to_string()),
                    service_tier: Some(" priority ".to_string()),
                    prompt_cache_key: Some(" session-1 ".to_string()),
                    ..ResponsesCompactionOptions::default()
                },
            )
            .await
            .expect("compaction request should succeed");
    }

    #[test]
    fn native_response_preserves_opaque_compaction_item_for_replay() {
        let provider = test_provider("https://api.openresponses.com/v1");
        let compaction_item = json!({
            "type": "compaction",
            "id": "cmp_1",
            "encrypted_content": "opaque_state"
        });
        let response = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                compaction_item.clone(),
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "continued"}]
                }
            ]
        });

        let parsed = OpenResponsesProvider::parse_native_response_payload(response, "gpt-5".to_string())
            .expect("native response should parse");
        let reasoning_details = parsed.reasoning_details.clone().expect("compaction item should be preserved");
        assert_eq!(serde_json::from_str::<Value>(&reasoning_details[0]).unwrap(), compaction_item);
        let reasoning_values = reasoning_details
            .iter()
            .map(|item| serde_json::from_str(item).expect("reasoning detail should be valid JSON"))
            .collect();

        let payload = provider
            .build_native_payload(
                &LLMRequest {
                    model: "gpt-5".to_string(),
                    messages: vec![
                        Message::assistant(parsed.content.unwrap_or_default())
                            .with_reasoning_details(Some(reasoning_values)),
                    ]
                    .into(),
                    ..Default::default()
                },
                false,
            )
            .expect("native payload should replay compaction item");
        let input = payload
            .get("input")
            .and_then(Value::as_array)
            .expect("input should be an array");

        assert_eq!(input[0], compaction_item);
        assert_eq!(input[1].get("type").and_then(Value::as_str), Some("message"));
        assert_eq!(input[1].get("role").and_then(Value::as_str), Some("assistant"));
    }

    #[test]
    fn native_payload_preserves_opaque_reasoning_details_items() {
        let provider = test_provider("https://api.openresponses.com/v1");
        let message = Message::assistant(String::new()).with_reasoning_details(Some(vec![json!({
            "type": "compaction",
            "id": "cmp_1",
            "status": "completed",
            "encrypted_content": "opaque_state"
        })]));
        let request = LLMRequest {
            model: "gpt-5".to_string(),
            messages: vec![message].into(),
            ..Default::default()
        };

        let payload = provider
            .build_native_payload(&request, false)
            .expect("native payload should serialize");
        let input = payload
            .get("input")
            .and_then(Value::as_array)
            .expect("input should be an array");

        assert_eq!(input.len(), 1);
        assert_eq!(input[0].get("type").and_then(Value::as_str), Some("compaction"));
        assert_eq!(input[0].get("encrypted_content").and_then(Value::as_str), Some("opaque_state"));
    }

    #[test]
    fn native_payload_normalizes_stringified_reasoning_details_items() {
        let provider = test_provider("https://api.openresponses.com/v1");
        let message = Message::assistant(String::new()).with_reasoning_details(Some(vec![
            json!(r#"{"type":"compaction","id":"cmp_1","encrypted_content":"opaque_state"}"#),
            json!("not-json"),
        ]));
        let request = LLMRequest {
            model: "gpt-5".to_string(),
            messages: vec![message].into(),
            ..Default::default()
        };

        let payload = provider
            .build_native_payload(&request, false)
            .expect("native payload should serialize");
        let input = payload
            .get("input")
            .and_then(Value::as_array)
            .expect("input should be an array");

        assert_eq!(input.len(), 1);
        assert_eq!(input[0].get("type").and_then(Value::as_str), Some("compaction"));
    }

    #[test]
    fn native_payload_emits_tool_response_only_as_function_call_output() {
        let provider = test_provider("https://api.openresponses.com/v1");
        let request = LLMRequest {
            model: "gpt-5".to_string(),
            messages: vec![
                Message::assistant_with_tools(
                    String::new(),
                    vec![ToolCall::function(
                        "call_1".to_string(),
                        "shell".to_string(),
                        "{\"command\":\"pwd\"}".to_string(),
                    )],
                ),
                Message::tool_response("call_1".to_string(), "/tmp/work".to_string()),
            ]
            .into(),
            ..Default::default()
        };

        let payload = provider
            .build_native_payload(&request, false)
            .expect("native payload should serialize");
        let input = payload
            .get("input")
            .and_then(Value::as_array)
            .expect("input should be an array");

        assert!(input.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some("call_1")
        }));
        assert!(!input.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("user")
                && item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .any(|part| part.get("text").and_then(Value::as_str) == Some("/tmp/work"))
        }));
    }

    #[test]
    fn native_payload_preserves_multimodal_tool_output_items() {
        let provider = test_provider("https://api.openresponses.com/v1");
        let request = LLMRequest {
            model: "gpt-5".to_string(),
            messages: vec![
                Message::assistant_with_tools(
                    String::new(),
                    vec![ToolCall::function(
                        "call_1".to_string(),
                        "view_image".to_string(),
                        "{\"path\":\"./img.png\"}".to_string(),
                    )],
                ),
                Message::tool_response(
                    "call_1".to_string(),
                    r#"[{"type":"input_text","text":"inline image note"},{"type":"input_image","image_url":"data:image/png;base64,abc"}]"#
                        .to_string(),
                ),
            ].into(),
            ..Default::default()
        };

        let payload = provider
            .build_native_payload(&request, false)
            .expect("native payload should serialize");
        let input = payload
            .get("input")
            .and_then(Value::as_array)
            .expect("input should be an array");

        let function_call_output = input
            .iter()
            .find(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some("call_1")
            })
            .expect("function_call_output item should exist");

        let output_items = function_call_output
            .get("output")
            .and_then(Value::as_array)
            .expect("multimodal output should be serialized as an array");
        assert_eq!(output_items.len(), 2);
        assert_eq!(output_items[0]["type"], "input_text");
        assert_eq!(output_items[0]["text"], "inline image note");
        assert_eq!(output_items[1]["type"], "input_image");
        assert_eq!(output_items[1]["image_url"], "data:image/png;base64,abc");
    }

    #[test]
    fn native_payload_drops_unsupported_svg_image_parts() {
        // Regression: an SVG auto-attached from a quoted path in a WebMCP diff
        // must not reach the wire — providers fail the whole request with 400
        // `invalid_value` when any `input_image` carries an unsupported type.
        let provider = test_provider("https://api.openresponses.com/v1");
        let request = LLMRequest {
            model: "gpt-5".to_string(),
            messages: vec![Message::user_with_parts(vec![
                crate::provider::ContentPart::text("see the logo".to_string()),
                crate::provider::ContentPart::image("PHN2Zz48L3N2Zz4=".to_string(), "image/svg+xml".to_string()),
            ])]
            .into(),
            ..Default::default()
        };

        let payload = provider
            .build_native_payload(&request, false)
            .expect("native payload should serialize");
        let serialized = serde_json::to_string(&payload).expect("payload should serialize");
        assert!(!serialized.contains("image/svg+xml"), "SVG image must be dropped from the wire payload");
        assert!(!serialized.contains("input_image"), "no image part should remain");
        assert!(serialized.contains("see the logo"), "the text part must survive");
    }

    #[tokio::test]
    async fn generate_falls_back_to_chat_completions_when_native_endpoint_is_missing() {
        let Some(server) = start_mock_server_or_skip().await else {
            return;
        };
        let provider = test_provider(&server.uri());

        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl_fallback",
                "choices": [{
                    "finish_reason": "stop",
                    "message": {
                        "content": "fallback completion"
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let response = provider
            .generate(LLMRequest {
                model: "gpt-5".to_string(),
                messages: vec![Message::user("hello".to_string())].into(),
                ..Default::default()
            })
            .await
            .expect("fallback generate should succeed");

        assert_eq!(response.content.as_deref(), Some("fallback completion"));
    }

    #[tokio::test]
    async fn stream_falls_back_to_chat_completions_when_native_endpoint_is_missing() {
        let Some(server) = start_mock_server_or_skip().await else {
            return;
        };
        let provider = test_provider(&server.uri());

        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"fallback stream\"}}]}\n\n\
data: [DONE]\n\n",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut stream = provider
            .stream(LLMRequest {
                model: "gpt-5".to_string(),
                messages: vec![Message::user("hello".to_string())].into(),
                ..Default::default()
            })
            .await
            .expect("fallback stream should succeed");

        let mut completed = None;
        while let Some(event) = stream.next().await {
            match event.expect("stream event should parse") {
                LLMStreamEvent::Completed { response } => completed = Some(response),
                LLMStreamEvent::Token { .. }
                | LLMStreamEvent::Reasoning { .. }
                | LLMStreamEvent::ReasoningSignature { .. }
                | LLMStreamEvent::ReasoningStage { .. } => {}
            }
        }

        let response = completed.expect("stream should finish with a completed response");
        assert_eq!(response.content.as_deref(), Some("fallback stream"));
    }

    #[tokio::test]
    async fn native_stream_decodes_only_fields_consumed_by_the_hot_path() {
        let Some(server) = start_mock_server_or_skip().await else {
            return;
        };
        let provider = test_provider(&server.uri());

        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"native stream\",\"ignored\":{\"nested\":true}}\n\n\
data: {\"type\":\"response.reasoning_content.delta\",\"delta\":\"think\"}\n\n\
data: [DONE]\n\n",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut stream = provider
            .stream(LLMRequest {
                model: "gpt-5".to_string(),
                messages: vec![Message::user("hello".to_string())].into(),
                ..Default::default()
            })
            .await
            .expect("native stream should succeed");

        let mut completed = None;
        while let Some(event) = stream.next().await {
            match event.expect("stream event should parse") {
                LLMStreamEvent::Completed { response } => completed = Some(response),
                LLMStreamEvent::Token { .. }
                | LLMStreamEvent::Reasoning { .. }
                | LLMStreamEvent::ReasoningSignature { .. }
                | LLMStreamEvent::ReasoningStage { .. } => {}
            }
        }

        let response = completed.expect("stream should finish with a completed response");
        assert_eq!(response.content.as_deref(), Some("native stream"));
    }

    #[tokio::test]
    async fn native_stream_preserves_opaque_compaction_output_items() {
        let Some(server) = start_mock_server_or_skip().await else {
            return;
        };
        let provider = test_provider(&server.uri());
        let compaction_item = json!({
            "type": "compaction",
            "id": "cmp_1",
            "encrypted_content": "opaque_state"
        });
        let added_event = json!({
            "type": "response.output_item.added",
            "item": {
                "type": "compaction",
                "id": "cmp_1",
                "encrypted_content": "partial_state"
            }
        });
        let done_event = json!({
            "type": "response.output_item.done",
            "item": compaction_item.clone()
        });

        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!("data: {added_event}\n\ndata: {done_event}\n\ndata: [DONE]\n\n")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut stream = provider
            .stream(LLMRequest {
                model: "gpt-5".to_string(),
                messages: vec![Message::user("continue".to_string())].into(),
                ..Default::default()
            })
            .await
            .expect("native stream should succeed");

        let mut completed = None;
        while let Some(event) = stream.next().await {
            if let LLMStreamEvent::Completed { response } = event.expect("stream event should parse") {
                completed = Some(response);
            }
        }

        let response = completed.expect("stream should finish with a completed response");
        let details = response.reasoning_details.expect("compaction item should be preserved");
        assert_eq!(details.len(), 1, "added and done events must not duplicate the item");
        assert_eq!(serde_json::from_str::<Value>(&details[0]).unwrap(), compaction_item);
    }

    #[tokio::test]
    async fn stream_normalized_emits_tool_call_start_and_delta_events() {
        let Some(server) = start_mock_server_or_skip().await else {
            return;
        };
        let provider = test_provider(&server.uri());

        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"type\":\"response.output_item.added\",\"item_id\":\"call_1\",\"output_index\":0,\"sequence_number\":1,\"item\":{\"type\":\"function_call\",\"id\":\"call_1\",\"call_id\":\"call_1\",\"name\":\"search_workspace\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n\
data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"call_1\",\"output_index\":0,\"content_index\":0,\"sequence_number\":2,\"delta\":\"{\\\"pattern\\\":\\\"ph\"}\n\n\
data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"call_1\",\"output_index\":0,\"content_index\":0,\"sequence_number\":3,\"delta\":\"ase\\\"}\"}\n\n\
data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"content_index\":0,\"sequence_number\":4,\"delta\":\"done\"}\n\n\
data: [DONE]\n\n",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut stream = provider
            .stream_normalized(LLMRequest {
                model: "gpt-5".to_string(),
                messages: vec![Message::user("hello".to_string())].into(),
                ..Default::default()
            })
            .await
            .expect("normalized stream should succeed");

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.expect("stream event should parse"));
        }

        assert!(matches!(
            events.as_slice(),
            [
                NormalizedStreamEvent::ToolCallStart { call_id, name },
                NormalizedStreamEvent::ToolCallDelta { call_id: first_delta_id, delta: first_delta },
                NormalizedStreamEvent::ToolCallDelta { call_id: second_delta_id, delta: second_delta },
                NormalizedStreamEvent::TextDelta { delta },
                NormalizedStreamEvent::Done { .. }
            ]
            if call_id == "call_1"
                && name.as_deref() == Some("search_workspace")
                && first_delta_id == "call_1"
                && first_delta == "{\"pattern\":\"ph"
                && second_delta_id == "call_1"
                && second_delta == "ase\"}"
                && delta == "done"
        ));
    }
}
