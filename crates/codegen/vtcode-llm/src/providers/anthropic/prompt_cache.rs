//! Prompt caching configuration for Anthropic Claude API
//!
//! Implements Anthropic's prompt caching feature with configurable TTLs:
//! - "5m" (5 minutes) for dynamic content like messages
//! - "1h" (1 hour) for stable content like tools and system prompts

use vtcode_config::core::AnthropicPromptCacheSettings;

pub fn get_cache_ttl_for_seconds(ttl_seconds: u64) -> &'static str {
    if ttl_seconds >= 3600 { "1h" } else { "5m" }
}

/// Effective tools/system TTL. `prefer_extended_ttl` upgrades a 5m breakpoint to 1h.
pub fn get_tools_cache_ttl(settings: &AnthropicPromptCacheSettings) -> &'static str {
    if settings.prefer_extended_ttl {
        return "1h";
    }
    get_cache_ttl_for_seconds(settings.tools_ttl_seconds)
}

/// Effective messages TTL (same promotion rules as tools).
pub fn get_messages_cache_ttl(settings: &AnthropicPromptCacheSettings) -> &'static str {
    if settings.prefer_extended_ttl {
        return "1h";
    }
    get_cache_ttl_for_seconds(settings.messages_ttl_seconds)
}

/// TTL for budget-continuation / long-lived profile requests.
pub fn get_profile_cache_ttl(settings: &AnthropicPromptCacheSettings) -> &'static str {
    match settings.extended_ttl_seconds {
        Some(seconds) => get_cache_ttl_for_seconds(seconds),
        // Profiles are expected to outlive the 5m window by default.
        None => "1h",
    }
}

pub fn requires_extended_ttl_beta(settings: &AnthropicPromptCacheSettings) -> bool {
    // Beta is required when an emitted breakpoint TTL is 1h. `prefer_extended_ttl`
    // promotes tools/messages, so it is covered by the effective TTL check.
    get_tools_cache_ttl(settings) == "1h" || get_messages_cache_ttl(settings) == "1h"
}
