use std::str::FromStr;

use crate::models::ModelParseError;

use super::ModelId;

impl FromStr for ModelId {
    type Err = ModelParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use crate::constants::models;
        let trimmed = s.trim();

        if trimmed == models::zai::GLM_5_3 {
            return Ok(ModelId::ZaiGlm53);
        }

        if trimmed == models::GPT_5_CODEX {
            return Ok(ModelId::GPT56Sol);
        }

        if trimmed == "gpt-5.4" {
            return Ok(ModelId::GPT56Sol);
        }

        if let Some(opencode_model) = trimmed
            .strip_prefix("opencode/")
            .or_else(|| trimmed.strip_prefix("opencode-zen/"))
        {
            return match opencode_model {
                "gpt-5.4" | "gpt-5.6-sol" => Ok(ModelId::GPT56Sol),
                _ => Err(ModelParseError::InvalidModel(trimmed.to_string())),
            };
        }

        if let Some(opencode_model) = trimmed.strip_prefix("opencode-go/") {
            return match opencode_model {
                m if m == models::opencode_go::GLM_5_3 => Ok(ModelId::OpenCodeGoGlm53),
                m if m == models::opencode_go::GPT_5_6_LUNA => Ok(ModelId::OpenCodeGoGpt56Luna),
                m if m == models::opencode_go::KIMI_K3 => Ok(ModelId::OpenCodeGoKimiK3),
                m if m == models::opencode_go::MINIMAX_M3 => Ok(ModelId::OpenCodeGoMinimaxM3),
                _ => Err(ModelParseError::InvalidModel(trimmed.to_string())),
            };
        }

        if let Some(model) = Self::parse_openrouter_model(trimmed) {
            return Ok(model);
        }

        if let Some(model) = Self::parse_table(trimmed) {
            return Ok(model);
        }

        match trimmed {
            // OpenRouter models without generated metadata
            "moonshotai/kimi-k3" => Ok(ModelId::OpenRouterMoonshotaiKimiK3),
            _ => {
                if let Some(model) = Self::parse_openrouter_model(s) {
                    Ok(model)
                } else {
                    Err(ModelParseError::InvalidModel(s.to_string()))
                }
            }
        }
    }
}
