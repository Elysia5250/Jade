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
