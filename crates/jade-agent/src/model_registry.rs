//! Single source of truth for built-in providers, model capabilities,
//! pricing cliffs, reasoning-content echo, routing tier scores, and retirements.

use crate::agent::context::catalog::ModelCap;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct FamilySpec {
    pub frag: &'static str,
    pub window: u64,
    pub max_output: Option<u64>,
    pub pricing_cliff: Option<u64>,
}

const FAMILY_TABLE: &[FamilySpec] = &[
    // Anthropic Claude 5 / Fable / Mythos (1M context, 128K output)
    FamilySpec {
        frag: "claude-fable",
        window: 1_000_000,
        max_output: Some(128_000),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "claude-mythos",
        window: 1_000_000,
        max_output: Some(128_000),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "claude-opus-5",
        window: 1_000_000,
        max_output: Some(128_000),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "claude-sonnet-5",
        window: 1_000_000,
        max_output: Some(128_000),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "claude-haiku-5",
        window: 1_000_000,
        max_output: Some(128_000),
        pricing_cliff: None,
    },
    // Anthropic Claude 3.x / 4.x (200K context)
    FamilySpec {
        frag: "claude",
        window: 200_000,
        max_output: Some(64_000),
        pricing_cliff: None,
    },
    // Google Gemini 4 / 2.5 / 3 (1M unified context)
    FamilySpec {
        frag: "gemini-4",
        window: 1_048_576,
        max_output: Some(65_536),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "gemini",
        window: 1_048_576,
        max_output: Some(65_536),
        pricing_cliff: None,
    },
    // OpenAI GPT-6 (512K astra / 384K sol harness, no 272K cliff) & GPT-5.x (272K cliff)
    FamilySpec {
        frag: "gpt-6",
        window: 1_050_000,
        max_output: Some(128_000),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "gpt-5.6",
        window: 1_050_000,
        max_output: Some(128_000),
        pricing_cliff: Some(272_000),
    },
    FamilySpec {
        frag: "gpt-5.5",
        window: 1_050_000,
        max_output: Some(128_000),
        pricing_cliff: Some(272_000),
    },
    FamilySpec {
        frag: "gpt-5",
        window: 400_000,
        max_output: Some(128_000),
        pricing_cliff: Some(272_000),
    },
    FamilySpec {
        frag: "gpt-4.1",
        window: 1_047_576,
        max_output: Some(32_768),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "gpt-4o",
        window: 128_000,
        max_output: Some(16_384),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "gpt-4-turbo",
        window: 128_000,
        max_output: Some(4_096),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "o1",
        window: 200_000,
        max_output: Some(100_000),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "o3",
        window: 200_000,
        max_output: Some(100_000),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "o4",
        window: 200_000,
        max_output: Some(100_000),
        pricing_cliff: None,
    },
    // DeepSeek V4 (1M) vs legacy R1/Chat
    FamilySpec {
        frag: "deepseek-v4",
        window: 1_000_000,
        max_output: Some(384_000),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "deepseek-flash",
        window: 1_000_000,
        max_output: Some(384_000),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "deepseek-reasoner",
        window: 65_536,
        max_output: Some(32_768),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "deepseek",
        window: 131_072,
        max_output: Some(32_768),
        pricing_cliff: None,
    },
    // Qwen4 / Qwen3.8
    FamilySpec {
        frag: "qwen4",
        window: 1_000_000,
        max_output: Some(262_144),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "qwen3.8-max",
        window: 1_000_000,
        max_output: Some(131_072),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "qwen3.8-flash-next",
        window: 262_144,
        max_output: Some(131_072),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "qwen3.8-flash",
        window: 1_000_000,
        max_output: Some(131_072),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "qwen3.8-2.4t",
        window: 262_144,
        max_output: Some(131_072),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "qwen",
        window: 131_072,
        max_output: Some(32_768),
        pricing_cliff: None,
    },
    // xAI Grok
    FamilySpec {
        frag: "grok-4.7",
        window: 500_000,
        max_output: None,
        pricing_cliff: Some(200_000),
    },
    FamilySpec {
        frag: "grok-4.6",
        window: 500_000,
        max_output: None,
        pricing_cliff: Some(200_000),
    },
    FamilySpec {
        frag: "grok-4.5",
        window: 500_000,
        max_output: None,
        pricing_cliff: Some(200_000),
    },
    FamilySpec {
        frag: "grok-4.3",
        window: 1_000_000,
        max_output: None,
        pricing_cliff: Some(200_000),
    },
    FamilySpec {
        frag: "grok-4.20",
        window: 1_000_000,
        max_output: None,
        pricing_cliff: Some(200_000),
    },
    FamilySpec {
        frag: "grok-build",
        window: 256_000,
        max_output: None,
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "grok",
        window: 131_072,
        max_output: None,
        pricing_cliff: None,
    },
    // Moonshot Kimi & Z.ai GLM
    FamilySpec {
        frag: "kimi-k3",
        window: 1_048_576,
        max_output: Some(128_000),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "kimi",
        window: 131_072,
        max_output: None,
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "glm-5",
        window: 1_048_576,
        max_output: Some(131_072),
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "glm",
        window: 131_072,
        max_output: None,
        pricing_cliff: None,
    },
    FamilySpec {
        frag: "llama",
        window: 128_000,
        max_output: None,
        pricing_cliff: None,
    },
];

/// Match `frag` on token boundaries so e.g. `o3` matches `o3-mini` / `openai/o3`
/// without matching `my-pro3-model`, and `o1` never matches `video1`.
pub fn matches_family_fragment(model_id: &str, frag: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    let bytes = id.as_bytes();
    let flen = frag.len();
    let ends_with_digit = frag.as_bytes().last().is_some_and(u8::is_ascii_digit);
    let mut pos = 0;
    while let Some(rel) = id[pos..].find(frag) {
        let start = pos + rel;
        let end = start + flen;
        let left_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let right_ok = if end == bytes.len() || !bytes[end].is_ascii_alphanumeric() {
            true
        } else {
            !ends_with_digit && bytes[end].is_ascii_digit()
        };
        if left_ok && right_ok {
            return true;
        }
        pos = start + 1;
    }
    false
}

/// Built-in 2026 model family capabilities. Longest matching fragment wins.
pub fn builtin_model_cap(model_id: &str) -> Option<ModelCap> {
    FAMILY_TABLE
        .iter()
        .filter(|r| matches_family_fragment(model_id, r.frag))
        .max_by_key(|r| r.frag.len())
        .map(|r| ModelCap {
            context_window: r.window,
            max_output: r.max_output,
            pricing_cliff: r.pricing_cliff,
            source: "builtin".into(),
            updated_ms: 0,
        })
}

/// Whether `model_id` echoes `reasoning_content` on multi-step OpenAI-chat turns.
pub fn echoes_reasoning_content(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    let name = id.rsplit(['/', ':']).next().unwrap_or(&id);
    name.contains("deepseek")
        || name.contains("reasoner")
        || name.starts_with("qwq")
        || name.contains("-r1")
        || name.contains("kimi")
        || name.contains("glm")
        || name.contains("qwen3")
}

/// Models retired upstream whose APIs reject requests outright.
/// Note: `"deepseek-chat"` and `"deepseek-reasoner"` are active aliases and remain allowed.
pub fn is_retired_model(provider_id: &str, model_id: &str) -> bool {
    provider_id == "deepseek" && model_id == "deepseek-v4.1-flash"
}

#[derive(Clone, Copy)]
pub(crate) enum RoutingTier {
    Fast,
    Flagship,
}

pub(crate) fn tier_score(model_id: &str, tier: RoutingTier) -> i32 {
    let id = model_id.to_ascii_lowercase();
    match tier {
        RoutingTier::Fast => {
            if id.contains("haiku") {
                100
            } else if id.contains("flash") {
                95
            } else if id.contains("nano") {
                90
            } else if id.contains("mini") {
                88
            } else if id.contains("lite") || id.contains("fast") || id.contains("small") {
                80
            } else {
                0
            }
        }
        RoutingTier::Flagship => {
            if id.contains("mythos") || id.contains("fable") || id.contains("opus") {
                100
            } else if id.contains("gpt-6") || id.contains("gpt-5.6") || id.contains("gpt-5.5") {
                96
            } else if id.contains("gpt-5") && !id.contains("mini") && !id.contains("nano") {
                90
            } else if id.contains("gemini-4") || id.contains("pro") {
                88
            } else if id.contains("sonnet") {
                82
            } else {
                0
            }
        }
    }
}

pub(crate) struct ProviderDefaults {
    pub base_url_env: Option<&'static str>,
    pub default_model: &'static str,
}

pub(crate) fn provider_defaults(provider_id: &str) -> Option<ProviderDefaults> {
    match provider_id {
        "openai" => Some(ProviderDefaults {
            base_url_env: Some("OPENAI_BASE_URL"),
            default_model: "gpt-5.4",
        }),
        "anthropic" => Some(ProviderDefaults {
            base_url_env: Some("ANTHROPIC_BASE_URL"),
            default_model: "claude-sonnet-4-5",
        }),
        "deepseek" => Some(ProviderDefaults {
            base_url_env: Some("DEEPSEEK_BASE_URL"),
            default_model: "deepseek-v3.2",
        }),
        "google" => Some(ProviderDefaults {
            base_url_env: Some("GEMINI_BASE_URL"),
            default_model: "gemini-2.5-pro",
        }),
        "xai" => Some(ProviderDefaults {
            base_url_env: Some("XAI_BASE_URL"),
            default_model: "grok-4",
        }),
        "qwen" => Some(ProviderDefaults {
            base_url_env: Some("DASHSCOPE_BASE_URL"),
            default_model: "qwen-plus",
        }),
        "zhipu" => Some(ProviderDefaults {
            base_url_env: Some("ZHIPU_BASE_URL"),
            default_model: "glm-4-plus",
        }),
        "moonshot" => Some(ProviderDefaults {
            base_url_env: Some("MOONSHOT_BASE_URL"),
            default_model: "moonshot-v1-auto",
        }),
        "minimax" => Some(ProviderDefaults {
            base_url_env: Some("MINIMAX_BASE_URL"),
            default_model: "MiniMax-Text-01",
        }),
        "volcengine" => Some(ProviderDefaults {
            base_url_env: Some("ARK_BASE_URL"),
            default_model: "doubao-seed-1-6",
        }),
        "mistral" => Some(ProviderDefaults {
            base_url_env: Some("MISTRAL_BASE_URL"),
            default_model: "mistral-large-latest",
        }),
        "groq" => Some(ProviderDefaults {
            base_url_env: Some("GROQ_BASE_URL"),
            default_model: "llama-3.3-70b-versatile",
        }),
        "openrouter" => Some(ProviderDefaults {
            base_url_env: Some("OPENROUTER_BASE_URL"),
            default_model: "anthropic/claude-sonnet-4.5",
        }),
        "siliconflow" => Some(ProviderDefaults {
            base_url_env: Some("SILICONFLOW_BASE_URL"),
            default_model: "deepseek-ai/DeepSeek-V3",
        }),
        "aihubmix" => Some(ProviderDefaults {
            base_url_env: Some("AIHUBMIX_BASE_URL"),
            default_model: "gpt-4o",
        }),
        "ai302" => Some(ProviderDefaults {
            base_url_env: Some("AI302_BASE_URL"),
            default_model: "gpt-4o",
        }),
        "opencode-zen" => Some(ProviderDefaults {
            base_url_env: Some("OPENCODE_BASE_URL"),
            default_model: "claude-sonnet-4-5",
        }),
        "opencode-go" => Some(ProviderDefaults {
            base_url_env: Some("OPENCODE_GO_BASE_URL"),
            default_model: "claude-sonnet-4-5",
        }),
        _ => None,
    }
}

pub(crate) fn env_base_url_var(provider_id: &str) -> Option<&'static str> {
    provider_defaults(provider_id).and_then(|d| d.base_url_env)
}

pub fn default_env_model(provider_id: &str) -> &'static str {
    provider_defaults(provider_id)
        .map(|d| d.default_model)
        .unwrap_or("")
}

pub(crate) fn infer_provider_id_from_model<'a, T>(
    model_id: &str,
    providers: &'a BTreeMap<String, T>,
) -> Option<&'a str> {
    let lower = model_id.to_ascii_lowercase();
    if lower.starts_with("claude") {
        if providers.contains_key("anthropic") {
            Some("anthropic")
        } else if providers.contains_key("openrouter") {
            Some("openrouter")
        } else {
            None
        }
    } else if lower.starts_with("gpt-")
        || matches_family_fragment(&lower, "o1")
        || matches_family_fragment(&lower, "o3")
        || matches_family_fragment(&lower, "o4")
    {
        providers.contains_key("openai").then_some("openai")
    } else if lower.starts_with("gemini") {
        providers.contains_key("google").then_some("google")
    } else if lower.starts_with("grok") {
        providers.contains_key("xai").then_some("xai")
    } else if lower.starts_with("qwen") {
        providers.contains_key("qwen").then_some("qwen")
    } else if lower.starts_with("glm") {
        providers.contains_key("zhipu").then_some("zhipu")
    } else if lower.starts_with("kimi") || lower.starts_with("moonshot") {
        providers.contains_key("moonshot").then_some("moonshot")
    } else if lower.starts_with("minimax") {
        providers.contains_key("minimax").then_some("minimax")
    } else if lower.starts_with("doubao") {
        providers.contains_key("volcengine").then_some("volcengine")
    } else if lower.starts_with("mistral") {
        providers.contains_key("mistral").then_some("mistral")
    } else if lower.starts_with("deepseek") {
        providers.contains_key("deepseek").then_some("deepseek")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_matching_avoids_false_o1_and_o3_hits() {
        assert!(builtin_model_cap("o3-mini").is_some());
        assert!(builtin_model_cap("openai/o3").is_some());
        assert!(builtin_model_cap("o1-preview").is_some());
        assert!(builtin_model_cap("my-pro3-model").is_none());
        assert!(builtin_model_cap("video1").is_none());
        assert_eq!(
            builtin_model_cap("gpt-6-astra").unwrap().pricing_cliff,
            None
        );
    }

    #[test]
    fn deepseek_chat_and_reasoner_are_not_retired() {
        assert!(!is_retired_model("deepseek", "deepseek-chat"));
        assert!(!is_retired_model("deepseek", "deepseek-reasoner"));
        assert!(is_retired_model("deepseek", "deepseek-v4.1-flash"));
    }
}
