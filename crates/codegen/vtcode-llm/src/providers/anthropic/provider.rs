//! Main Anthropic Claude provider implementation
//!
//! This is the primary interface for the Anthropic provider, implementing
//! the LLMProvider and LLMClient traits. It delegates to submodules for:
//! - Request building (request_builder)
//! - Response parsing (response_parser)
//! - Stream decoding (stream_decoder)
//! - Capability detection (capabilities)
//! - Validation (validation)
//! - Header management (headers)

use crate::client::LLMClient;
use crate::provider::{LLMError, LLMProvider, LLMRequest, LLMResponse, LLMStream, LLMStreamEvent, ToolDefinition};
use vtcode_config::TimeoutsConfig;
use vtcode_config::constants::{env_vars, models, urls};
use vtcode_config::core::{AnthropicConfig, AnthropicPromptCacheSettings, ModelConfig, PromptCachingConfig};

use super::capabilities;
use super::fallback_retry::{self, RefusalRetryPlan};
use super::headers;
use super::request_builder::{self, RequestBuilderContext};
use super::response_parser;
use super::stream_decoder;
use super::validation;

use crate::providers::common::{extract_prompt_cache_settings, override_base_url, resolve_model};
use crate::providers::error_handling::{format_network_error, format_parse_error, handle_anthropic_http_error};
use crate::providers::openai::CustomProviderAuthHandle;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client as HttpClient;
use reqwest::StatusCode;
use serde_json::Value;
use std::env;

const ANTHROPIC_COMPACT_BETA: &str = "compact-2026-01-12";
const ANTHROPIC_CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";
const ANTHROPIC_ADVISOR_BETA: &str = "advisor-tool-2026-03-01";

/// Whether `base_url` points at the first-party Claude API, the only endpoint
/// VT Code talks to that accepts server-side refusal fallbacks.
fn is_first_party_anthropic_endpoint(base_url: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.eq_ignore_ascii_case("api.anthropic.com")))
        .unwrap_or(false)
}

#[derive(Clone)]
pub struct AnthropicProvider {
    api_key: String,
    http_client: HttpClient,
    base_url: String,
    model: String,
    prompt_cache_enabled: bool,
    prompt_cache_settings: AnthropicPromptCacheSettings,
    anthropic_config: AnthropicConfig,
    custom_provider_auth: Option<CustomProviderAuthHandle>,
    model_behavior: Option<ModelConfig>,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_model_internal(
            api_key,
            models::anthropic::DEFAULT_MODEL.to_string(),
            None,
            None,
            AnthropicConfig::default(),
            TimeoutsConfig::default(),
            None,
        )
    }

    fn with_model(api_key: String, model: String) -> Self {
        Self::with_model_internal(
            api_key,
            model,
            None,
            None,
            AnthropicConfig::default(),
            TimeoutsConfig::default(),
            None,
        )
    }

    pub(crate) fn new_with_client(
        api_key: String,
        model: String,
        http_client: reqwest::Client,
        base_url: String,
        _timeouts: TimeoutsConfig,
    ) -> Self {
        Self {
            api_key,
            http_client,
            base_url,
            model,
            prompt_cache_enabled: false,
            prompt_cache_settings: AnthropicPromptCacheSettings::default(),
            anthropic_config: AnthropicConfig::default(),
            custom_provider_auth: None,
            model_behavior: None,
        }
    }

    pub fn from_config(
        api_key: Option<String>,
        model: Option<String>,
        base_url: Option<String>,
        prompt_cache: Option<PromptCachingConfig>,
        timeouts: Option<TimeoutsConfig>,
        anthropic_config: Option<AnthropicConfig>,
        model_behavior: Option<ModelConfig>,
    ) -> Self {
        let api_key_value = api_key.unwrap_or_default();
        let model_value = resolve_model(model, models::anthropic::DEFAULT_MODEL);
        let anthropic_cfg = anthropic_config.unwrap_or_default();

        Self::with_model_internal(
            api_key_value,
            model_value,
            prompt_cache,
            base_url,
            anthropic_cfg,
            timeouts.unwrap_or_default(),
            model_behavior,
        )
    }

    fn with_model_internal(
        api_key: String,
        model: String,
        prompt_cache: Option<PromptCachingConfig>,
        base_url: Option<String>,
        anthropic_config: AnthropicConfig,
        timeouts: TimeoutsConfig,
        model_behavior: Option<ModelConfig>,
    ) -> Self {
        use crate::http_client::HttpClientFactory;

        let (prompt_cache_enabled, prompt_cache_settings) = extract_prompt_cache_settings(
            prompt_cache,
            |providers| &providers.anthropic,
            |cfg, provider_settings| cfg.enabled && provider_settings.enabled,
        );

        let base_url_value = if models::minimax::SUPPORTED_MODELS.contains(&model.as_str()) {
            Self::resolve_minimax_base_url(base_url)
        } else {
            override_base_url(urls::ANTHROPIC_API_BASE, base_url, Some(env_vars::ANTHROPIC_BASE_URL))
        };

        Self {
            api_key,
            http_client: HttpClientFactory::for_llm(&timeouts),
            base_url: base_url_value,
            model,
            prompt_cache_enabled,
            prompt_cache_settings,
            anthropic_config,
            custom_provider_auth: None,
            model_behavior,
        }
    }

    pub(crate) fn with_custom_auth(mut self, custom_provider_auth: Option<CustomProviderAuthHandle>) -> Self {
        self.custom_provider_auth = custom_provider_auth;
        self
    }

    fn resolve_minimax_base_url(base_url: Option<String>) -> String {
        fn sanitize(value: &str) -> Option<String> {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.trim_end_matches('/').to_string())
            }
        }

        fn is_official_minimax_host(url: &str) -> bool {
            let lower = url.to_ascii_lowercase();
            [
                "://api.minimax.io",
                "://platform.minimax.io",
                "api.minimax.io",
                "platform.minimax.io",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
        }

        let resolved = base_url
            .and_then(|value| sanitize(&value))
            .or_else(|| env::var(env_vars::MINIMAX_BASE_URL).ok().and_then(|value| sanitize(&value)))
            .or_else(|| sanitize(urls::MINIMAX_API_BASE))
            .unwrap_or_else(|| urls::MINIMAX_API_BASE.trim_end_matches('/').to_string());

        let mut normalized = resolved;

        if normalized.ends_with("/messages") {
            normalized = normalized.trim_end_matches("/messages").trim_end_matches('/').to_string();
        }

        if let Some(pos) = normalized.find("/v1/") {
            normalized = normalized[..pos + 3].to_string();
        }

        let mut without_v1 = normalized.trim_end_matches('/').to_string();
        if without_v1.ends_with("/v1") {
            without_v1 = without_v1.trim_end_matches("/v1").trim_end_matches('/').to_string();
        }

        if is_official_minimax_host(&without_v1) && !without_v1.to_ascii_lowercase().contains("/anthropic") {
            without_v1 = format!("{}/anthropic", without_v1.trim_end_matches('/'));
        }

        format!("{}/v1", without_v1.trim_end_matches('/'))
    }

    fn requires_advanced_tool_use_beta(&self, request: &LLMRequest) -> bool {
        request.tools.as_ref().is_some_and(|tools| {
            tools.iter().any(|tool| {
                (tool.is_tool_search() || tool.defer_loading.unwrap_or(false))
                    || tool.allowed_callers.as_ref().is_some_and(|callers| !callers.is_empty())
                    || tool.input_examples.as_ref().is_some_and(|examples| !examples.is_empty())
            })
        })
    }

    fn code_execution_betas(&self, request: &LLMRequest) -> Vec<String> {
        request
            .tools
            .as_ref()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| {
                        tool.is_anthropic_code_execution()
                            .then(|| code_execution_beta_name(&tool.tool_type))
                            .flatten()
                    })
                    .fold(Vec::new(), |mut betas, beta| {
                        if !betas.contains(&beta) {
                            betas.push(beta);
                        }
                        betas
                    })
            })
            .unwrap_or_default()
    }

    fn context_management_betas(&self, request: &LLMRequest) -> Vec<&'static str> {
        let mut betas = Vec::new();

        if request
            .tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(ToolDefinition::is_anthropic_memory_tool))
        {
            betas.push(ANTHROPIC_CONTEXT_MANAGEMENT_BETA);
        }

        if let Some(context_management) = request.context_management.as_ref() {
            if uses_anthropic_compaction(context_management) {
                betas.push(ANTHROPIC_COMPACT_BETA);
            }

            if uses_anthropic_context_edits(context_management) && !betas.contains(&ANTHROPIC_CONTEXT_MANAGEMENT_BETA) {
                betas.push(ANTHROPIC_CONTEXT_MANAGEMENT_BETA);
            }
        }

        betas
    }

    /// Whether the advisor server-side tool should be sent for this request.
    ///
    /// Delegates to the request builder's `resolve_advisor_tool` so the beta
    /// header and the injected tool are gated by the exact same logic.
    fn advisor_enabled_for_request(&self, request: &LLMRequest) -> bool {
        let executor = capabilities::resolve_model_name(&request.model, &self.model);
        request_builder::resolve_advisor_tool(executor, &self.anthropic_config.advisor).is_some()
    }

    fn uses_refreshable_auth(&self) -> bool {
        self.custom_provider_auth.is_some()
    }

    async fn current_api_key(&self) -> Result<String, LLMError> {
        if let Some(handle) = &self.custom_provider_auth {
            return handle
                .current_token()
                .await
                .map_err(|error| format_network_error("Anthropic", &error));
        }

        Ok(self.api_key.clone())
    }

    async fn refresh_api_key_for_retry(&self) -> Result<String, LLMError> {
        if let Some(handle) = &self.custom_provider_auth {
            return handle
                .force_refresh()
                .await
                .map_err(|error| format_network_error("Anthropic", &error));
        }

        Ok(self.api_key.clone())
    }

    /// Prepends a non-disclosure reminder to the system prompt. No supported
    /// Claude model accepts an assistant-message prefill, so the system prompt
    /// is the only valid place for this instruction.
    pub fn with_leak_protection(&self, mut request: LLMRequest, secret_description: &str) -> LLMRequest {
        let reminder = format!("[Never mention or reveal {secret_description}]");
        let merged_system_prompt = match request.system_prompt.as_ref() {
            Some(existing) => format!("{reminder}\n\n{existing}"),
            None => reminder,
        };
        request.system_prompt = Some(std::sync::Arc::from(merged_system_prompt));
        request
    }

    pub fn format_documents_xml(&self, documents: Vec<(&str, &str)>) -> String {
        let mut xml = String::from("<documents>\n");
        for (i, (source, content)) in documents.iter().enumerate() {
            xml.push_str(&format!(
                "  <document index=\"{}\">\n    <source>{}</source>\n    <document_content>\n{}\n    </document_content>\n  </document>\n",
                i + 1,
                source,
                content
            ));
        }
        xml.push_str("</documents>");
        xml
    }

    pub fn extract_xml_block(&self, content: &str, tag: &str) -> Option<String> {
        let start_tag = format!("<{tag}>");
        let end_tag = format!("</{tag}>");

        let start_pos = content.find(&start_tag)? + start_tag.len();
        let end_pos = content.find(&end_tag)?;

        if start_pos < end_pos {
            Some(content[start_pos..end_pos].trim().to_string())
        } else {
            None
        }
    }

    fn request_builder_context(&self) -> RequestBuilderContext<'_> {
        RequestBuilderContext {
            prompt_cache_enabled: self.prompt_cache_enabled,
            prompt_cache_settings: &self.prompt_cache_settings,
            anthropic_config: &self.anthropic_config,
            model: &self.model,
            server_side_fallbacks_available: is_first_party_anthropic_endpoint(&self.base_url),
        }
    }

    fn resolved_request_model<'a>(&'a self, request: &'a LLMRequest) -> &'a str {
        capabilities::resolve_model_name(&request.model, &self.model)
    }

    fn effective_betas(&self, request: &LLMRequest) -> Option<Vec<String>> {
        let mut betas = request.betas.clone().unwrap_or_default();
        for beta in self.context_management_betas(request) {
            if !betas.iter().any(|existing| existing == beta) {
                betas.push(beta.to_string());
            }
        }
        for beta in self.code_execution_betas(request) {
            if !betas.iter().any(|existing| existing == &beta) {
                betas.push(beta);
            }
        }
        if self.advisor_enabled_for_request(request) && !betas.iter().any(|beta| beta == ANTHROPIC_ADVISOR_BETA) {
            betas.push(ANTHROPIC_ADVISOR_BETA.to_string());
        }

        (!betas.is_empty()).then_some(betas)
    }

    fn convert_to_anthropic_format(&self, request: &LLMRequest) -> Result<Value, LLMError> {
        request_builder::convert_to_anthropic_format(request, &self.request_builder_context())
    }

    fn beta_header_for_request(
        &self,
        request: &LLMRequest,
        anthropic_request: &Value,
        include_advanced_tool_use: bool,
        request_betas: Option<&[String]>,
    ) -> Option<String> {
        let server_side_fallback = headers::ServerSideFallbackForm::of_request(anthropic_request);
        let beta_config = headers::BetaHeaderConfig {
            config: &self.anthropic_config,
            model: self.resolved_request_model(request),
            include_advanced_tool_use,
            include_manual_interleaved_beta: anthropic_request
                .get("thinking")
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                == Some("enabled"),
            request_betas,
            include_task_budget: anthropic_request
                .get("output_config")
                .and_then(|value| value.get("task_budget"))
                .is_some(),
            server_side_fallback,
            // The credit beta must accompany the original request for a
            // refusal to carry `fallback_credit_token`. The default-form
            // fallback beta already grants those fields; the list form does
            // not, so it needs the credit beta alongside it.
            include_fallback_credit: request.fallback_credit_token.is_some()
                || server_side_fallback == Some(headers::ServerSideFallbackForm::List),
            include_mid_conversation_tool_changes: false,
            include_mid_conversation_system_clear_at: capabilities::supports_turn_scoped_system_messages(
                self.resolved_request_model(request),
                &self.model,
            ) && anthropic_request
                .get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages
                        .iter()
                        .any(|message| message.get("clear_at").and_then(Value::as_str) == Some("next_user_message"))
                }),
            // The primary config or any server-side fallback entry may carry
            // `display: "updates"`; either needs the beta.
            include_thinking_display_updates: std::iter::once(anthropic_request.get("thinking"))
                .chain(
                    anthropic_request
                        .get("fallbacks")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|fallback| fallback.get("thinking")),
                )
                .flatten()
                .any(|thinking| thinking.get("display").and_then(Value::as_str) == Some("updates")),
        };

        headers::combined_beta_header_value(self.prompt_cache_enabled, &self.prompt_cache_settings, &beta_config)
    }

    async fn send_request(
        &self,
        request: &LLMRequest,
        anthropic_request: &Value,
    ) -> Result<AnthropicHttpResponse, LLMError> {
        let include_advanced_tool_use = self.requires_advanced_tool_use_beta(request);
        let betas = self.effective_betas(request);
        let url = format!("{}/messages", self.base_url);

        let beta_header =
            self.beta_header_for_request(request, anthropic_request, include_advanced_tool_use, betas.as_deref());
        let metadata = request.metadata.clone();

        let send_once = |api_key: String| {
            let mut request_builder = self
                .http_client
                .post(&url)
                .header("x-api-key", api_key)
                .header("anthropic-version", urls::ANTHROPIC_API_VERSION);

            if let Some(beta_header) = beta_header.clone() {
                request_builder = request_builder.header("anthropic-beta", beta_header);
            }

            if let Some(metadata) = metadata.as_ref()
                && let Ok(metadata_str) = serde_json::to_string(metadata)
            {
                request_builder = request_builder.header("X-Turn-Metadata", metadata_str);
            }

            request_builder.json(anthropic_request)
        };

        let response = send_once(self.current_api_key().await?)
            .send()
            .await
            .map_err(|e| format_network_error("Anthropic", &e))?;

        let response = if self.uses_refreshable_auth()
            && matches!(response.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        {
            send_once(self.refresh_api_key_for_retry().await?)
                .send()
                .await
                .map_err(|e| format_network_error("Anthropic", &e))?
        } else {
            response
        };

        let response = handle_anthropic_http_error(response).await?;

        let request_id = response
            .headers()
            .get("request-id")
            .and_then(|h| h.to_str().ok().map(|s| s.to_string()));
        let organization_id = response
            .headers()
            .get("anthropic-organization-id")
            .and_then(|h| h.to_str().ok().map(|s| s.to_string()));

        Ok(AnthropicHttpResponse { response, request_id, organization_id })
    }
}

fn code_execution_beta_name(tool_type: &str) -> Option<String> {
    let suffix = tool_type.strip_prefix("code_execution_")?;
    if suffix.len() != 8 || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }

    Some(format!("code-execution-{}-{}-{}", &suffix[0..4], &suffix[4..6], &suffix[6..8]))
}

fn uses_anthropic_compaction(context_management: &Value) -> bool {
    context_management
        .as_array()
        .is_some_and(|items| items.iter().any(is_compaction_item))
        || context_management
            .get("edits")
            .and_then(Value::as_array)
            .is_some_and(|edits| edits.iter().any(is_compaction_edit_item))
}

fn is_compaction_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("compaction")
}

fn is_compaction_edit_item(item: &Value) -> bool {
    item.get("type")
        .and_then(Value::as_str)
        .is_some_and(|edit_type| edit_type.starts_with("compact_"))
}

fn uses_anthropic_context_edits(context_management: &Value) -> bool {
    context_management
        .get("edits")
        .and_then(Value::as_array)
        .is_some_and(|edits| edits.iter().any(is_context_edit_item))
}

fn is_context_edit_item(item: &Value) -> bool {
    item.get("type")
        .and_then(Value::as_str)
        .is_some_and(|edit_type| edit_type.starts_with("clear_tool_uses_") || edit_type.starts_with("clear_thinking_"))
}

struct AnthropicHttpResponse {
    response: reqwest::Response,
    request_id: Option<String>,
    organization_id: Option<String>,
}

impl AnthropicProvider {
    async fn generate_with_body(
        &self,
        request: &LLMRequest,
        anthropic_request: &Value,
        model: String,
    ) -> Result<LLMResponse, LLMError> {
        let AnthropicHttpResponse { response, request_id, organization_id } =
            self.send_request(request, anthropic_request).await?;

        let anthropic_response: Value = response.json().await.map_err(|e| format_parse_error("Anthropic", &e))?;

        let mut llm_response = response_parser::parse_response(anthropic_response, model)?;
        llm_response.request_id = request_id;
        llm_response.organization_id = organization_id;
        Ok(llm_response)
    }

    /// Sends the one bounded refusal retry described in `fallback_retry`. A
    /// 400 naming the credit token is resent once without it; any other
    /// failure is logged and reported as `None` so the caller keeps the
    /// original refusal.
    async fn send_refusal_retry(
        &self,
        request: &LLMRequest,
        original_body: &Value,
        plan: &RefusalRetryPlan,
    ) -> Option<AnthropicHttpResponse> {
        let fallback_retry::RefusalRetryBody { mut body, with_token } = plan.retry_body(original_body, &self.model);
        tracing::info!(
            provider = "anthropic",
            recommended_model = %plan.model,
            with_credit_token = with_token,
            "refusal fallback could not run server-side; retrying once on the recommended model"
        );
        match self.send_request(&plan.retry_request(request, with_token), &body).await {
            Ok(response) => Some(response),
            Err(err) if with_token && fallback_retry::rejects_credit_token(&err) => {
                tracing::warn!(
                    provider = "anthropic",
                    error = %err,
                    "fallback credit token rejected; resending the refusal retry without it"
                );
                fallback_retry::strip_credit_token(&mut body);
                match self.send_request(&plan.retry_request(request, false), &body).await {
                    Ok(response) => Some(response),
                    Err(err) => {
                        tracing::warn!(provider = "anthropic", error = %err, "refusal retry failed");
                        None
                    }
                }
            }
            Err(err) => {
                tracing::warn!(provider = "anthropic", error = %err, "refusal retry failed");
                None
            }
        }
    }

    async fn retry_refused_generate(
        &self,
        request: &LLMRequest,
        original_body: &Value,
        plan: &RefusalRetryPlan,
        refused: LLMResponse,
    ) -> Result<LLMResponse, LLMError> {
        let Some(AnthropicHttpResponse { response, request_id, organization_id }) =
            self.send_refusal_retry(request, original_body, plan).await
        else {
            return Ok(refused);
        };
        let parsed = match response.json::<Value>().await {
            Ok(value) => response_parser::parse_response(value, plan.model.clone()),
            Err(err) => Err(format_parse_error("Anthropic", &err)),
        };
        match parsed {
            Ok(mut retried) => {
                retried.request_id = request_id;
                retried.organization_id = organization_id;
                Ok(retried)
            }
            Err(err) => {
                tracing::warn!(provider = "anthropic", error = %err, "refusal retry response could not be parsed");
                Ok(refused)
            }
        }
    }

    /// Wraps a primary stream so a pre-output refusal eligible for the
    /// bounded retry is replaced by the retry's stream. Every other event,
    /// including a refusal that is not retried, passes through unchanged.
    fn stream_with_refusal_retry(
        &self,
        primary: LLMStream,
        request: LLMRequest,
        original_body: Value,
        model: String,
    ) -> LLMStream {
        let provider = self.clone();
        Box::pin(async_stream::stream! {
            let mut primary = primary;
            while let Some(event) = primary.next().await {
                let response = match event {
                    Ok(LLMStreamEvent::Completed { response }) => response,
                    other => {
                        yield other;
                        continue;
                    }
                };
                let retry = match RefusalRetryPlan::for_response(&response, &model) {
                    Some(plan) => provider
                        .send_refusal_retry(&request, &original_body, &plan)
                        .await
                        .map(|http| (plan, http)),
                    None => None,
                };
                match retry {
                    Some((plan, AnthropicHttpResponse { response: http, request_id, organization_id })) => {
                        let mut retried =
                            stream_decoder::create_stream(http, plan.model, request_id, organization_id);
                        while let Some(event) = retried.next().await {
                            yield event;
                        }
                    }
                    None => yield Ok(LLMStreamEvent::Completed { response }),
                }
            }
        })
    }
}

#[async_trait]
impl LLMProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_non_streaming(&self, _model: &str) -> bool {
        // Pinned so the stream-timeout fallback cannot silently regress.
        true
    }

    fn supports_reasoning(&self, model: &str) -> bool {
        // Codex-inspired robustness: Setting model_supports_reasoning to false
        // does NOT disable it for known reasoning models.
        capabilities::supports_reasoning(model, &self.model)
            || self
                .model_behavior
                .as_ref()
                .and_then(|b| b.model_supports_reasoning)
                .unwrap_or(false)
    }

    fn supported_reasoning_efforts(&self, model: &str) -> &'static [&'static str] {
        capabilities::allowed_efforts_for_model(model, &self.model).unwrap_or_else(|| {
            self.model_behavior
                .as_ref()
                .and_then(|behavior| behavior.model_supports_reasoning_effort)
                .filter(|supported| *supported)
                .map(|_| crate::provider::GENERIC_REASONING_EFFORTS)
                .unwrap_or(&[])
        })
    }

    fn supports_reasoning_effort(&self, model: &str) -> bool {
        // Same robustness logic for reasoning effort
        capabilities::supports_reasoning_effort(model, &self.model)
            || self
                .model_behavior
                .as_ref()
                .and_then(|b| b.model_supports_reasoning_effort)
                .unwrap_or(false)
    }

    fn supports_parallel_tool_config(&self, model: &str) -> bool {
        capabilities::supports_parallel_tool_config(model)
    }

    fn supports_context_edits(&self, _model: &str) -> bool {
        true
    }

    fn supports_turn_scoped_system_messages(&self, model: &str) -> bool {
        capabilities::supports_turn_scoped_system_messages(model, &self.model)
    }

    fn supports_responses_compaction(&self, model: &str) -> bool {
        // Anthropic server-side compaction is supported on Claude Opus 4.x
        // and Sonnet 4.6+ models via context_management.edits.
        capabilities::supports_compaction(model)
    }

    fn supports_native_inline_compaction(&self, model: &str) -> bool {
        // Anthropic drives compaction inline via the `compact_20260112`
        // context-management edit on a `generate` request, so it is the
        // `NativeInline` strategy provider.
        capabilities::supports_compaction(model)
    }

    fn effective_context_size(&self, model: &str) -> usize {
        capabilities::effective_context_size(model)
    }

    fn supports_structured_output(&self, model: &str) -> bool {
        capabilities::supports_structured_output(model, &self.model)
    }

    fn supports_vision(&self, model: &str) -> bool {
        capabilities::supports_vision(model, &self.model)
    }

    async fn generate(&self, request: LLMRequest) -> Result<LLMResponse, LLMError> {
        let resolved_model = self.resolved_request_model(&request).to_string();
        let anthropic_request = self.convert_to_anthropic_format(&request)?;

        let response = self
            .generate_with_body(&request, &anthropic_request, resolved_model.clone())
            .await?;
        match RefusalRetryPlan::for_response(&response, &resolved_model) {
            Some(plan) => self.retry_refused_generate(&request, &anthropic_request, &plan, response).await,
            None => Ok(response),
        }
    }

    async fn stream(&self, request: LLMRequest) -> Result<LLMStream, LLMError> {
        let resolved_model = self.resolved_request_model(&request).to_string();
        let mut anthropic_request = self.convert_to_anthropic_format(&request)?;

        if let Some(obj) = anthropic_request.as_object_mut() {
            obj.insert("stream".to_string(), Value::Bool(true));
        }

        let AnthropicHttpResponse { response, request_id, organization_id } =
            self.send_request(&request, &anthropic_request).await?;

        let primary = stream_decoder::create_stream(response, resolved_model.clone(), request_id, organization_id);
        Ok(self.stream_with_refusal_retry(primary, request, anthropic_request, resolved_model))
    }

    fn supported_models(&self) -> Vec<String> {
        capabilities::supported_models()
    }

    fn validate_request(&self, request: &LLMRequest) -> Result<(), LLMError> {
        validation::validate_request(request, &self.model, &self.anthropic_config, "Anthropic")
    }
}

#[async_trait]
impl LLMClient for AnthropicProvider {
    async fn generate(&mut self, prompt: &str) -> Result<LLMResponse, LLMError> {
        let request = crate::providers::common::make_default_request(prompt, &self.model);
        let request_model = request.model.clone();
        let response = LLMProvider::generate(self, request).await?;

        Ok(LLMResponse {
            content: Some(response.content.unwrap_or_default()),
            model: request_model,
            usage: response.usage.map(crate::providers::common::convert_usage_to_llm_types),
            reasoning: response.reasoning,
            reasoning_details: response.reasoning_details,
            request_id: response.request_id,
            organization_id: response.organization_id,
            finish_reason: response.finish_reason,
            tool_calls: response.tool_calls,
            tool_references: response.tool_references,
            compaction: response.compaction,
        })
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::{AnthropicProvider, capabilities, code_execution_beta_name, headers};
    use crate::provider::{
        ContentPart, LLMProvider, LLMRequest, LLMStreamEvent, Message, MessageContent, ToolDefinition,
    };
    use futures::StreamExt;
    use serde_json::json;
    use vtcode_config::constants::models;

    #[tokio::test]
    async fn generate_sends_inline_compaction_and_parses_compaction_response() {
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("anthropic-beta", "compact-2026-01-12"))
            .and(body_partial_json(json!({
                "context_management": {
                    "edits": [{"type": "compact_20260112"}]
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "compaction", "content": "opaque summary"}],
                "stop_reason": "compaction"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new_with_client(
            "test-key".to_string(),
            models::CLAUDE_SONNET_5.to_string(),
            reqwest::Client::builder().no_proxy().build().expect("test client should build"),
            format!("{}/v1", server.uri()),
            vtcode_config::TimeoutsConfig::default(),
        );
        let response = LLMProvider::generate(
            &provider,
            LLMRequest {
                model: models::CLAUDE_SONNET_5.to_string(),
                messages: vec![Message::user("compact this".to_string())].into(),
                context_management: Some(json!({
                    "edits": [{
                        "type": "compact_20260112",
                        "trigger": {"type": "input_tokens", "value": 50_000},
                        "pause_after_compaction": true
                    }]
                })),
                ..Default::default()
            },
        )
        .await
        .expect("inline compaction request should succeed");

        assert!(matches!(response.finish_reason, crate::provider::FinishReason::Pause));
        assert_eq!(response.compaction.as_deref(), Some("opaque summary"));
    }

    #[tokio::test]
    async fn stream_preserves_anthropic_compaction_block_and_iteration_usage() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-5\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"compaction\",\"content\":null,\"signature\":\"signed-summary\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"compaction_delta\",\"content\":null,\"encrypted_content\":\"opaque-extension\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"compaction_delta\",\"content\":\"opaque \"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"compaction_delta\",\"content\":\"summary\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"compaction\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":null,\"output_tokens\":0,\"iterations\":[{\"type\":\"compaction\",\"input_tokens\":50,\"output_tokens\":5},{\"type\":\"message\",\"model\":null,\"input_tokens\":10,\"output_tokens\":0},{\"type\":\"advisor_message\",\"model\":\"claude-opus-5\",\"input_tokens\":7,\"output_tokens\":2}]}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(json!({"stream": true})))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new_with_client(
            "test-key".to_string(),
            models::CLAUDE_SONNET_5.to_string(),
            reqwest::Client::builder().no_proxy().build().expect("test client should build"),
            format!("{}/v1", server.uri()),
            vtcode_config::TimeoutsConfig::default(),
        );
        let mut stream = LLMProvider::stream(
            &provider,
            LLMRequest {
                model: models::CLAUDE_SONNET_5.to_string(),
                messages: vec![Message::user("continue after compaction".to_string())].into(),
                ..Default::default()
            },
        )
        .await
        .expect("stream request should succeed");

        let mut completed = None;
        while let Some(event) = stream.next().await {
            match event.expect("stream event") {
                LLMStreamEvent::Completed { response } => completed = Some(*response),
                LLMStreamEvent::Token { .. }
                | LLMStreamEvent::Reasoning { .. }
                | LLMStreamEvent::ReasoningSignature { .. }
                | LLMStreamEvent::ReasoningStage { .. } => {}
            }
        }

        let response = completed.expect("completed stream response");
        assert_eq!(response.compaction.as_deref(), Some("opaque summary"));
        let details = response.reasoning_details.expect("compaction detail");
        assert_eq!(details.len(), 1);
        let detail: serde_json::Value = serde_json::from_str(&details[0]).expect("serialized compaction detail");
        assert_eq!(detail["content"], "opaque summary");
        assert_eq!(detail["signature"], "signed-summary");
        assert_eq!(detail["encrypted_content"], "opaque-extension");

        let usage = response.usage.expect("usage");
        let totals = usage.billable_totals();
        assert_eq!(totals.prompt_tokens, 67);
        assert_eq!(totals.completion_tokens, 7);
    }

    #[tokio::test]
    async fn stream_carries_refusal_stop_details_from_message_delta() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-5\",\"stop_reason\":null,\"stop_sequence\":null,\"stop_details\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_sequence\":null,\"stop_details\":{\"type\":\"refusal\",\"category\":\"cyber\",\"explanation\":\"declined\",\"fallback_credit_token\":\"credit-1\",\"fallback_has_prefill_claim\":true}},\"usage\":{\"output_tokens\":0}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(json!({"stream": true})))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new_with_client(
            "test-key".to_string(),
            models::CLAUDE_SONNET_5.to_string(),
            reqwest::Client::builder().no_proxy().build().expect("test client should build"),
            format!("{}/v1", server.uri()),
            vtcode_config::TimeoutsConfig::default(),
        );
        let mut stream = LLMProvider::stream(
            &provider,
            LLMRequest {
                model: models::CLAUDE_SONNET_5.to_string(),
                messages: vec![Message::user("hello".to_string())].into(),
                ..Default::default()
            },
        )
        .await
        .expect("stream request should succeed");

        let mut completed = None;
        while let Some(event) = stream.next().await {
            if let LLMStreamEvent::Completed { response } = event.expect("stream event") {
                completed = Some(*response);
            }
        }

        let response = completed.expect("completed stream response");
        assert!(matches!(response.finish_reason, crate::provider::FinishReason::Refusal));
        let details = response.reasoning_details.expect("stop_details detail");
        assert_eq!(details.len(), 1);
        let detail: serde_json::Value = serde_json::from_str(&details[0]).expect("serialized stop_details");
        assert_eq!(detail["type"], "stop_details");
        assert_eq!(detail["category"], "cyber");
        assert_eq!(detail["explanation"], "declined");
        assert_eq!(detail["fallback_credit_token"], "credit-1");
        assert_eq!(detail["fallback_has_prefill_claim"], true);
    }

    #[tokio::test]
    async fn stream_records_interleaved_block_order_for_replay() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-5-5\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-1\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Reading \"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"the parser.\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Checking entry.\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-2\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"src/parser.rs\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":12}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;

        let model = models::anthropic::CLAUDE_OPUS_5_5;
        let provider = AnthropicProvider::new_with_client(
            "test-key".to_string(),
            model.to_string(),
            reqwest::Client::builder().no_proxy().build().expect("test client should build"),
            format!("{}/v1", server.uri()),
            vtcode_config::TimeoutsConfig::default(),
        );
        let mut stream = LLMProvider::stream(
            &provider,
            LLMRequest {
                model: model.to_string(),
                messages: vec![Message::user("fix the parser".to_string())].into(),
                ..Default::default()
            },
        )
        .await
        .expect("stream request should succeed");

        let mut completed = None;
        while let Some(event) = stream.next().await {
            if let LLMStreamEvent::Completed { response } = event.expect("stream event") {
                completed = Some(*response);
            }
        }
        let response = completed.expect("completed stream response");
        let tool_calls = response.tool_calls.clone().expect("tool calls");
        let details = response
            .reasoning_details
            .clone()
            .map(|details| details.into_iter().map(serde_json::Value::String).collect());
        let assistant = Message::assistant_with_tools_and_reasoning(
            response.content.clone().expect("text content"),
            tool_calls,
            details,
        );

        let replay = LLMRequest {
            model: model.to_string(),
            messages: vec![
                Message::user("fix the parser".to_string()),
                assistant,
                Message::tool_response("toolu_1".to_string(), "fn parse() {}".to_string()),
            ]
            .into(),
            ..Default::default()
        };
        let payload = provider.convert_to_anthropic_format(&replay).expect("payload conversion");
        assert_eq!(
            payload["messages"][1]["content"],
            json!([
                { "type": "thinking", "thinking": "", "signature": "sig-1" },
                { "type": "text", "text": "Reading the parser." },
                { "type": "thinking", "thinking": "Checking entry.", "signature": "sig-2" },
                { "type": "tool_use", "id": "toolu_1", "name": "read_file", "input": { "path": "src/parser.rs" } }
            ])
        );
    }

    #[tokio::test]
    async fn stream_mid_output_fallback_drops_declined_thinking_and_tool_use() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-fable-5-1\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Refused model reasoning.\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-1\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Checking. \"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_declined\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"src/\"}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"fallback\",\"from\":{\"model\":\"claude-fable-5-1\"},\"to\":{\"model\":\"claude-opus-4-8\"}}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":4,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"text_delta\",\"text\":\"Here is the answer.\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":4}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":12}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;

        let model = models::anthropic::CLAUDE_OPUS_5_5;
        let provider = AnthropicProvider::new_with_client(
            "test-key".to_string(),
            model.to_string(),
            reqwest::Client::builder().no_proxy().build().expect("test client should build"),
            format!("{}/v1", server.uri()),
            vtcode_config::TimeoutsConfig::default(),
        );
        let mut stream = LLMProvider::stream(
            &provider,
            LLMRequest {
                model: model.to_string(),
                messages: vec![Message::user("fix the parser".to_string())].into(),
                ..Default::default()
            },
        )
        .await
        .expect("stream request should succeed");

        let mut completed = None;
        while let Some(event) = stream.next().await {
            if let LLMStreamEvent::Completed { response } = event.expect("stream event") {
                completed = Some(*response);
            }
        }
        let response = completed.expect("completed stream response");
        assert!(response.tool_calls.is_none(), "declined tool_use must not run");
        assert_eq!(response.content.as_deref(), Some("Checking. Here is the answer."));
        let raw_details = response.reasoning_details.clone().expect("details");
        let details: Vec<serde_json::Value> = raw_details
            .iter()
            .map(|detail| serde_json::from_str(detail).expect("detail json"))
            .collect();
        assert!(details.iter().all(|detail| detail["type"] != "thinking"));
        assert!(details.iter().any(|detail| {
            detail["type"] == "fallback"
                && detail["from"]["model"] == "claude-fable-5-1"
                && detail["to"]["model"] == "claude-opus-4-8"
        }));

        let assistant = Message::assistant(response.content.clone().expect("text content"))
            .with_reasoning_details(Some(raw_details.into_iter().map(serde_json::Value::String).collect()));
        let replay = LLMRequest {
            model: model.to_string(),
            messages: vec![
                Message::user("fix the parser".to_string()),
                assistant,
                Message::user("thanks".to_string()),
            ]
            .into(),
            ..Default::default()
        };
        let payload = provider.convert_to_anthropic_format(&replay).expect("payload conversion");
        assert_eq!(
            payload["messages"][1]["content"],
            json!([
                { "type": "text", "text": "Checking. " },
                { "type": "text", "text": "Here is the answer." }
            ])
        );
    }

    #[test]
    fn non_streaming_capability_is_pinned_for_stream_timeout_fallback() {
        // Pinned true in the provider impl; MinimaxProvider's delegation and
        // the runloop's stream-timeout fallback both depend on this value.
        let provider = AnthropicProvider::new("test-key".to_string());
        assert!(LLMProvider::supports_non_streaming(&provider, models::anthropic::CLAUDE_OPUS_5));
    }

    #[test]
    fn with_leak_protection_prepends_reminder_to_system_prompt_for_every_model() {
        for model in [
            models::CLAUDE_SONNET_5,
            models::anthropic::CLAUDE_OPUS_5_5,
            "claude-unlisted-model",
        ] {
            let provider = AnthropicProvider::with_model("test-key".to_string(), model.to_string());
            let request = LLMRequest {
                model: model.to_string(),
                messages: vec![Message::user("hi".to_string())].into(),
                system_prompt: Some(std::sync::Arc::from("Base instructions.")),
                ..Default::default()
            };

            let protected = provider.with_leak_protection(request, "the API key");

            assert_eq!(
                protected.system_prompt.as_deref(),
                Some("[Never mention or reveal the API key]\n\nBase instructions."),
                "model {model}"
            );
            let payload = provider.convert_to_anthropic_format(&protected).expect("payload conversion");
            let messages = payload["messages"].as_array().expect("messages array");
            assert_eq!(messages.last().expect("last message")["role"], "user", "model {model}");
        }
    }

    #[test]
    fn with_leak_protection_sets_system_prompt_when_absent() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("hi".to_string())].into(),
            ..Default::default()
        };

        let protected = provider.with_leak_protection(request, "internal notes");

        assert_eq!(protected.system_prompt.as_deref(), Some("[Never mention or reveal internal notes]"));
    }

    #[test]
    fn resolve_minimax_base_url_defaults_to_anthropic_v1() {
        assert_eq!(AnthropicProvider::resolve_minimax_base_url(None), "https://api.minimax.io/anthropic/v1");
    }

    #[test]
    fn resolve_minimax_base_url_normalizes_root_host_to_anthropic_v1() {
        assert_eq!(
            AnthropicProvider::resolve_minimax_base_url(Some("https://api.minimax.io".to_string())),
            "https://api.minimax.io/anthropic/v1"
        );
        assert_eq!(
            AnthropicProvider::resolve_minimax_base_url(Some("https://api.minimax.io/v1".to_string())),
            "https://api.minimax.io/anthropic/v1"
        );
    }

    #[test]
    fn resolve_minimax_base_url_keeps_explicit_anthropic_path() {
        assert_eq!(
            AnthropicProvider::resolve_minimax_base_url(Some("https://api.minimax.io/anthropic".to_string())),
            "https://api.minimax.io/anthropic/v1"
        );
        assert_eq!(
            AnthropicProvider::resolve_minimax_base_url(Some(
                "https://api.minimax.io/anthropic/v1/messages".to_string()
            )),
            "https://api.minimax.io/anthropic/v1"
        );
    }

    #[test]
    fn resolve_minimax_base_url_respects_custom_proxy_path() {
        assert_eq!(
            AnthropicProvider::resolve_minimax_base_url(Some("https://proxy.example.com/minimax/v1".to_string())),
            "https://proxy.example.com/minimax/v1"
        );
    }

    #[test]
    fn native_structured_outputs_do_not_require_structured_output_beta() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            output_format: Some(json!({
                "type": "object",
                "properties": {
                    "answer": {"type": "string"}
                },
                "required": ["answer"],
                "additionalProperties": false
            })),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        let beta_header = provider.beta_header_for_request(&request, &payload, false, None);

        assert_eq!(payload["output_config"]["format"]["type"], "json_schema");
        if let Some(header) = &beta_header {
            assert!(!header.contains("structured-outputs-2025-11-13"));
        }
    }

    #[test]
    fn effective_betas_include_code_execution_but_not_files_api_for_file_inputs() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message {
                role: crate::provider::MessageRole::User,
                content: MessageContent::Parts(vec![
                    ContentPart::text("Analyze this CSV".to_string()),
                    ContentPart::file_from_id("file_abc123".to_string()),
                ]),
                ..Default::default()
            }]
            .into(),
            tools: Some(std::sync::Arc::new(vec![ToolDefinition {
                tool_type: "code_execution_20250825".to_string(),
                function: None,
                allowed_callers: None,
                input_examples: None,
                web_search: None,
                hosted_tool_config: None,
                shell: None,
                grammar: None,
                strict: None,
                defer_loading: None,
                namespace: None,
                advisor: None,
            }])),
            ..Default::default()
        };

        let betas = provider.effective_betas(&request).expect("betas");
        assert!(betas.iter().any(|beta| beta == "code-execution-2025-08-25"));
        // The Files API is GA: file_id inputs need no beta header.
        assert!(!betas.iter().any(|beta| beta.starts_with("files-api")), "betas: {betas:?}");
    }

    #[test]
    fn effective_betas_include_context_management_beta_for_memory_tools() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("remember this preference".to_string())].into(),
            tools: Some(std::sync::Arc::new(vec![ToolDefinition {
                tool_type: "memory_20250818".to_string(),
                function: None,
                allowed_callers: None,
                input_examples: None,
                web_search: None,
                hosted_tool_config: None,
                shell: None,
                grammar: None,
                strict: None,
                defer_loading: None,
                namespace: None,
                advisor: None,
            }])),
            ..Default::default()
        };

        let betas = provider.effective_betas(&request).expect("betas");
        assert!(betas.iter().any(|beta| beta == "context-management-2025-06-27"));
    }

    #[test]
    fn effective_betas_include_context_management_beta_for_context_edits() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("continue".to_string())].into(),
            context_management: Some(json!({
                "edits": [
                    {"type": "clear_tool_uses_20250919"}
                ]
            })),
            ..Default::default()
        };

        let betas = provider.effective_betas(&request).expect("betas");
        assert!(betas.iter().any(|beta| beta == "context-management-2025-06-27"));
        assert!(!betas.iter().any(|beta| beta == "compact-2026-01-12"));
    }

    #[test]
    fn effective_betas_include_compact_beta_for_compaction_requests() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("continue".to_string())].into(),
            context_management: Some(json!([
                {
                    "type": "compaction",
                    "compact_threshold": 180000
                }
            ])),
            ..Default::default()
        };

        let betas = provider.effective_betas(&request).expect("betas");
        assert!(betas.iter().any(|beta| beta == "compact-2026-01-12"));
    }

    #[test]
    fn effective_betas_include_compact_beta_for_compaction_edits() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("continue".to_string())].into(),
            context_management: Some(json!({
                "edits": [
                    {
                        "type": "compact_20260112",
                        "trigger": {
                            "type": "input_tokens",
                            "value": 180000
                        }
                    }
                ]
            })),
            ..Default::default()
        };

        let betas = provider.effective_betas(&request).expect("betas");
        assert!(betas.iter().any(|beta| beta == "compact-2026-01-12"));
        assert!(!betas.iter().any(|beta| beta == "context-management-2025-06-27"));
    }

    #[test]
    fn effective_betas_include_both_headers_for_mixed_context_edits() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("continue".to_string())].into(),
            context_management: Some(json!({
                "edits": [
                    {"type": "clear_tool_uses_20250919"},
                    {
                        "type": "compact_20260112",
                        "trigger": {
                            "type": "input_tokens",
                            "value": 180000
                        }
                    }
                ]
            })),
            ..Default::default()
        };

        let betas = provider.effective_betas(&request).expect("betas");
        assert!(betas.iter().any(|beta| beta == "compact-2026-01-12"));
        assert!(betas.iter().any(|beta| beta == "context-management-2025-06-27"));
    }

    #[test]
    fn beta_header_includes_advanced_tool_use_for_programmatic_tools() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("find warmest city".to_string())].into(),
            tools: Some(std::sync::Arc::new(vec![
                ToolDefinition::function(
                    "get_weather".to_string(),
                    "Get weather for a city".to_string(),
                    json!({
                        "type": "object",
                        "properties": {
                            "city": {"type": "string"}
                        },
                        "required": ["city"]
                    }),
                )
                .with_allowed_callers(vec!["code_execution_20250825".to_string()]),
            ])),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        let beta_header = provider
            .beta_header_for_request(&request, &payload, true, None)
            .expect("beta header");

        assert!(beta_header.contains("advanced-tool-use-2025-11-20"));
    }

    #[test]
    fn beta_header_omits_context_1m_for_native_1m_models() {
        let model = models::CLAUDE_SONNET_5;
        let provider = AnthropicProvider::with_model("test-key".to_string(), model.to_string());
        let request = LLMRequest {
            model: model.to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        let beta_header = provider.beta_header_for_request(&request, &payload, false, None);

        if let Some(header) = &beta_header {
            assert!(!header.contains("context-1m-2025-08-07"));
        }
    }

    #[test]
    fn beta_header_uses_request_model_instead_of_provider_default() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        let beta_header = provider.beta_header_for_request(&request, &payload, false, None);

        assert_eq!(payload["model"], models::CLAUDE_SONNET_5);
        if let Some(header) = &beta_header {
            assert!(!header.contains("interleaved-thinking-2025-05-14"));
        }
    }

    #[test]
    fn beta_header_omits_interleaved_thinking_for_sonnet_5_adaptive_mode() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            thinking_budget: Some(4096),
            max_tokens: Some(8192),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        let beta_header = provider.beta_header_for_request(&request, &payload, false, None);

        assert_eq!(payload["thinking"]["type"], "adaptive");
        if let Some(header) = &beta_header {
            assert!(!header.contains("interleaved-thinking-2025-05-14"));
        }
    }

    #[test]
    fn opus_5_5_requests_progress_updates_display_with_its_beta() {
        let model = models::anthropic::CLAUDE_OPUS_5_5;
        let provider = AnthropicProvider::with_model("test-key".to_string(), model.to_string());
        let request = LLMRequest {
            model: model.to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        assert_eq!(payload["thinking"]["type"], "adaptive");
        assert_eq!(payload["thinking"]["display"], "updates");

        let beta_header = provider
            .beta_header_for_request(&request, &payload, false, None)
            .expect("display updates beta header");
        assert_eq!(
            beta_header
                .split(", ")
                .filter(|beta| *beta == headers::THINKING_DISPLAY_UPDATES_BETA)
                .count(),
            1
        );
    }

    #[test]
    fn display_updates_beta_covers_fallback_entries() {
        let model = models::anthropic::CLAUDE_OPUS_5;
        let provider = AnthropicProvider::with_model("test-key".to_string(), model.to_string());
        let request = LLMRequest {
            model: model.to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            fallbacks: Some(vec![crate::provider::FallbackModel {
                model: models::anthropic::CLAUDE_OPUS_5_5.to_string(),
                max_tokens: None,
                thinking: Some(crate::provider::AnthropicThinkingConfig::Adaptive {
                    display: Some("updates".to_string()),
                }),
            }]),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        assert!(payload["thinking"].get("display").is_none());
        assert_eq!(payload["fallbacks"][0]["thinking"]["display"], "updates");
        let beta_header = provider
            .beta_header_for_request(&request, &payload, false, None)
            .expect("beta header");
        assert!(
            beta_header
                .split(", ")
                .any(|beta| beta == headers::THINKING_DISPLAY_UPDATES_BETA)
        );
    }

    #[test]
    fn display_updates_beta_is_omitted_when_display_is_not_updates() {
        let model = models::anthropic::CLAUDE_SONNET_5;
        let provider = AnthropicProvider::with_model("test-key".to_string(), model.to_string());
        let request = LLMRequest {
            model: model.to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        assert!(payload["thinking"].get("display").is_none());
        let beta_header = provider.beta_header_for_request(&request, &payload, false, None);
        assert!(
            !beta_header.is_some_and(|header| {
                header.split(", ").any(|beta| beta == headers::THINKING_DISPLAY_UPDATES_BETA)
            })
        );
    }

    fn first_party_provider(model: &str) -> AnthropicProvider {
        AnthropicProvider::new_with_client(
            "test-key".to_string(),
            model.to_string(),
            reqwest::Client::new(),
            vtcode_config::constants::urls::ANTHROPIC_API_BASE.to_string(),
            vtcode_config::TimeoutsConfig::default(),
        )
    }

    fn split_betas(header: Option<String>) -> Vec<String> {
        header
            .map(|header| header.split(", ").map(str::to_string).collect())
            .unwrap_or_default()
    }

    #[test]
    fn opus_5_5_default_payload_requests_default_fallbacks_with_matching_beta() {
        let model = models::anthropic::CLAUDE_OPUS_5_5;
        let provider = first_party_provider(model);
        let request = LLMRequest {
            model: model.to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        assert_eq!(payload["fallbacks"], json!("default"));
        let betas = split_betas(provider.beta_header_for_request(&request, &payload, false, None));
        assert!(betas.iter().any(|beta| beta == "server-side-fallback-2026-07-01"), "{betas:?}");
        assert!(!betas.iter().any(|beta| beta == "server-side-fallback-2026-06-01"), "{betas:?}");
        // The default-form beta already grants the fallback-credit fields.
        assert!(!betas.iter().any(|beta| beta == "fallback-credit-2026-07-01"), "{betas:?}");
    }

    #[test]
    fn configured_fallback_list_uses_list_form_and_credit_betas() {
        let model = models::anthropic::CLAUDE_OPUS_5;
        let mut provider = first_party_provider(model);
        provider.anthropic_config.fallbacks =
            vtcode_config::core::AnthropicFallbacks::Models(vec![vtcode_config::core::AnthropicFallbackTarget {
                model: "claude-opus-4-8".to_string(),
                max_tokens: None,
            }]);
        let request = LLMRequest {
            model: model.to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        assert_eq!(payload["fallbacks"][0]["model"], "claude-opus-4-8");
        let betas = split_betas(provider.beta_header_for_request(&request, &payload, false, None));
        assert!(betas.iter().any(|beta| beta == "server-side-fallback-2026-06-01"), "{betas:?}");
        assert!(!betas.iter().any(|beta| beta == "server-side-fallback-2026-07-01"), "{betas:?}");
        // The list form does not grant the credit fields, so the original
        // request carries the credit beta for a refusal to return a token.
        assert!(betas.iter().any(|beta| beta == "fallback-credit-2026-07-01"), "{betas:?}");
    }

    #[test]
    fn unprofiled_and_off_payloads_send_no_fallbacks_or_beta() {
        let unprofiled = first_party_provider("claude-3-5-haiku-latest");
        let request = LLMRequest {
            model: "claude-3-5-haiku-latest".to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };
        let payload = unprofiled.convert_to_anthropic_format(&request).expect("payload conversion");
        assert!(payload.get("fallbacks").is_none());
        let betas = split_betas(unprofiled.beta_header_for_request(&request, &payload, false, None));
        assert!(!betas.iter().any(|beta| beta.starts_with("server-side-fallback")), "{betas:?}");

        let model = models::anthropic::CLAUDE_OPUS_5_5;
        let mut off = first_party_provider(model);
        off.anthropic_config.fallbacks =
            vtcode_config::core::AnthropicFallbacks::Mode(vtcode_config::core::AnthropicFallbackMode::Off);
        let request = LLMRequest {
            model: model.to_string(),
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };
        let payload = off.convert_to_anthropic_format(&request).expect("payload conversion");
        assert!(payload.get("fallbacks").is_none());
        let betas = split_betas(off.beta_header_for_request(&request, &payload, false, None));
        assert!(!betas.iter().any(|beta| beta.starts_with("server-side-fallback")), "{betas:?}");
    }

    fn body_has(request: &wiremock::Request, key: &str, value: Option<&str>) -> bool {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
        match value {
            Some(expected) => body.get(key).and_then(serde_json::Value::as_str) == Some(expected),
            None => body.get(key).is_none(),
        }
    }

    /// An Opus 5.5 provider whose thinking config (`display: "summarized"`)
    /// is valid on the recommended Opus 4.8 as sent, so a refusal retry can
    /// match the refused request exactly and redeem the credit token.
    fn credit_retry_provider(server: &wiremock::MockServer) -> AnthropicProvider {
        let mut provider = AnthropicProvider::new_with_client(
            "test-key".to_string(),
            models::anthropic::CLAUDE_OPUS_5_5.to_string(),
            reqwest::Client::builder().no_proxy().build().expect("test client should build"),
            format!("{}/v1", server.uri()),
            vtcode_config::TimeoutsConfig::default(),
        );
        provider.anthropic_config.thinking_display = Some(vtcode_config::ThinkingDisplayMode::Summarized);
        provider
    }

    fn refusal_with_recommendation() -> serde_json::Value {
        json!({
            "content": [],
            "stop_reason": "refusal",
            "stop_details": {
                "type": "refusal",
                "category": "cyber",
                "explanation": "declined",
                "fallback_credit_token": "credit-1",
                "recommended_model": "claude-opus-4-8"
            }
        })
    }

    #[tokio::test]
    async fn generate_retries_refusal_once_on_recommended_model_with_credit_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| body_has(req, "model", Some("claude-opus-5-5")))
            .respond_with(ResponseTemplate::new(200).set_body_json(refusal_with_recommendation()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| {
                body_has(req, "model", Some("claude-opus-4-8"))
                    && body_has(req, "fallback_credit_token", Some("credit-1"))
                    && body_has(req, "fallbacks", None)
                    && req
                        .headers
                        .get("anthropic-beta")
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value.split(", ").any(|beta| beta == "fallback-credit-2026-07-01"))
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "retried answer"}],
                "stop_reason": "end_turn"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = credit_retry_provider(&server);
        let response = LLMProvider::generate(
            &provider,
            LLMRequest {
                model: models::anthropic::CLAUDE_OPUS_5_5.to_string(),
                messages: vec![Message::user("hello".to_string())].into(),
                ..Default::default()
            },
        )
        .await
        .expect("retried request should succeed");

        assert_eq!(response.content.as_deref(), Some("retried answer"));
        assert_eq!(response.model, "claude-opus-4-8");
        assert!(matches!(response.finish_reason, crate::provider::FinishReason::Stop));
    }

    #[tokio::test]
    async fn generate_resends_without_credit_token_when_token_is_rejected() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| body_has(req, "model", Some("claude-opus-5-5")))
            .respond_with(ResponseTemplate::new(200).set_body_json(refusal_with_recommendation()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| {
                body_has(req, "model", Some("claude-opus-4-8"))
                    && body_has(req, "fallback_credit_token", Some("credit-1"))
            })
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": "fallback_credit_token has expired"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| {
                body_has(req, "model", Some("claude-opus-4-8")) && body_has(req, "fallback_credit_token", None)
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "uncredited answer"}],
                "stop_reason": "end_turn"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = credit_retry_provider(&server);
        let response = LLMProvider::generate(
            &provider,
            LLMRequest {
                model: models::anthropic::CLAUDE_OPUS_5_5.to_string(),
                messages: vec![Message::user("hello".to_string())].into(),
                ..Default::default()
            },
        )
        .await
        .expect("uncredited retry should succeed");

        assert_eq!(response.content.as_deref(), Some("uncredited answer"));
    }

    #[tokio::test]
    async fn generate_retry_without_credit_token_when_thinking_must_be_rewritten() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
                body_has(req, "model", Some("claude-opus-5-5")) && body["thinking"]["display"] == "updates"
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(refusal_with_recommendation()))
            .expect(1)
            .mount(&server)
            .await;
        // Opus 4.8 rejects `display: "updates"`, so the retry's thinking
        // differs from the refused request and the token could never match:
        // one request, sent without the token.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
                body_has(req, "model", Some("claude-opus-4-8"))
                    && body_has(req, "fallback_credit_token", None)
                    && body["thinking"].get("display").is_none()
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "uncredited answer"}],
                "stop_reason": "end_turn"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new_with_client(
            "test-key".to_string(),
            models::anthropic::CLAUDE_OPUS_5_5.to_string(),
            reqwest::Client::builder().no_proxy().build().expect("test client should build"),
            format!("{}/v1", server.uri()),
            vtcode_config::TimeoutsConfig::default(),
        );
        let response = LLMProvider::generate(
            &provider,
            LLMRequest {
                model: models::anthropic::CLAUDE_OPUS_5_5.to_string(),
                messages: vec![Message::user("hello".to_string())].into(),
                ..Default::default()
            },
        )
        .await
        .expect("uncredited retry should succeed");

        assert_eq!(response.content.as_deref(), Some("uncredited answer"));
        assert_eq!(response.model, "claude-opus-4-8");
    }

    #[tokio::test]
    async fn generate_keeps_original_refusal_when_retry_fails() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| body_has(req, "model", Some("claude-opus-5-5")))
            .respond_with(ResponseTemplate::new(200).set_body_json(refusal_with_recommendation()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| body_has(req, "model", Some("claude-opus-4-8")))
            .respond_with(ResponseTemplate::new(529).set_body_json(json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new_with_client(
            "test-key".to_string(),
            models::anthropic::CLAUDE_OPUS_5_5.to_string(),
            reqwest::Client::builder().no_proxy().build().expect("test client should build"),
            format!("{}/v1", server.uri()),
            vtcode_config::TimeoutsConfig::default(),
        );
        let response = LLMProvider::generate(
            &provider,
            LLMRequest {
                model: models::anthropic::CLAUDE_OPUS_5_5.to_string(),
                messages: vec![Message::user("hello".to_string())].into(),
                ..Default::default()
            },
        )
        .await
        .expect("original refusal is returned");

        assert!(matches!(response.finish_reason, crate::provider::FinishReason::Refusal));
        assert_eq!(response.model, models::anthropic::CLAUDE_OPUS_5_5);
    }

    #[tokio::test]
    async fn stream_replaces_retryable_refusal_with_retry_stream() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let refused = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-5-5\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_sequence\":null,\"stop_details\":{\"type\":\"refusal\",\"category\":\"cyber\",\"fallback_credit_token\":\"credit-1\",\"recommended_model\":\"claude-opus-4-8\"}},\"usage\":{\"output_tokens\":0}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let retried = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-8\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"retried\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| body_has(req, "model", Some("claude-opus-5-5")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(refused, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(|req: &wiremock::Request| {
                body_has(req, "model", Some("claude-opus-4-8"))
                    && body_has(req, "fallback_credit_token", Some("credit-1"))
            })
            .respond_with(ResponseTemplate::new(200).set_body_raw(retried, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = credit_retry_provider(&server);
        let mut stream = LLMProvider::stream(
            &provider,
            LLMRequest {
                model: models::anthropic::CLAUDE_OPUS_5_5.to_string(),
                messages: vec![Message::user("hello".to_string())].into(),
                ..Default::default()
            },
        )
        .await
        .expect("stream request should succeed");

        let mut completed = Vec::new();
        while let Some(event) = stream.next().await {
            if let LLMStreamEvent::Completed { response } = event.expect("stream event") {
                completed.push(*response);
            }
        }

        assert_eq!(completed.len(), 1, "the refused attempt is replaced, not surfaced");
        assert_eq!(completed[0].content.as_deref(), Some("retried"));
        assert!(matches!(completed[0].finish_reason, crate::provider::FinishReason::Stop));
    }

    #[test]
    fn convert_to_anthropic_format_falls_back_to_provider_default_model() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            messages: vec![Message::user("hello".to_string())].into(),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");

        assert_eq!(payload["model"], models::CLAUDE_SONNET_5);
    }

    #[test]
    fn beta_header_includes_advanced_tool_use_for_tool_search_requests() {
        let provider = AnthropicProvider::with_model("test-key".to_string(), models::CLAUDE_SONNET_5.to_string());
        let request = LLMRequest {
            model: models::CLAUDE_SONNET_5.to_string(),
            messages: vec![Message::user("find the deployment tool".to_string())].into(),
            tools: Some(std::sync::Arc::new(vec![ToolDefinition::tool_search(
                crate::provider::ToolSearchAlgorithm::Regex,
            )])),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        let beta_header = provider
            .beta_header_for_request(&request, &payload, true, None)
            .expect("beta header");

        assert!(beta_header.contains("advanced-tool-use-2025-11-20"));
    }

    #[test]
    fn code_execution_beta_name_uses_tool_revision() {
        assert_eq!(code_execution_beta_name("code_execution_20250825").as_deref(), Some("code-execution-2025-08-25"));
        assert_eq!(code_execution_beta_name("code_execution_20250522").as_deref(), Some("code-execution-2025-05-22"));
        assert!(code_execution_beta_name("code_execution_latest").is_none());
    }

    #[test]
    fn turn_scoped_system_notice_is_emitted_after_tool_result() {
        let model = models::anthropic::CLAUDE_FABLE_5;
        let provider = AnthropicProvider::with_model("test-key".to_string(), model.to_string());
        let request = LLMRequest {
            model: model.to_string(),
            messages: vec![
                Message::assistant_with_tools(
                    String::new(),
                    vec![crate::provider::ToolCall::function(
                        "toolu_1".to_string(),
                        "exec_command".to_string(),
                        "{}".to_string(),
                    )],
                ),
                Message::tool_response("toolu_1".to_string(), "exit 1".to_string()),
                Message::turn_scoped_system("Only you see that command's output".to_string()),
            ]
            .into(),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        assert_eq!(payload["messages"][1]["role"], "user");
        assert_eq!(payload["messages"][2]["role"], "system");
        assert_eq!(payload["messages"][2]["clear_at"], "next_user_message");
        assert_eq!(payload["messages"][2]["content"][0]["type"], "text");
        assert_eq!(payload["messages"][2]["content"][0]["text"], "Only you see that command's output");
        assert!(
            !payload
                .get("system")
                .is_some_and(|system| { system.to_string().contains("Only you see that command's output") })
        );

        let beta_header = provider
            .beta_header_for_request(&request, &payload, false, None)
            .expect("turn-scoped beta header");
        assert_eq!(
            beta_header
                .split(", ")
                .filter(|beta| *beta == headers::MID_CONVERSATION_SYSTEM_CLEAR_AT_BETA)
                .count(),
            1
        );
    }

    #[test]
    fn turn_scoped_system_notice_is_promoted_for_unsupported_sonnet() {
        let model = models::CLAUDE_SONNET_5;
        let provider = AnthropicProvider::with_model("test-key".to_string(), model.to_string());
        let request = LLMRequest {
            model: model.to_string(),
            messages: vec![
                Message::user("continue".to_string()),
                Message::turn_scoped_system("do not leak output".to_string()),
            ]
            .into(),
            ..Default::default()
        };

        let payload = provider.convert_to_anthropic_format(&request).expect("payload conversion");
        assert!(
            payload["messages"]
                .as_array()
                .is_some_and(|messages| { messages.iter().all(|message| message.get("clear_at").is_none()) })
        );
        assert!(
            payload
                .get("system")
                .is_some_and(|system| { system.to_string().contains("do not leak output") })
        );
        let beta_header = provider.beta_header_for_request(&request, &payload, false, None);
        assert!(!beta_header.is_some_and(|header| {
            header
                .split(", ")
                .any(|beta| beta == headers::MID_CONVERSATION_SYSTEM_CLEAR_AT_BETA)
        }));
    }

    #[test]
    fn turn_scoped_system_capability_matches_supported_model_families() {
        assert!(capabilities::supports_turn_scoped_system_messages(models::anthropic::CLAUDE_FABLE_5, ""));
        assert!(capabilities::supports_turn_scoped_system_messages("claude-opus-4-8", ""));
        assert!(capabilities::supports_turn_scoped_system_messages(models::anthropic::CLAUDE_OPUS_5, ""));
        assert!(!capabilities::supports_turn_scoped_system_messages(models::anthropic::CLAUDE_SONNET_5, ""));
    }
}
