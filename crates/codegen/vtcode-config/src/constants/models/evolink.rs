// Evolink models (OpenAI-compatible multi-model gateway)
// https://docs.evolink.ai/llms.txt
//
// Evolink is an aggregator that exposes many upstream models behind a single
// gateway. GPT/DeepSeek/Doubao models use the OpenAI Chat Completions format
// at `/v1/chat/completions`; Claude models use the Anthropic Messages format
// at `/v1/messages`. Bare model names collide with VT Code's first-class
// providers, so curated `ModelId` catalog entries are namespaced with an
// `evolink/` prefix; the provider strips that prefix before sending upstream.

// --- OpenAI-compatible models (Chat Completions API) ---
pub const GPT_5_6: &str = "gpt-5.6";
pub const GPT_5_6_SOL: &str = "gpt-5.6-sol";
pub const GEMINI_3_1_PRO: &str = "gemini-3.7-flash";
pub const GEMINI_3_7_FLASH: &str = "gemini-3.7-flash";
pub const MINIMAX_M3: &str = "MiniMax-M3";

// --- Anthropic-compatible models (Messages API) ---
pub const CLAUDE_SONNET_5: &str = "claude-sonnet-5";
pub const CLAUDE_OPUS_5: &str = "claude-opus-5";
pub const CLAUDE_OPUS_5_5: &str = "claude-opus-5-5";

pub const DEFAULT_MODEL: &str = GPT_5_6;

/// Curated models VT Code exposes in config flows and `ModelId` metadata.
pub const SUPPORTED_MODELS: &[&str] = &[
    GPT_5_6,
    GPT_5_6_SOL,
    GEMINI_3_1_PRO,
    GEMINI_3_7_FLASH,
    MINIMAX_M3,
    CLAUDE_SONNET_5,
    CLAUDE_OPUS_5,
    CLAUDE_OPUS_5_5,
    CLAUDE_OPUS_5,
    CLAUDE_SONNET_5,
];

/// Models that use the Anthropic Messages API instead of OpenAI Chat Completions.
const ANTHROPIC_FORMAT_MODELS: &[&str] = &[
    CLAUDE_SONNET_5,
    CLAUDE_OPUS_5,
    CLAUDE_OPUS_5_5,
    CLAUDE_OPUS_5,
    CLAUDE_SONNET_5,
];

/// Models that emit reasoning traces / accept `reasoning_effort`.
pub const REASONING_MODELS: &[&str] = &[];

/// Returns `true` if the model should use the Anthropic Messages API format.
pub fn is_anthropic_format(model: &str) -> bool {
    ANTHROPIC_FORMAT_MODELS.contains(&model)
}
