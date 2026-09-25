//! OpenAI Responses-API history/chain handling.
//!
//! Resolves the previous-response continuation state for providers that
//! support the Responses-API chaining protocol: trims the outgoing message
//! history to what the provider still needs given a live
//! `previous_response_id`, clears a stale chain when the provider signals
//! it, and records the chain after a successful response. Invariant: chain
//! state is only ever recorded/consulted for providers where
//! `records_responses_continuation_state`/`prepare_responses_continuation_request`
//! (from `vtcode_core::llm::provider`) report support -- other providers
//! always see their full, untrimmed history.

use std::borrow::Cow;
use std::sync::Arc;

use vtcode_core::llm::provider::{
    self as uni, prepare_responses_continuation_request, records_responses_continuation_state,
};

pub(super) fn update_previous_response_chain_after_success_shared(
    session_stats: &mut crate::agent::runloop::unified::state::SessionStats,
    provider_name: &str,
    provider_supports_responses_compaction: bool,
    active_model: &str,
    response_request_id: Option<&str>,
    messages: Arc<Vec<uni::Message>>,
) {
    if records_responses_continuation_state(provider_name, provider_supports_responses_compaction) {
        session_stats.set_previous_response_chain_shared(provider_name, active_model, response_request_id, messages);
    }
}

pub(super) fn prepare_responses_request_history<'a>(
    session_stats: &mut crate::agent::runloop::unified::state::SessionStats,
    provider_name: &str,
    provider_supports_responses_compaction: bool,
    active_model: &str,
    messages: &'a [uni::Message],
) -> (Cow<'a, [uni::Message]>, Option<String>) {
    let prepared = prepare_responses_continuation_request(
        provider_name,
        provider_supports_responses_compaction,
        messages,
        session_stats.previous_response_chain_for(provider_name, active_model),
    );
    if prepared.clear_stale_chain {
        session_stats.clear_previous_response_chain_for(provider_name, active_model);
    }

    (prepared.messages, prepared.previous_response_id)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vtcode_core::llm::provider as uni;

    use super::update_previous_response_chain_after_success_shared;

    #[test]
    fn openai_session_stats_does_not_record_previous_response_chain() {
        let mut session_stats = crate::agent::runloop::unified::state::SessionStats::default();
        let messages = vec![uni::Message::user("hello".to_string())];

        update_previous_response_chain_after_success_shared(
            &mut session_stats,
            "openai",
            true,
            "gpt-5.6-sol",
            Some("resp_123"),
            Arc::new(messages),
        );

        assert_eq!(session_stats.previous_response_chain_for("openai", "gpt-5.6-sol"), None);
    }
}
