use super::super::errors::is_flex_service_tier_unsupported;
use super::super::responses_api::{build_standard_responses_payload, parse_responses_payload};
use super::OpenAIProvider;
use super::generation::payload_uses_flex_service_tier;
use crate::error_display;
use crate::provider::{LLMError, LLMNormalizedStream, LLMRequest, LLMResponse, NormalizedStreamEvent};
use crate::providers::shared::{ResponsesNormalizedStreamOptions, ResponsesNormalizedStreamProcessor};
use futures::{SinkExt, StreamExt};
use hashbrown::HashMap;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::OwnedMutexGuard;
use tokio::time::{Instant, timeout_at};
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

type ResponsesSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
const WEBSOCKET_ACTIVE_RESPONSE_ERROR_PREFIX: &str = "Conversation already has an active response in progress:";
const OPENAI_BETA_RESPONSES_WEBSOCKET_V2: &str = "responses=v2";
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
const PREVIOUS_RESPONSE_NOT_FOUND_CODE: &str = "previous_response_not_found";
const WEBSOCKET_AUTH_RETRY_STATUSES: [&str; 2] = ["401", "403"];

// Retained custom WebSocket transport while VTCode migrates the optional path
// to Rig. Rig 0.40 exposes `client.responses_websocket()` behind its
// `websocket` feature, including warm-up requests, but it does not yet cover
// VTCode's custom auth refresh and retry policy. Keep this path separate from
// normal HTTP/SSE request shaping: it may use `previous_response_id` only as
// connection-local state, and every websocket request still sends `store=false`.
fn is_websocket_active_response_error(err: &LLMError) -> bool {
    let message = match err {
        LLMError::Provider { message, .. } | LLMError::Network { message, .. } => message,
        LLMError::Authentication { .. } | LLMError::RateLimit { .. } | LLMError::InvalidRequest { .. } => return false,
    };

    message.contains(WEBSOCKET_ACTIVE_RESPONSE_ERROR_PREFIX)
}

fn is_websocket_connection_limit_error(err: &LLMError) -> bool {
    let message = match err {
        LLMError::Provider { message, .. } | LLMError::Network { message, .. } => message,
        LLMError::Authentication { .. } | LLMError::RateLimit { .. } | LLMError::InvalidRequest { .. } => return false,
    };

    message.contains(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE)
}

pub(super) fn is_websocket_previous_response_not_found_error(err: &LLMError) -> bool {
    let message = match err {
        LLMError::Provider { message, .. } | LLMError::Network { message, .. } => message,
        LLMError::Authentication { .. } | LLMError::RateLimit { .. } | LLMError::InvalidRequest { .. } => return false,
    };

    message.contains(PREVIOUS_RESPONSE_NOT_FOUND_CODE)
}

fn is_websocket_reconnect_error(err: &LLMError) -> bool {
    matches!(err, LLMError::Network { .. })
}

fn is_websocket_auth_retryable(error: &tokio_tungstenite::tungstenite::Error) -> bool {
    let message = error.to_string();
    WEBSOCKET_AUTH_RETRY_STATUSES.iter().any(|status| message.contains(status))
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpenAIResponsesWebSocketContinuationCache {
    response_id: String,
    full_input: Vec<Value>,
    model: String,
    instructions: Option<String>,
    tools: Option<Value>,
}

impl OpenAIResponsesWebSocketContinuationCache {
    fn from_response(
        response_json: &Value,
        request: &LLMRequest,
        prepared: &PreparedWebSocketEvent,
        fallback_model: &str,
    ) -> Option<Self> {
        let response_id = response_json
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())?
            .to_string();

        let model = if request.model.trim().is_empty() {
            fallback_model.to_string()
        } else {
            request.model.clone()
        };
        let instructions = response_json
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| prepared.event.get("instructions").and_then(Value::as_str).map(str::to_owned));
        let tools = prepared.event.get("tools").cloned();
        let mut full_input = canonical_history(&prepared.full_input);
        if prepared.event.get("generate") != Some(&Value::Bool(false)) {
            full_input.extend(completed_replay_output(response_json, &model)?);
        }
        Some(Self {
            response_id,
            full_input,
            model,
            instructions,
            tools,
        })
    }

    fn can_continue_from(&self, payload: &Value) -> bool {
        if self.response_id.is_empty() {
            return false;
        }

        let Some(current_model) = payload.get("model").and_then(Value::as_str) else {
            return false;
        };
        if self.model != current_model {
            return false;
        }

        let current_instructions = payload.get("instructions").and_then(Value::as_str).map(str::to_owned);
        if self.instructions != current_instructions {
            return false;
        }

        let current_tools = payload.get("tools").cloned();
        if self.tools != current_tools {
            return false;
        }

        let Some(current_input) = payload.get("input").and_then(Value::as_array) else {
            return false;
        };
        input_is_incremental(self.full_input.as_slice(), current_input.as_slice())
    }
}

#[derive(Debug)]
pub(crate) struct OpenAIResponsesWebSocketSession {
    socket: Box<ResponsesSocket>,
}

impl OpenAIResponsesWebSocketSession {
    fn new(socket: ResponsesSocket) -> Self {
        Self { socket: Box::new(socket) }
    }
}

#[derive(Debug)]
struct PreparedWebSocketEvent {
    event: Value,
    full_input: Vec<Value>,
    used_previous_response_id: bool,
}

// A response owns the socket outside its reusable slot. Dropping a future or
// stream closes that socket; only a validated completion returns it to the slot.
struct WebSocketLease {
    slot: OwnedMutexGuard<Option<OpenAIResponsesWebSocketSession>>,
    session: Option<OpenAIResponsesWebSocketSession>,
    cache: Arc<Mutex<Option<OpenAIResponsesWebSocketContinuationCache>>>,
    continuation: Option<OpenAIResponsesWebSocketContinuationCache>,
    tier_cache: Arc<Mutex<HashMap<String, bool>>>,
    model: String,
    tier_requested: bool,
    deadline: Instant,
}

impl WebSocketLease {
    fn session(&mut self) -> Result<&mut OpenAIResponsesWebSocketSession, LLMError> {
        self.session
            .as_mut()
            .ok_or_else(|| format_provider_error("OpenAI WebSocket session missing".to_string()))
    }

    async fn send(&mut self, event: &Value) -> Result<(), LLMError> {
        let deadline = self.deadline;
        timeout_at(deadline, self.session()?.socket.send(Message::Text(event.to_string().into())))
            .await
            .map_err(|_elapsed| websocket_deadline_error())?
            .map_err(|err| format_network_error(format!("Failed to send OpenAI WebSocket payload: {err}")))
    }

    async fn read(&mut self) -> Result<Value, LLMError> {
        let deadline = self.deadline;
        let event = timeout_at(deadline, read_websocket_event(self.session()?))
            .await
            .map_err(|_elapsed| websocket_deadline_error())??;
        let policy = crate::providers::shared::response_stream_event_policy(&event)
            .map_err(|message| format_provider_error(message.to_string()))?;
        if policy == crate::providers::shared::ResponsesStreamEventPolicy::Unsupported {
            return Err(format_provider_error("Unsupported OpenAI WebSocket event".to_string()));
        }
        let event_type = event.get("type").and_then(Value::as_str);
        if matches!(event_type, Some("error" | "response.failed" | "response.incomplete")) {
            let error = event.get("response").unwrap_or(&event);
            let status = event
                .get("status")
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok())
                .and_then(|status| reqwest::StatusCode::from_u16(status).ok())
                .unwrap_or(reqwest::StatusCode::BAD_REQUEST);
            if self.tier_requested && is_flex_service_tier_unsupported(status, &error.to_string()) {
                self.tier_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(self.model.clone(), true);
                tracing::warn!(model = %self.model, "OpenAI WebSocket rejected service tier; using tier-less HTTP");
            }
            let error = error.get("error").unwrap_or(error);
            let code = error.get("code").and_then(Value::as_str).unwrap_or("unknown_error");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("OpenAI WebSocket response failed or incomplete");
            return Err(format_provider_error(format!("{code}: {message}")));
        }
        if event_type == Some("response.completed") {
            let response = event.get("response").filter(|value| value.is_object()).ok_or_else(|| {
                format_provider_error("OpenAI WebSocket completed event missing response".to_string())
            })?;
            if response
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status != "completed")
            {
                return Err(format_provider_error("OpenAI WebSocket completion has unsuccessful status".to_string()));
            }
        }
        Ok(event)
    }

    async fn read_response(&mut self) -> Result<Value, LLMError> {
        loop {
            let event = self.read().await?;
            if event.get("type").and_then(Value::as_str) == Some("response.completed") {
                return event
                    .get("response")
                    .cloned()
                    .ok_or_else(|| format_provider_error("OpenAI WebSocket completion missing response".to_string()));
            }
        }
    }

    fn complete(mut self, continuation: Option<OpenAIResponsesWebSocketContinuationCache>) {
        *self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = continuation;
        *self.slot = self.session.take();
    }
}

fn websocket_deadline_error() -> LLMError {
    format_network_error("OpenAI WebSocket request deadline exceeded".to_string())
}

fn completed_websocket_stream(
    lease: WebSocketLease,
    continuation: Option<OpenAIResponsesWebSocketContinuationCache>,
    events: Vec<NormalizedStreamEvent>,
) -> LLMNormalizedStream {
    Box::pin(async_stream::try_stream! {
        for event in events {
            if matches!(event, NormalizedStreamEvent::Done { .. }) {
                // Usage can precede Done. Dropping before completion delivery
                // must still invalidate this lease, including during startup.
                lease.complete(continuation);
                yield event;
                break;
            }
            yield event;
        }
    })
}

impl OpenAIProvider {
    async fn websocket_lease(
        &self,
        request: &LLMRequest,
        payload: &Value,
        deadline: Instant,
    ) -> Result<WebSocketLease, LLMError> {
        let mut slot = timeout_at(deadline, Arc::clone(&self.websocket_session).lock_owned())
            .await
            .map_err(|_elapsed| websocket_deadline_error())?;
        // Taking both before the first await makes cancellation invalidate state,
        // including cancellation during authentication or connection startup.
        let session = slot.take();
        let continuation = self
            .websocket_continuation_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mut lease = WebSocketLease {
            slot,
            session,
            cache: Arc::clone(&self.websocket_continuation_cache),
            continuation,
            tier_cache: Arc::clone(&self.service_tier_unsupported_cache),
            model: request.model.clone(),
            tier_requested: payload_uses_flex_service_tier(payload),
            deadline,
        };
        if lease.session.is_none() {
            lease.continuation = None;
        }
        timeout_at(deadline, Box::pin(self.ensure_websocket_session(&mut lease.session, request)))
            .await
            .map_err(|_elapsed| websocket_deadline_error())??;
        Ok(lease)
    }

    pub(super) async fn stream_via_responses_websocket(
        &self,
        request: &LLMRequest,
        legacy_startup: bool,
    ) -> Result<LLMNormalizedStream, LLMError> {
        let payload = self.convert_to_openai_responses_format(request)?;
        let payload = self.maybe_strip_flex_service_tier(&request.model, &payload);
        let deadline = Instant::now() + self.websocket_streaming_ceiling;
        let mut lease = self.websocket_lease(request, &payload, deadline).await?;
        // Another response may have rejected the tier while this request waited
        // for the lane. Apply that cache before sending either event.
        let payload = self.maybe_strip_flex_service_tier(&request.model, &payload);
        lease.tier_requested = payload_uses_flex_service_tier(&payload);
        if lease.continuation.is_none() {
            let warmup = prepare_websocket_event(&payload, None, true)?;
            lease.send(&warmup.event).await?;
            let response = lease.read_response().await?;
            lease.continuation =
                OpenAIResponsesWebSocketContinuationCache::from_response(&response, request, &warmup, &self.model);
        }
        let prepared = prepare_websocket_event(&payload, lease.continuation.as_ref(), false)?;
        lease.send(&prepared.event).await?;
        let model = request.model.clone();
        let parse_model = model.clone();
        let include_metrics = self.prompt_cache_enabled && self.prompt_cache_settings.surface_metrics;
        let mut processor = ResponsesNormalizedStreamProcessor::new(
            ResponsesNormalizedStreamOptions {
                provider_name: "OpenAI",
                model,
                emit_reasoning: Self::model_supports_reasoning_summaries(&request.model),
                include_cached_prompt_metrics: include_metrics,
            },
            move |value| {
                let response = parse_responses_payload(value, parse_model.clone(), include_metrics)?;
                Ok(Self::normalize_reasoning_output(&parse_model, response))
            },
        );
        let request = request.clone();
        let fallback_model = self.model.clone();
        // Consume lifecycle/control frames here so failures before any deliverable
        // event can fall back to HTTP. No background task outlives stream drop.
        let mut first_events;
        let mut completed = None;
        loop {
            let event = lease.read().await?;
            first_events = processor.handle_payload_data(&event.to_string())?;
            if processor.is_done() {
                completed = event.get("response").cloned();
                break;
            }
            if legacy_startup {
                // Legacy callers only receive tool calls in their completion.
                first_events.retain(|event| {
                    !matches!(
                        event,
                        NormalizedStreamEvent::ToolCallStart { .. } | NormalizedStreamEvent::ToolCallDelta { .. }
                    )
                });
            }
            if !first_events.is_empty() {
                break;
            }
        }
        // Validate completion-only startup eagerly, without polling a stream
        // that would release its lease before the caller receives Done.
        if let Some(response) = completed {
            let final_events = processor.finish()?;
            let continuation = OpenAIResponsesWebSocketContinuationCache::from_response(
                &response,
                &request,
                &prepared,
                &fallback_model,
            );
            return Ok(completed_websocket_stream(lease, continuation, final_events));
        }
        Ok(Box::pin(async_stream::try_stream! {
            for event in first_events { yield event; }
            let response = loop {
                let event = lease.read().await?;
                for event in processor.handle_payload_data(&event.to_string())? { yield event; }
                if processor.is_done() {
                    break event.get("response").cloned().ok_or_else(|| format_provider_error("OpenAI WebSocket stream ended without completion".to_string()))?;
                }
            };
            let final_events = processor.finish()?;
            let continuation = OpenAIResponsesWebSocketContinuationCache::from_response(&response, &request, &prepared, &fallback_model);
            let mut completion = completed_websocket_stream(lease, continuation, final_events);
            while let Some(event) = completion.next().await {
                yield event?;
            }
        }))
    }

    pub(super) async fn generate_via_responses_websocket(&self, request: &LLMRequest) -> Result<LLMResponse, LLMError> {
        let payload = self.convert_to_openai_responses_format(request)?;
        let payload = self.maybe_strip_flex_service_tier(&request.model, &payload);
        let deadline = Instant::now() + self.websocket_streaming_ceiling;
        let mut retried_active_response = false;
        let mut retried_reconnect = false;
        let mut retried_new_chain = false;
        loop {
            let mut lease = self.websocket_lease(request, &payload, deadline).await?;
            let payload = self.maybe_strip_flex_service_tier(&request.model, &payload);
            lease.tier_requested = payload_uses_flex_service_tier(&payload);
            if lease.continuation.is_none() {
                let warmup = prepare_websocket_event(&payload, None, true)?;
                let result = async {
                    lease.send(&warmup.event).await?;
                    lease.read_response().await
                }
                .await;
                match result {
                    Ok(response) => {
                        lease.continuation = OpenAIResponsesWebSocketContinuationCache::from_response(
                            &response,
                            request,
                            &warmup,
                            &self.model,
                        )
                    }
                    Err(err) => {
                        if !retried_reconnect
                            && (is_websocket_reconnect_error(&err) || is_websocket_connection_limit_error(&err))
                        {
                            retried_reconnect = true;
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
            let prepared = prepare_websocket_event(&payload, lease.continuation.as_ref(), false)?;
            let result = async {
                lease.send(&prepared.event).await?;
                lease.read_response().await
            }
            .await;
            match result {
                Ok(response_json) => {
                    let parsed = self.parse_openai_responses_response(response_json.clone(), request.model.clone())?;
                    let continuation = OpenAIResponsesWebSocketContinuationCache::from_response(
                        &response_json,
                        request,
                        &prepared,
                        &self.model,
                    );
                    lease.complete(continuation);
                    return Ok(parsed);
                }
                Err(err) => {
                    if !retried_active_response && is_websocket_active_response_error(&err) {
                        retried_active_response = true;
                        continue;
                    }
                    if !retried_new_chain
                        && prepared.used_previous_response_id
                        && is_websocket_previous_response_not_found_error(&err)
                    {
                        retried_new_chain = true;
                        continue;
                    }
                    if !retried_reconnect
                        && (is_websocket_reconnect_error(&err) || is_websocket_connection_limit_error(&err))
                    {
                        retried_reconnect = true;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    async fn ensure_websocket_session<'a>(
        &self,
        session_guard: &'a mut Option<OpenAIResponsesWebSocketSession>,
        request: &LLMRequest,
    ) -> Result<&'a mut OpenAIResponsesWebSocketSession, LLMError> {
        if session_guard.is_none() {
            let connection_deadline = Instant::now() + Duration::from_secs(30);
            let ws_url = responses_websocket_url(&self.base_url)?;
            let build_request = |api_key: &str| -> Result<_, LLMError> {
                let mut ws_request = ws_url
                    .clone()
                    .into_client_request()
                    .map_err(|err| format_provider_error(format!("Invalid OpenAI WebSocket request: {err}")))?;

                ws_request.headers_mut().insert(
                    "Authorization",
                    HeaderValue::from_str(&format!("Bearer {api_key}"))
                        .map_err(|err| format_provider_error(format!("Invalid OpenAI authorization header: {err}")))?,
                );
                ws_request
                    .headers_mut()
                    .insert("OpenAI-Beta", HeaderValue::from_static(OPENAI_BETA_RESPONSES_WEBSOCKET_V2));
                if let Some(metadata) = &request.metadata
                    && let Ok(metadata_str) = serde_json::to_string(metadata)
                    && let Ok(value) = HeaderValue::from_str(&metadata_str)
                {
                    ws_request.headers_mut().insert("X-Turn-Metadata", value);
                }

                Ok(ws_request)
            };

            let api_key = timeout_at(connection_deadline, self.current_api_key())
                .await
                .map_err(|_elapsed| format_network_error("OpenAI WebSocket connection timeout".to_string()))??;
            let ws_request = build_request(&api_key)?;
            let socket = match timeout_at(connection_deadline, Box::pin(connect_async(ws_request)))
                .await
                .map_err(|_elapsed| websocket_deadline_error())?
            {
                Ok((socket, _)) => socket,
                Err(err) if self.uses_chatgpt_auth() && is_websocket_auth_retryable(&err) => {
                    let retry_api_key = timeout_at(connection_deadline, self.refresh_api_key_for_retry())
                        .await
                        .map_err(|_elapsed| {
                            format_network_error("OpenAI WebSocket connection timeout".to_string())
                        })??;
                    let retry_request = build_request(&retry_api_key)?;
                    let (socket, _) = timeout_at(connection_deadline, Box::pin(connect_async(retry_request)))
                        .await
                        .map_err(|_elapsed| websocket_deadline_error())?
                        .map_err(|retry_err| {
                            format_network_error(format!("Failed to connect OpenAI WebSocket: {retry_err}"))
                        })?;
                    socket
                }
                Err(err) => {
                    return Err(format_network_error(format!("Failed to connect OpenAI WebSocket: {err}")));
                }
            };
            *session_guard = Some(OpenAIResponsesWebSocketSession::new(socket));
        }

        session_guard
            .as_mut()
            .ok_or_else(|| format_provider_error("OpenAI WebSocket session unexpectedly missing".to_string()))
    }

    #[cfg(test)]
    fn websocket_continuation_snapshot(&self) -> Option<OpenAIResponsesWebSocketContinuationCache> {
        self.websocket_continuation_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    #[cfg(test)]
    fn update_websocket_continuation(
        &self,
        response_json: &Value,
        request: &LLMRequest,
        prepared: &PreparedWebSocketEvent,
        fallback_model: &str,
    ) {
        let cache =
            OpenAIResponsesWebSocketContinuationCache::from_response(response_json, request, prepared, fallback_model);
        *self
            .websocket_continuation_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = cache;
    }
}

fn input_is_incremental(last_input: &[Value], current_input: &[Value]) -> bool {
    if current_input.len() < last_input.len() {
        return false;
    }
    canonical_history(current_input).starts_with(&canonical_history(last_input))
}

fn apply_generate_mode(request_obj: &mut Map<String, Value>, warmup: bool) {
    if warmup {
        request_obj.insert("generate".to_string(), Value::Bool(false));
    } else {
        request_obj.remove("generate");
    }
}

fn prepare_websocket_event(
    payload: &Value,
    continuation: Option<&OpenAIResponsesWebSocketContinuationCache>,
    warmup: bool,
) -> Result<PreparedWebSocketEvent, LLMError> {
    let mut request_obj = payload
        .as_object()
        .cloned()
        .ok_or_else(|| format_provider_error("Invalid Responses payload".to_string()))?;

    request_obj.remove("stream");
    request_obj.remove("background");
    request_obj.insert("store".to_string(), Value::Bool(false));
    apply_generate_mode(&mut request_obj, warmup);

    let full_input = request_obj
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| format_provider_error("Responses payload missing input".to_string()))?;

    let mut used_previous_response_id = false;
    if !warmup
        && let Some(continuation) = continuation
        && continuation.can_continue_from(&Value::Object(request_obj.clone()))
    {
        if !continuation.response_id.is_empty() {
            request_obj.insert("previous_response_id".to_string(), Value::String(continuation.response_id.clone()));
            let incremental = full_input[continuation.full_input.len()..].to_vec();
            request_obj.insert("input".to_string(), Value::Array(incremental));
            used_previous_response_id = true;
        }
    } else {
        request_obj.remove("previous_response_id");
    }

    let event = Value::Object(
        std::iter::once(("type".to_string(), Value::String("response.create".to_string())))
            .chain(request_obj)
            .collect(),
    );

    Ok(PreparedWebSocketEvent { event, full_input, used_previous_response_id })
}

async fn read_websocket_event(session: &mut OpenAIResponsesWebSocketSession) -> Result<Value, LLMError> {
    while let Some(message) = session.socket.next().await {
        let message = message.map_err(|err| format_network_error(format!("OpenAI WebSocket receive failed: {err}")))?;
        match message {
            Message::Text(text) => {
                let event: Value = serde_json::from_str(text.as_ref())
                    .map_err(|err| format_provider_error(format!("Invalid OpenAI WebSocket event JSON: {err}")))?;
                if event.get("type").and_then(Value::as_str).is_none() {
                    return Err(format_provider_error("OpenAI WebSocket event missing type".to_string()));
                }
                return Ok(event);
            }
            Message::Ping(payload) => {
                session
                    .socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|err| format_network_error(format!("Failed to reply to OpenAI WebSocket ping: {err}")))?;
            }
            Message::Pong(_) => {}
            Message::Close(_) => {
                return Err(format_network_error("OpenAI WebSocket connection closed before completion".to_string()));
            }
            _ => return Err(format_provider_error("Unexpected OpenAI WebSocket frame".to_string())),
        }
    }
    Err(format_network_error("OpenAI WebSocket stream ended without completion".to_string()))
}

fn canonical_history(input: &[Value]) -> Vec<Value> {
    fn strip_breakpoints(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.remove("prompt_cache_breakpoint");
                for value in object.values_mut() {
                    strip_breakpoints(value);
                }
            }
            Value::Array(array) => {
                for value in array {
                    strip_breakpoints(value);
                }
            }
            _ => {}
        }
    }
    let mut input = input.to_vec();
    for item in &mut input {
        strip_breakpoints(item);
    }
    input
}

// Compare output in the same replay shape the next request builder uses. Opaque
// or unrecoverable output starts a full-input chain rather than risking a match.
fn completed_replay_output(response: &Value, model: &str) -> Option<Vec<Value>> {
    let output = response.get("output").and_then(Value::as_array)?;
    if output.is_empty() {
        return None;
    }
    if output.iter().any(|item| item.get("phase").is_some()) {
        return None;
    }
    if output.iter().any(|item| {
        !matches!(
            item.get("type").and_then(Value::as_str),
            Some("message" | "reasoning" | "function_call" | "custom_tool_call")
        )
    }) {
        return None;
    }
    let response = parse_responses_payload(response.clone(), model.to_string(), false).ok()?;
    let response = OpenAIProvider::normalize_reasoning_output(model, response);
    let message = crate::provider::Message::assistant_with_tools_and_reasoning(
        response.content.unwrap_or_default(),
        response.tool_calls.unwrap_or_default(),
        response
            .reasoning_details
            .map(|details| {
                details
                    .into_iter()
                    .map(|detail| serde_json::from_str(&detail))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .ok()?,
    );
    let request = LLMRequest {
        model: model.to_string(),
        messages: vec![message].into(),
        ..Default::default()
    };
    let mut replay = build_standard_responses_payload(&request, true).ok()?.input;
    // The history builder supplies missing tool outputs for HTTP replay. They
    // are not part of this completion and must stay in the next turn's delta.
    replay.retain(|item| {
        !matches!(item.get("type").and_then(Value::as_str), Some("function_call_output" | "custom_tool_call_output"))
    });
    Some(canonical_history(&replay))
}

fn responses_websocket_url(base_url: &str) -> Result<String, LLMError> {
    let mut url = url::Url::parse(base_url)
        .map_err(|err| format_provider_error(format!("Invalid OpenAI base URL for WebSocket mode: {err}")))?;

    match url.scheme() {
        "https" => {
            let _ = url.set_scheme("wss");
        }
        "http" => {
            let _ = url.set_scheme("ws");
        }
        "wss" | "ws" => {}
        other => {
            return Err(format_provider_error(format!("Unsupported URL scheme for WebSocket mode: {other}")));
        }
    }

    if !url.path().ends_with("/responses") {
        let mut path = url.path().trim_end_matches('/').to_string();
        if path.is_empty() {
            path.push('/');
        }
        path.push_str("/responses");
        url.set_path(&path);
    }

    Ok(url.to_string())
}

#[cold]
fn format_provider_error(message: String) -> LLMError {
    LLMError::Provider {
        message: error_display::format_llm_error("OpenAI", &message),
        metadata: None,
    }
}

#[cold]
fn format_network_error(message: String) -> LLMError {
    LLMError::Network {
        message: error_display::format_llm_error("OpenAI", &message),
        metadata: None,
    }
}

#[cfg(test)]
mod tests;
