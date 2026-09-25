// OpenCode Go models (low-cost subscription)
// https://opencode.ai/docs/go/
pub const DEFAULT_MODEL: &str = GLM_5_3;

pub const GROK_4_5: &str = "grok-4.5";
pub const GLM_5_3: &str = "glm-5.3";
pub const GPT_5_6_LUNA: &str = "gpt-5.6-luna";
pub const KIMI_K3: &str = "kimi-k3";
pub const MINIMAX_M3: &str = "minimax-m3";

pub const MESSAGES_API_MODELS: &[&str] = &[MINIMAX_M3];
pub const CHAT_COMPLETIONS_MODELS: &[&str] = &[GROK_4_5, GLM_5_3, GPT_5_6_LUNA, KIMI_K3];

// Curated models VT Code currently exposes in config flows and ModelId metadata.
pub(crate) const CONFIGURED_MODELS: &[&str] = &[GROK_4_5, GLM_5_3, GPT_5_6_LUNA, KIMI_K3, MINIMAX_M3];

pub const SUPPORTED_MODELS: &[&str] = CONFIGURED_MODELS;
pub const REASONING_MODELS: &[&str] = &[];
