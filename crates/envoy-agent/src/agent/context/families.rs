//! Built-in 2026 model family sizes. Longest matching fragment wins.
use super::catalog::ModelCap;

/// Built-in 2026-11 model family capability table. Longest fragment wins.
pub fn builtin_model_cap(model_id: &str) -> Option<ModelCap> {
    let id = model_id.to_ascii_lowercase();
    struct Rule {
        frag: &'static str,
        window: u64,
        max_out: Option<u64>,
        cliff: Option<u64>,
    }
    const TABLE: &[Rule] = &[
        // Anthropic Claude 5 / Fable / Mythos (1M context, 128K output)
        Rule {
            frag: "claude-fable",
            window: 1_000_000,
            max_out: Some(128_000),
            cliff: None,
        },
        Rule {
            frag: "claude-mythos",
            window: 1_000_000,
            max_out: Some(128_000),
            cliff: None,
        },
        Rule {
            frag: "claude-opus-5",
            window: 1_000_000,
            max_out: Some(128_000),
            cliff: None,
        },
        Rule {
            frag: "claude-sonnet-5",
            window: 1_000_000,
            max_out: Some(128_000),
            cliff: None,
        },
        Rule {
            frag: "claude-haiku-5",
            window: 1_000_000,
            max_out: Some(128_000),
            cliff: None,
        },
        // Anthropic Claude 3.x / 4.x (200K context)
        Rule {
            frag: "claude",
            window: 200_000,
            max_out: Some(64_000),
            cliff: None,
        },
        // Google Gemini 4 / 2.5 / 3 (1M unified context)
        Rule {
            frag: "gemini-4",
            window: 1_048_576,
            max_out: Some(65_536),
            cliff: None,
        },
        Rule {
            frag: "gemini",
            window: 1_048_576,
            max_out: Some(65_536),
            cliff: None,
        },
        // OpenAI GPT-6 / GPT-5.5+ (1.05M raw, 272K billing cliff)
        Rule {
            frag: "gpt-6",
            window: 1_050_000,
            max_out: Some(128_000),
            cliff: Some(272_000),
        },
        Rule {
            frag: "gpt-5.6",
            window: 1_050_000,
            max_out: Some(128_000),
            cliff: Some(272_000),
        },
        Rule {
            frag: "gpt-5.5",
            window: 1_050_000,
            max_out: Some(128_000),
            cliff: Some(272_000),
        },
        Rule {
            frag: "gpt-5",
            window: 400_000,
            max_out: Some(128_000),
            cliff: Some(272_000),
        },
        Rule {
            frag: "gpt-4.1",
            window: 1_047_576,
            max_out: Some(32_768),
            cliff: None,
        },
        Rule {
            frag: "gpt-4o",
            window: 128_000,
            max_out: Some(16_384),
            cliff: None,
        },
        Rule {
            frag: "gpt-4-turbo",
            window: 128_000,
            max_out: Some(4_096),
            cliff: None,
        },
        Rule {
            frag: "o1",
            window: 200_000,
            max_out: Some(100_000),
            cliff: None,
        },
        Rule {
            frag: "o3",
            window: 200_000,
            max_out: Some(100_000),
            cliff: None,
        },
        Rule {
            frag: "o4",
            window: 200_000,
            max_out: Some(100_000),
            cliff: None,
        },
        // DeepSeek V4 (1M) vs legacy R1/Chat
        Rule {
            frag: "deepseek-v4",
            window: 1_000_000,
            max_out: Some(384_000),
            cliff: None,
        },
        Rule {
            frag: "deepseek-flash",
            window: 1_000_000,
            max_out: Some(384_000),
            cliff: None,
        },
        Rule {
            frag: "deepseek-reasoner",
            window: 65_536,
            max_out: Some(32_768),
            cliff: None,
        },
        Rule {
            frag: "deepseek",
            window: 131_072,
            max_out: Some(32_768),
            cliff: None,
        },
        // Qwen4 / Qwen3.8
        Rule {
            frag: "qwen4",
            window: 1_000_000,
            max_out: Some(262_144),
            cliff: None,
        },
        Rule {
            frag: "qwen3.8-max",
            window: 1_000_000,
            max_out: Some(131_072),
            cliff: None,
        },
        Rule {
            frag: "qwen3.8-flash-next",
            window: 262_144,
            max_out: Some(131_072),
            cliff: None,
        },
        Rule {
            frag: "qwen3.8-flash",
            window: 1_000_000,
            max_out: Some(131_072),
            cliff: None,
        },
        Rule {
            frag: "qwen3.8-2.4t",
            window: 262_144,
            max_out: Some(131_072),
            cliff: None,
        },
        Rule {
            frag: "qwen",
            window: 131_072,
            max_out: Some(32_768),
            cliff: None,
        },
        // xAI Grok
        Rule {
            frag: "grok-4.7",
            window: 500_000,
            max_out: None,
            cliff: Some(200_000),
        },
        Rule {
            frag: "grok-4.6",
            window: 500_000,
            max_out: None,
            cliff: Some(200_000),
        },
        Rule {
            frag: "grok-4.5",
            window: 500_000,
            max_out: None,
            cliff: Some(200_000),
        },
        Rule {
            frag: "grok-4.3",
            window: 1_000_000,
            max_out: None,
            cliff: Some(200_000),
        },
        Rule {
            frag: "grok-4.20",
            window: 1_000_000,
            max_out: None,
            cliff: Some(200_000),
        },
        Rule {
            frag: "grok-build",
            window: 256_000,
            max_out: None,
            cliff: None,
        },
        Rule {
            frag: "grok",
            window: 131_072,
            max_out: None,
            cliff: None,
        },
        // Moonshot Kimi & Z.ai GLM
        Rule {
            frag: "kimi-k3",
            window: 1_048_576,
            max_out: Some(128_000),
            cliff: None,
        },
        Rule {
            frag: "kimi",
            window: 131_072,
            max_out: None,
            cliff: None,
        },
        Rule {
            frag: "glm-5",
            window: 1_048_576,
            max_out: Some(131_072),
            cliff: None,
        },
        Rule {
            frag: "glm",
            window: 131_072,
            max_out: None,
            cliff: None,
        },
        Rule {
            frag: "llama",
            window: 128_000,
            max_out: None,
            cliff: None,
        },
    ];
    TABLE
        .iter()
        .filter(|r| id.contains(r.frag))
        .max_by_key(|r| r.frag.len())
        .map(|r| ModelCap {
            context_window: r.window,
            max_output: r.max_out,
            pricing_cliff: r.cliff,
            source: "builtin".into(),
            updated_ms: 0,
        })
}
