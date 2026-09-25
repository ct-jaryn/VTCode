//! Bounded client-side retry for refusals whose server-side fallback could not run.
//!
//! With server-side fallbacks, a final `refusal` normally means the whole
//! fallback chain declined, and retrying the same prompt is pointless. The
//! exception is a refusal whose `stop_details.recommended_model` is set: the
//! server wanted to fall back but that attempt could not run (rate limited or
//! overloaded), so a direct retry on the recommended model may succeed. That
//! retry echoes `stop_details.fallback_credit_token` so the refused attempt's
//! prompt-cache cost is credited, and must keep every prompt-shaping field
//! (system, messages, tools, tool_choice, thinking) unchanged.
//!
//! VT Code retries at most once, and only when the refusal arrived before any
//! assistant text or tool call and carries no prefill claim: a claim requires
//! echoing the refused response's raw content as an assistant message, which
//! the universal response does not keep, and a streamed partial answer has
//! already been shown. `reasoning_extraction` declines are never retried.

use serde_json::Value;

use crate::provider::{LLMError, LLMRequest, LLMResponse};
use crate::providers::anthropic_types::ThinkingConfig;

use super::request_builder::rewrite_thinking_for_model;

/// Refusal category that server-side fallbacks never retry.
const REASONING_EXTRACTION_CATEGORY: &str = "reasoning_extraction";
const CREDIT_TOKEN_FIELD: &str = "fallback_credit_token";
const PREFILL_CLAIM_FIELD: &str = "fallback_has_prefill_claim";

/// A single retry on the model the refused response recommended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefusalRetryPlan {
    pub(crate) model: String,
    pub(crate) credit_token: String,
}

impl RefusalRetryPlan {
    /// Returns a retry plan when `response` is a pre-output refusal whose
    /// server-side fallback could not run and that carries a credit token.
    pub(crate) fn for_response(response: &LLMResponse, original_model: &str) -> Option<Self> {
        if !matches!(response.finish_reason, crate::provider::FinishReason::Refusal) {
            return None;
        }
        let has_text = response.content.as_deref().is_some_and(|text| !text.trim().is_empty());
        let has_tool_calls = response.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty());
        if has_text || has_tool_calls {
            return None;
        }

        let detail = stop_details(response)?;
        let category = detail.get("category").and_then(Value::as_str).unwrap_or_default();
        if category == REASONING_EXTRACTION_CATEGORY {
            return None;
        }
        // A prefill claim means the retry must append the refused content
        // (for example a thinking-only partial) as an assistant message.
        // That content is not kept, and a retry without it would not match.
        if detail.get(PREFILL_CLAIM_FIELD).and_then(Value::as_bool) == Some(true) {
            return None;
        }
        let model = detail.get("recommended_model").and_then(Value::as_str)?.trim();
        let credit_token = detail.get(CREDIT_TOKEN_FIELD).and_then(Value::as_str)?.trim();
        if model.is_empty() || credit_token.is_empty() || model == original_model {
            return None;
        }

        Some(Self {
            model: model.to_string(),
            credit_token: credit_token.to_string(),
        })
    }

    /// The logical request for the retry, used for header selection
    /// (`fallback-credit` beta, per-model betas) and the response model.
    pub(crate) fn retry_request(&self, request: &LLMRequest, with_token: bool) -> LLMRequest {
        let mut retry = request.clone();
        retry.model = self.model.clone();
        retry.fallbacks = None;
        retry.fallback_credit_token = with_token.then(|| self.credit_token.clone());
        retry
    }

    /// The retry body: the refused request's exact payload retargeted at the
    /// recommended model, without `fallbacks`, and with the credit token when
    /// it can still be redeemed.
    ///
    /// `thinking` is rewritten only when the recommended model would reject
    /// the original config outright (for example `display: "updates"`); the
    /// unchanged config would fail anyway. A credit retry must match the
    /// refused request's `thinking` exactly, so a rewritten body is sent
    /// without the token from the start (credit forfeited) instead of
    /// spending a request on a guaranteed mismatch.
    pub(crate) fn retry_body(&self, original: &Value, default_model: &str) -> RefusalRetryBody {
        let mut body = original.clone();
        let Some(object) = body.as_object_mut() else {
            return RefusalRetryBody { body, with_token: false };
        };
        object.insert("model".to_string(), Value::String(self.model.clone()));
        object.remove("fallbacks");

        let effort = object
            .get("output_config")
            .and_then(|config| config.get("effort"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let rewritten = object
            .get("thinking")
            .and_then(|thinking| serde_json::from_value::<ThinkingConfig>(thinking.clone()).ok())
            .and_then(|thinking| rewrite_thinking_for_model(&thinking, &self.model, default_model, effort.as_deref()))
            .and_then(|thinking| serde_json::to_value(thinking).ok());
        let with_token = rewritten.is_none();
        if let Some(thinking) = rewritten {
            object.insert("thinking".to_string(), thinking);
        } else {
            object.insert(CREDIT_TOKEN_FIELD.to_string(), Value::String(self.credit_token.clone()));
        }
        RefusalRetryBody { body, with_token }
    }
}

/// A refusal retry payload and whether it redeems the credit token.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RefusalRetryBody {
    pub(crate) body: Value,
    pub(crate) with_token: bool,
}

/// Removes the credit token from a retry body (credit forfeited).
pub(crate) fn strip_credit_token(body: &mut Value) {
    if let Some(object) = body.as_object_mut() {
        object.remove(CREDIT_TOKEN_FIELD);
    }
}

/// Whether `err` is a 400 that names `fallback_credit_token` (expired,
/// mismatched or otherwise unusable token). Such a retry is resent once
/// without the token.
pub(crate) fn rejects_credit_token(err: &LLMError) -> bool {
    let LLMError::InvalidRequest { message, metadata } = err else {
        return false;
    };
    let status_is_400 = metadata.as_ref().is_some_and(|metadata| metadata.status == Some(400));
    let names_token = message.contains(CREDIT_TOKEN_FIELD)
        || metadata
            .as_ref()
            .and_then(|metadata| metadata.message.as_deref())
            .is_some_and(|diagnostic| diagnostic.contains(CREDIT_TOKEN_FIELD));
    status_is_400 && names_token
}

fn stop_details(response: &LLMResponse) -> Option<Value> {
    response
        .reasoning_details
        .as_ref()?
        .iter()
        .filter_map(|detail| serde_json::from_str::<Value>(detail).ok())
        .find(|detail| detail.get("type").and_then(Value::as_str) == Some("stop_details"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FinishReason;
    use serde_json::json;

    fn refusal(detail: Value) -> LLMResponse {
        LLMResponse {
            finish_reason: FinishReason::Refusal,
            reasoning_details: Some(vec![detail.to_string()]),
            ..Default::default()
        }
    }

    fn detail(category: &str, recommended: Option<&str>, token: Option<&str>) -> Value {
        let mut detail = json!({ "type": "stop_details", "category": category });
        if let Some(model) = recommended {
            detail["recommended_model"] = json!(model);
        }
        if let Some(token) = token {
            detail["fallback_credit_token"] = json!(token);
        }
        detail
    }

    #[test]
    fn plans_retry_only_for_pre_output_refusals_with_recommended_model_and_token() {
        let response = refusal(detail("cyber", Some("claude-opus-4-8"), Some("tok")));
        assert_eq!(
            RefusalRetryPlan::for_response(&response, "claude-opus-5-5"),
            Some(RefusalRetryPlan {
                model: "claude-opus-4-8".to_string(),
                credit_token: "tok".to_string(),
            })
        );

        for response in [
            refusal(detail("cyber", None, Some("tok"))),
            refusal(detail("cyber", Some("claude-opus-4-8"), None)),
            refusal(detail("reasoning_extraction", Some("claude-opus-4-8"), Some("tok"))),
            refusal(detail("cyber", Some("claude-opus-5-5"), Some("tok"))),
        ] {
            assert_eq!(RefusalRetryPlan::for_response(&response, "claude-opus-5-5"), None);
        }

        let mut partial = refusal(detail("cyber", Some("claude-opus-4-8"), Some("tok")));
        partial.content = Some("Here is".to_string());
        assert_eq!(RefusalRetryPlan::for_response(&partial, "claude-opus-5-5"), None);

        let mut claimed = detail("cyber", Some("claude-opus-4-8"), Some("tok"));
        claimed["fallback_has_prefill_claim"] = json!(true);
        assert_eq!(RefusalRetryPlan::for_response(&refusal(claimed), "claude-opus-5-5"), None);

        let mut unclaimed = detail("cyber", Some("claude-opus-4-8"), Some("tok"));
        unclaimed["fallback_has_prefill_claim"] = json!(false);
        assert!(RefusalRetryPlan::for_response(&refusal(unclaimed), "claude-opus-5-5").is_some());

        let mut stopped = refusal(detail("cyber", Some("claude-opus-4-8"), Some("tok")));
        stopped.finish_reason = FinishReason::Stop;
        assert_eq!(RefusalRetryPlan::for_response(&stopped, "claude-opus-5-5"), None);
    }

    #[test]
    fn retry_body_keeps_prompt_fields_and_swaps_model_token_and_fallbacks() {
        let plan = RefusalRetryPlan {
            model: "claude-opus-4-8".to_string(),
            credit_token: "tok".to_string(),
        };
        let original = json!({
            "model": "claude-opus-5-5",
            "system": "sys",
            "messages": [{ "role": "user", "content": "hi" }],
            "thinking": { "type": "adaptive", "display": "summarized" },
            "fallbacks": "default",
            "max_tokens": 64000,
            "stream": true
        });

        let RefusalRetryBody { mut body, with_token } = plan.retry_body(&original, "claude-opus-5-5");
        assert!(with_token);
        assert_eq!(body["model"], "claude-opus-4-8");
        assert_eq!(body["fallback_credit_token"], "tok");
        assert!(body.get("fallbacks").is_none());
        assert_eq!(body["system"], original["system"]);
        assert_eq!(body["messages"], original["messages"]);
        assert_eq!(body["thinking"], original["thinking"]);
        assert_eq!(body["max_tokens"], 64000);
        assert_eq!(body["stream"], true);

        strip_credit_token(&mut body);
        assert!(body.get("fallback_credit_token").is_none());
    }

    #[test]
    fn retry_body_with_rewritten_thinking_forfeits_the_credit_token() {
        let plan = RefusalRetryPlan {
            model: "claude-opus-4-8".to_string(),
            credit_token: "tok".to_string(),
        };
        let original = json!({
            "model": "claude-opus-5-5",
            "messages": [{ "role": "user", "content": "hi" }],
            "thinking": { "type": "adaptive", "display": "updates" },
            "fallbacks": "default",
            "max_tokens": 64000
        });

        let RefusalRetryBody { body, with_token } = plan.retry_body(&original, "claude-opus-5-5");
        // `display: "updates"` is rejected outside Opus 5.5 / Fable 5.x, and
        // the rewritten `thinking` can never match the credit token.
        assert_eq!(body["thinking"], json!({ "type": "adaptive" }));
        assert!(!with_token);
        assert!(body.get("fallback_credit_token").is_none());
        assert_eq!(body["model"], "claude-opus-4-8");
        assert!(body.get("fallbacks").is_none());
    }

    #[test]
    fn only_400s_naming_the_credit_token_are_resent_without_it() {
        let named = LLMError::InvalidRequest {
            message: "Anthropic: invalid request: fallback_credit_token has expired".to_string(),
            metadata: Some(crate::provider::LLMErrorMetadata::new(
                "Anthropic",
                Some(400),
                Some("invalid_request".to_string()),
                None,
                None,
                None,
                None,
            )),
        };
        assert!(rejects_credit_token(&named));

        let other = LLMError::InvalidRequest {
            message: "Anthropic: invalid request: max_tokens too large".to_string(),
            metadata: Some(crate::provider::LLMErrorMetadata::new(
                "Anthropic",
                Some(400),
                None,
                None,
                None,
                None,
                None,
            )),
        };
        assert!(!rejects_credit_token(&other));
    }
}
