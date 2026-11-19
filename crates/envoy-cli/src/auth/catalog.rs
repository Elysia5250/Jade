//! Builtin provider catalog.
use super::{AuthMethodDef, AuthMethodKind, BuiltinProvider, ProviderProtocol};

pub const BUILTIN_PROVIDERS: &[BuiltinProvider] = &[
    BuiltinProvider {
        id: "openai",
        name: "OpenAI / ChatGPT",
        subtitle: "ChatGPT Plus/Pro OAuth or API Key",
        default_base_url: "https://api.openai.com/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("OPENAI_API_KEY"),
        methods: &[
            AuthMethodDef {
                kind: AuthMethodKind::OpenAiBrowserOAuth,
                label: "ChatGPT Plus / Pro (OAuth — Browser)",
                hint: "Opens browser login on localhost:1455 (with manual paste fallback)",
            },
            AuthMethodDef {
                kind: AuthMethodKind::OpenAiDeviceOAuth,
                label: "ChatGPT Plus / Pro (OAuth — Device Code)",
                hint: "Headless/SSH friendly: enter a short code at auth.openai.com",
            },
            AuthMethodDef {
                kind: AuthMethodKind::OpenAiApiKey,
                label: "OpenAI API Key",
                hint: "Pay-as-you-go platform.openai.com key (optional custom Base URL)",
            },
        ],
    },
    BuiltinProvider {
        id: "anthropic",
        name: "Anthropic (Claude)",
        subtitle: "Claude Pro/Max OAuth or API Key",
        default_base_url: "https://api.anthropic.com/v1",
        protocol: ProviderProtocol::Anthropic,
        env_var: Some("ANTHROPIC_API_KEY"),
        methods: &[
            AuthMethodDef {
                kind: AuthMethodKind::AnthropicClaudeOAuth,
                label: "Claude Pro / Max (OAuth)",
                hint: "Sign in with your claude.ai subscription and paste the code",
            },
            AuthMethodDef {
                kind: AuthMethodKind::AnthropicConsoleOAuth,
                label: "Anthropic Console (OAuth -> Create API Key)",
                hint: "Sign in with Anthropic Console to mint an API key automatically",
            },
            AuthMethodDef {
                kind: AuthMethodKind::AnthropicApiKey,
                label: "Anthropic API Key",
                hint: "Enter an sk-ant-... API key (optional custom Base URL)",
            },
        ],
    },
    BuiltinProvider {
        id: "github-copilot",
        name: "GitHub Copilot",
        subtitle: "GitHub OAuth Device Flow",
        default_base_url: "https://api.githubcopilot.com",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: None,
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::GitHubCopilotDeviceOAuth,
            label: "GitHub Copilot (OAuth Device Code)",
            hint: "Sign in at github.com/login/device with your Copilot subscription",
        }],
    },
    BuiltinProvider {
        id: "deepseek",
        name: "DeepSeek",
        subtitle: "API Key",
        default_base_url: "https://api.deepseek.com/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("DEEPSEEK_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "DeepSeek API Key",
            hint: "platform.deepseek.com API key",
        }],
    },
    BuiltinProvider {
        id: "google",
        name: "Google Gemini",
        subtitle: "API Key",
        default_base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("GEMINI_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "Gemini API Key",
            hint: "Google AI Studio API key",
        }],
    },
    BuiltinProvider {
        id: "xai",
        name: "xAI (Grok)",
        subtitle: "API Key",
        default_base_url: "https://api.x.ai/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("XAI_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "xAI API Key",
            hint: "console.x.ai API key",
        }],
    },
    BuiltinProvider {
        id: "qwen",
        name: "Alibaba Qwen",
        subtitle: "DashScope API Key",
        default_base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("DASHSCOPE_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "DashScope API Key",
            hint: "China Beijing key. International keys use https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        }],
    },
    BuiltinProvider {
        id: "zhipu",
        name: "Zhipu GLM",
        subtitle: "API Key",
        default_base_url: "https://open.bigmodel.cn/api/paas/v4",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("ZHIPU_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "Zhipu API Key",
            hint: "open.bigmodel.cn API key",
        }],
    },
    BuiltinProvider {
        id: "moonshot",
        name: "Moonshot (Kimi)",
        subtitle: "API Key",
        default_base_url: "https://api.moonshot.cn/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("MOONSHOT_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "Moonshot API Key",
            hint: "platform.moonshot.cn key. International endpoint is https://api.moonshot.ai/v1",
        }],
    },
    BuiltinProvider {
        id: "minimax",
        name: "MiniMax",
        subtitle: "API Key",
        default_base_url: "https://api.minimax.chat/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("MINIMAX_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "MiniMax API Key",
            hint: "platform.minimax.io API key",
        }],
    },
    BuiltinProvider {
        id: "volcengine",
        name: "Volcengine Ark",
        subtitle: "Doubao API Key",
        default_base_url: "https://ark.cn-beijing.volces.com/api/v3",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("ARK_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "Ark API Key",
            hint: "Volcengine Ark key. Set the model to an endpoint you enabled",
        }],
    },
    BuiltinProvider {
        id: "mistral",
        name: "Mistral",
        subtitle: "API Key",
        default_base_url: "https://api.mistral.ai/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("MISTRAL_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "Mistral API Key",
            hint: "console.mistral.ai API key",
        }],
    },
    BuiltinProvider {
        id: "groq",
        name: "Groq",
        subtitle: "API Key",
        default_base_url: "https://api.groq.com/openai/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("GROQ_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "Groq API Key",
            hint: "console.groq.com API key",
        }],
    },
    BuiltinProvider {
        id: "openrouter",
        name: "OpenRouter",
        subtitle: "API Key",
        default_base_url: "https://openrouter.ai/api/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("OPENROUTER_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "OpenRouter API Key",
            hint: "openrouter.ai key",
        }],
    },
    BuiltinProvider {
        id: "siliconflow",
        name: "SiliconFlow",
        subtitle: "OpenAI-compatible relay",
        default_base_url: "https://api.siliconflow.cn/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("SILICONFLOW_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "SiliconFlow API Key",
            hint: "siliconflow.cn key. Global endpoint is https://api.siliconflow.com/v1",
        }],
    },
    BuiltinProvider {
        id: "aihubmix",
        name: "AIHubMix",
        subtitle: "OpenAI-compatible relay",
        default_base_url: "https://aihubmix.com/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("AIHUBMIX_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "AIHubMix API Key",
            hint: "aihubmix.com key. Model ids come from /models",
        }],
    },
    BuiltinProvider {
        id: "ai302",
        name: "302.AI",
        subtitle: "OpenAI-compatible relay",
        default_base_url: "https://api.302.ai/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("AI302_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "302.AI API Key",
            hint: "302.ai key. Model ids come from /models",
        }],
    },
    BuiltinProvider {
        id: "opencode-zen",
        name: "OpenCode Zen",
        subtitle: "Pay as you go",
        default_base_url: "https://opencode.ai/zen/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("OPENCODE_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "OpenCode Zen API Key",
            hint: "Zen console key. The model list is fetched from /models.",
        }],
    },
    BuiltinProvider {
        id: "opencode-go",
        name: "OpenCode Go",
        subtitle: "Monthly subscription",
        default_base_url: "https://opencode.ai/zen/go/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("OPENCODE_GO_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "OpenCode Go API Key",
            hint: "Go subscription key. A different catalog from Zen.",
        }],
    },
    BuiltinProvider {
        id: "custom",
        name: "Custom Provider",
        subtitle: "OpenAI / Anthropic Compatible (Ollama, vLLM, Relay...)",
        default_base_url: "http://127.0.0.1:11434/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: None,
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::CustomForm,
            label: "Configure custom endpoint",
            hint: "Specify Base URL, Protocol, optional API key, and Model ID",
        }],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_providers_use_api_keys_and_have_env_defaults() {
        let ids = [
            "xai",
            "qwen",
            "zhipu",
            "moonshot",
            "minimax",
            "volcengine",
            "mistral",
            "groq",
            "siliconflow",
            "aihubmix",
            "ai302",
        ];
        for id in ids {
            let prov = BUILTIN_PROVIDERS
                .iter()
                .find(|p| p.id == id)
                .unwrap_or_else(|| panic!("missing {id}"));
            assert_eq!(prov.protocol, ProviderProtocol::OpenAiChat);
            assert!(prov.env_var.is_some(), "{id}");
            assert!(
                super::super::routing::env_base_url_var(id).is_some(),
                "{id}"
            );
            assert!(
                !super::super::routing::default_env_model(id).is_empty(),
                "{id}"
            );
            assert!(prov.default_base_url.starts_with("https://"), "{id}");
            assert_eq!(prov.methods[0].kind, AuthMethodKind::SimpleApiKey);
        }
    }
}
