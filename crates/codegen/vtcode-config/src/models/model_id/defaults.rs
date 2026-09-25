use crate::models::Provider;

use super::ModelId;

impl ModelId {
    /// Get recommended fallback models in order of preference
    pub fn fallback_models() -> Vec<ModelId> {
        vec![
            ModelId::ClaudeSonnet5,
            ModelId::ClaudeOpus5,
            ModelId::GPT56Sol,
            ModelId::Gemini38Flash,
            ModelId::OpenAIGptOss20b,
            ModelId::ZaiGlm53,
        ]
    }

    /// Get the default model for general use
    pub(crate) fn default_model() -> Self {
        ModelId::ClaudeSonnet5
    }

    /// Get the default orchestrator model (more capable)
    pub fn default_orchestrator() -> Self {
        ModelId::ClaudeSonnet5
    }

    /// Get provider-specific defaults for orchestrator
    pub fn default_orchestrator_for_provider(provider: Provider) -> Self {
        match provider {
            Provider::Gemini => ModelId::Gemini38Flash,
            Provider::OpenAI => ModelId::GPT56Sol,
            Provider::Anthropic => ModelId::ClaudeOpus55,
            Provider::Copilot => ModelId::CopilotAuto,
            Provider::Minimax => ModelId::MinimaxM3,
            Provider::MiMo => ModelId::MiMoV26Pro,
            Provider::Mistral => ModelId::MistralLarge3,
            Provider::DeepSeek => ModelId::DeepSeekFlash,
            Provider::Meta => ModelId::MetaMuseSpark13,
            Provider::HuggingFace => ModelId::HuggingFaceOpenAIGptOss120b,
            Provider::Moonshot => ModelId::MoonshotKimiK3,
            Provider::OpenRouter => ModelId::OpenRouterXiaomiMimoV26Pro,
            Provider::Ollama => ModelId::OllamaGptOss20b,
            Provider::OllamaCloud => ModelId::OllamaGptOss120bCloud,
            Provider::LmStudio => ModelId::GPT56Sol,
            Provider::LlamaCpp => ModelId::LlamaCppGptOss20b,
            Provider::ZAI => ModelId::ZaiGlm53,
            Provider::OpenCodeZen => ModelId::ClaudeSonnet5,
            Provider::OpenCodeGo => ModelId::OpenCodeGoMinimaxM3,
            Provider::Qwen => ModelId::DeepSeekFlash,
            Provider::StepFun => ModelId::StepFun37Flash,
            Provider::Evolink => ModelId::EvolinkGemini31Pro,
            Provider::XAI => ModelId::XaiGrok47,
            Provider::NVIDIA => ModelId::NvidiaNemotron3Ultra550bA55b,
            Provider::MergeGateway => ModelId::MergeGatewayDefaultRouting,
            Provider::Vercel => ModelId::VercelAnthropicClaudeSonnet5,
            _ => ModelId::ClaudeSonnet5,
        }
    }

    /// Get provider-specific defaults for single agent
    pub fn default_single_for_provider(provider: Provider) -> Self {
        match provider {
            Provider::Gemini => ModelId::Gemini38Flash,
            Provider::OpenAI => ModelId::GPT56Sol,
            Provider::Anthropic => ModelId::ClaudeSonnet5,
            Provider::Copilot => ModelId::CopilotAuto,
            Provider::Minimax => ModelId::MinimaxM3,
            Provider::MiMo => ModelId::MiMoV26Pro,
            Provider::Mistral => ModelId::MistralLarge3,
            Provider::DeepSeek => ModelId::DeepSeekFlash,
            Provider::Meta => ModelId::MetaMuseSpark13,
            Provider::HuggingFace => ModelId::HuggingFaceOpenAIGptOss120b,
            Provider::Moonshot => ModelId::MoonshotKimiK3,
            Provider::OpenRouter => ModelId::OpenRouterXiaomiMimoV26Pro,
            Provider::Ollama => ModelId::OllamaGptOss20b,
            Provider::OllamaCloud => ModelId::OllamaGptOss120bCloud,
            Provider::LmStudio => ModelId::GPT56Sol,
            Provider::LlamaCpp => ModelId::LlamaCppGptOss20b,
            Provider::ZAI => ModelId::ZaiGlm53,
            Provider::OpenCodeZen => ModelId::ClaudeSonnet5,
            Provider::OpenCodeGo => ModelId::OpenCodeGoMinimaxM3,
            Provider::Qwen => ModelId::DeepSeekFlash,
            Provider::StepFun => ModelId::StepFun37Flash,
            Provider::Evolink => ModelId::EvolinkGemini31Pro,
            Provider::XAI => ModelId::XaiGrok47,
            Provider::NVIDIA => ModelId::NvidiaNemotron3Ultra550bA55b,
            Provider::MergeGateway => ModelId::MergeGatewayDefaultRouting,
            Provider::Vercel => ModelId::VercelAnthropicClaudeSonnet5,
            _ => ModelId::ClaudeSonnet5,
        }
    }
}
