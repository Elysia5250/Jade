//! Native harness compaction windows and tiered context budgets.

use super::catalog::{resolve_model_cap, ModelCap};
use crate::auth::{load_prefs, save_prefs, ModelRef};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

const MIN_CUSTOM_BUDGET: u64 = 16_000;
const FOCUSED_CAP: u64 = 200_000;
const BALANCED_CAP: u64 = 512_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTier {
    /// Align each model family to its own native harness compaction window
    /// (e.g. Antigravity Gemini ~512k, Codex 272k/500k, Claude Code ~950k).
    #[default]
    Native,
    /// Precision / cost-guard cap (~200k ceiling).
    Focused,
    /// Balanced cap (~512k ceiling).
    Balanced,
    /// Push close to the physical window (`hard - output_reserve - buffer`).
    Max,
    /// Explicit token trigger set by `/budget <size>`.
    Custom(u64),
}

impl CompactionTier {
    pub fn label(self) -> String {
        match self {
            CompactionTier::Native => "native".into(),
            CompactionTier::Focused => "focused".into(),
            CompactionTier::Balanced => "balanced".into(),
            CompactionTier::Max => "max".into(),
            CompactionTier::Custom(n) => format_k(n),
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim().to_ascii_lowercase();
        match s.as_str() {
            "" | "auto" | "native" | "default" => Some(CompactionTier::Native),
            "focused" | "focus" | "tight" | "200k" => Some(CompactionTier::Focused),
            "balanced" | "balance" | "mid" => Some(CompactionTier::Balanced),
            "max" | "full" | "raw" => Some(CompactionTier::Max),
            _ => parse_token_count(&s).map(CompactionTier::Custom),
        }
    }
}

impl fmt::Display for CompactionTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

#[derive(Debug, Clone)]
pub struct BudgetPlan {
    pub tier: CompactionTier,
    pub hard_window: u64,
    pub compact_at: u64,
    pub prune_at: u64,
    pub tail_keep_tokens: u64,
    pub reserve_output: u64,
    pub harness_profile: &'static str,
    pub source: String,
}

/// Resolve the active [`BudgetPlan`] for `(provider_id, model_id)`.
pub fn resolve_budget(data_dir: &Path, provider_id: &str, model_id: &str) -> BudgetPlan {
    let prefs = load_prefs(data_dir);
    let key = format!("{provider_id}/{model_id}");
    let tier = prefs
        .model_budgets
        .get(&key)
        .copied()
        .map(CompactionTier::Custom)
        .unwrap_or(prefs.budget_tier);
    let cap = resolve_model_cap(data_dir, provider_id, model_id);
    plan_for_cap(model_id, &cap, tier)
}

/// Persist global or per-model budget tier in `model.json`.
pub fn set_budget_tier(
    data_dir: &Path,
    tier: CompactionTier,
    active: Option<&ModelRef>,
) -> anyhow::Result<()> {
    let mut prefs = load_prefs(data_dir);
    match tier {
        CompactionTier::Custom(tokens) => {
            if let Some(m) = active {
                prefs.model_budgets.insert(m.display(), tokens);
            } else {
                prefs.budget_tier = CompactionTier::Custom(tokens);
            }
        }
        other => {
            if let Some(m) = active {
                prefs.model_budgets.remove(&m.display());
            }
            prefs.budget_tier = other;
        }
    }
    save_prefs(data_dir, &prefs)
}

/// Build a [`BudgetPlan`] purely from model ID, physical capabilities, and tier.
pub fn plan_for_cap(model_id: &str, cap: &ModelCap, tier: CompactionTier) -> BudgetPlan {
    let hard = cap.context_window.max(16_384);
    let reserve_output = default_output_reserve(hard, cap.max_output);
    let safety_buffer = (hard * 5 / 100).clamp(4_096, 32_768);
    let max_trigger = hard
        .saturating_sub(reserve_output)
        .saturating_sub(safety_buffer)
        .max(MIN_CUSTOM_BUDGET.min(hard * 80 / 100));

    let (native_trigger, harness_profile) = native_harness_trigger(model_id, cap, max_trigger);
    let compact_at = match tier {
        CompactionTier::Native => native_trigger,
        CompactionTier::Focused => native_trigger.min(FOCUSED_CAP).min(max_trigger),
        CompactionTier::Balanced => native_trigger.min(BALANCED_CAP).min(max_trigger),
        CompactionTier::Max => max_trigger,
        CompactionTier::Custom(tokens) => tokens.clamp(MIN_CUSTOM_BUDGET.min(max_trigger), max_trigger),
    };

    let prune_at = (compact_at * 60 / 100).max(8_192);
    let tail_keep_tokens = (compact_at / 10).clamp(10_000, 48_000).min(compact_at / 3);

    BudgetPlan {
        tier,
        hard_window: hard,
        compact_at,
        prune_at,
        tail_keep_tokens,
        reserve_output,
        harness_profile,
        source: cap.source.clone(),
    }
}

fn default_output_reserve(hard: u64, max_output: Option<u64>) -> u64 {
    let base = if hard >= 500_000 {
        32_768
    } else if hard >= 160_000 {
        16_384
    } else {
        8_192
    };
    match max_output {
        Some(out) if out > 0 => base.min(out).min(hard / 4),
        _ => base.min(hard / 4),
    }
}

fn native_harness_trigger(
    model_id: &str,
    cap: &ModelCap,
    max_trigger: u64,
) -> (u64, &'static str) {
    let id = model_id.to_ascii_lowercase();
    let hard = cap.context_window.max(16_384);

    if has_small_model_tag(&id) {
        return ((hard / 2).min(96_000).min(max_trigger), "small-50%");
    }
    if let Some((target, profile)) = match_claude_or_gemini(&id, hard, max_trigger) {
        return (target, profile);
    }
    if let Some((target, profile)) = match_openai_or_grok(&id, hard, max_trigger) {
        return (target, profile);
    }
    if let Some((target, profile)) = match_cn_or_open_family(&id, hard, max_trigger) {
        return (target, profile);
    }

    conservative_fallback_trigger(hard, cap.pricing_cliff, max_trigger)
}

fn match_claude_or_gemini(
    id: &str,
    hard: u64,
    max_trigger: u64,
) -> Option<(u64, &'static str)> {
    if id.contains("claude") || id.contains("fable") || id.contains("mythos") {
        if hard >= 700_000 {
            if id.contains("fable") || id.contains("mythos") || id.contains("opus") {
                return Some((800_000u64.min(max_trigger), "claude-flagship-800k"));
            }
            if id.contains("haiku") {
                return Some((200_000u64.min(max_trigger), "claude-haiku-200k"));
            }
            return Some((512_000u64.min(max_trigger), "claude-sonnet-512k"));
        }
        if id.contains("haiku") {
            return Some((96_000u64.min(max_trigger), "claude-haiku-96k"));
        }
        return Some((160_000u64.min(max_trigger), "claude-legacy-160k"));
    }

    if id.contains("gemini") && hard >= 700_000 {
        if id.contains("carbon") {
            return Some((640_000u64.min(max_trigger), "gemini-carbon-640k"));
        }
        if id.contains("argon") {
            return Some((200_000u64.min(max_trigger), "gemini-argon-200k"));
        }
        return Some((512_000u64.min(max_trigger), "antigravity-512k"));
    }

    None
}

fn match_openai_or_grok(
    id: &str,
    hard: u64,
    max_trigger: u64,
) -> Option<(u64, &'static str)> {
    if id.contains("gpt-6") {
        if id.contains("luna") {
            return Some((180_000u64.min(max_trigger), "gpt6-luna-180k"));
        }
        if id.contains("astra") && hard >= 700_000 {
            return Some((512_000u64.min(max_trigger), "gpt6-astra-512k"));
        }
        if (id.contains("sol") || id.contains("terra")) && hard >= 550_000 {
            return Some((384_000u64.min(max_trigger), "gpt6-sol-384k"));
        }
        if hard >= 500_000 {
            return Some((384_000u64.min(max_trigger), "gpt6-384k"));
        }
    }
    if (id.contains("gpt-5") || id.contains("codex")) && hard >= 300_000 {
        return Some((244_800u64.min(max_trigger), "codex-272k"));
    }
    if id.contains("gpt-4.1") && hard >= 700_000 {
        return Some((256_000u64.min(max_trigger), "gpt41-256k"));
    }
    if has_token(id, "o1") || has_token(id, "o3") || has_token(id, "o4") {
        return Some((150_000u64.min(max_trigger), "openai-o-150k"));
    }
    if id.contains("gpt-4") {
        return Some((72_000u64.min(max_trigger), "gpt4-legacy-72k"));
    }

    if id.contains("grok") {
        if ["grok-4.7", "grok-4.6", "grok-4.5", "grok-4.3", "grok-4.20"]
            .iter()
            .any(|p| id.contains(p))
        {
            return Some((185_000u64.min(max_trigger), "xai-200k-cliff"));
        }
        if id.contains("grok-build") {
            return Some((160_000u64.min(max_trigger), "grok-build-160k"));
        }
        if hard >= 100_000 {
            return Some((72_000u64.min(max_trigger), "grok-legacy-72k"));
        }
    }

    None
}

fn match_cn_or_open_family(
    id: &str,
    hard: u64,
    max_trigger: u64,
) -> Option<(u64, &'static str)> {
    if id.contains("deepseek") {
        if hard >= 700_000 {
            return Some((272_000u64.min(max_trigger), "deepseek-272k"));
        }
        if id.contains("reasoner") || hard <= 80_000 {
            return Some(((hard * 55 / 100).min(36_000).min(max_trigger), "deepseek-r1-36k"));
        }
        return Some((72_000u64.min(max_trigger), "deepseek-legacy-72k"));
    }
    if id.contains("qwen") {
        if (id.contains("qwen4") || id.contains("qwen3.8-max")) && hard >= 700_000 {
            return Some((272_000u64.min(max_trigger), "qwen-flagship-272k"));
        }
        if (id.contains("flash") || id.contains("2.4t")) && hard >= 200_000 {
            return Some((160_000u64.min(max_trigger), "qwen-flash-160k"));
        }
        if hard >= 100_000 {
            return Some((72_000u64.min(max_trigger), "qwen-legacy-72k"));
        }
    }
    if id.contains("kimi") {
        if id.contains("kimi-k3") && hard >= 700_000 {
            return Some((272_000u64.min(max_trigger), "kimi-k3-272k"));
        }
        if hard >= 100_000 {
            return Some((72_000u64.min(max_trigger), "kimi-legacy-72k"));
        }
    }
    if id.contains("glm") {
        if id.contains("glm-5") && hard >= 700_000 {
            return Some((272_000u64.min(max_trigger), "glm-5-272k"));
        }
        if hard >= 100_000 {
            return Some((72_000u64.min(max_trigger), "glm-legacy-72k"));
        }
    }
    if id.contains("llama") && hard >= 100_000 {
        return Some((72_000u64.min(max_trigger), "llama-legacy-72k"));
    }
    None
}

fn conservative_fallback_trigger(
    hard: u64,
    pricing_cliff: Option<u64>,
    max_trigger: u64,
) -> (u64, &'static str) {
    let mut target = if hard <= 32_768 {
        hard * 50 / 100
    } else if hard <= 131_072 {
        hard * 55 / 100
    } else {
        128_000
    };
    if let Some(cliff) = pricing_cliff {
        target = target.min(cliff * 90 / 100);
    }
    (target.min(max_trigger).max(8_192), "conservative-fallback")
}

fn has_small_model_tag(id: &str) -> bool {
    if id.contains("haiku-4") || id.contains("haiku-3") {
        return true;
    }
    const SMALL_TOKENS: &[&str] = &[
        "mini", "nano", "lite", "small", "tiny", "e2b", "e4b", "0.5b", "1b", "1.5b", "2b", "3b",
        "4b", "7b", "8b", "9b", "12b", "14b", "24b", "27b", "32b",
    ];
    id.split(|c: char| matches!(c, '-' | '_' | ':' | '/' | '[' | ']'))
        .any(|tok| SMALL_TOKENS.contains(&tok))
}

fn has_token(id: &str, needle: &str) -> bool {
    id.split(|c: char| matches!(c, '-' | '_' | ':' | '/'))
        .any(|tok| tok == needle)
}

pub fn parse_token_count(raw: &str) -> Option<u64> {
    let s = raw.trim().to_ascii_lowercase();
    if let Some(n) = s.strip_suffix('k') {
        return n.trim().parse::<u64>().ok().map(|v| v.saturating_mul(1_000));
    }
    if let Some(n) = s.strip_suffix('m') {
        return n
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| *v > 0.0 && *v <= 4.0)
            .map(|v| (v * 1_000_000.0).round() as u64);
    }
    s.parse::<u64>().ok()
}

fn format_k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::catalog::builtin_model_cap;

    fn cap_or(model: &str, fallback: u64) -> ModelCap {
        builtin_model_cap(model).unwrap_or(ModelCap {
            context_window: fallback,
            max_output: None,
            pricing_cliff: None,
            source: "test".into(),
            updated_ms: 0,
        })
    }

    #[test]
    fn native_respects_flagship_hierarchy_and_conservative_small_fallbacks() {
        // S-tier flagships > secondary > lightweight
        let fable = plan_for_cap("claude-fable-5-2", &cap_or("claude-fable-5-2", 1_000_000), CompactionTier::Native);
        let sonnet5 = plan_for_cap("claude-sonnet-5-5", &cap_or("claude-sonnet-5-5", 1_000_000), CompactionTier::Native);
        let haiku5 = plan_for_cap("claude-haiku-5-5", &cap_or("claude-haiku-5-5", 1_000_000), CompactionTier::Native);
        assert_eq!(fable.compact_at, 800_000);
        assert_eq!(sonnet5.compact_at, 512_000);
        assert_eq!(haiku5.compact_at, 200_000);

        let carbon = plan_for_cap("gemini-4-carbon", &cap_or("gemini-4-carbon", 1_048_576), CompactionTier::Native);
        let hydrogen = plan_for_cap("gemini-4-hydrogen", &cap_or("gemini-4-hydrogen", 1_048_576), CompactionTier::Native);
        let argon = plan_for_cap("gemini-4-argon", &cap_or("gemini-4-argon", 1_048_576), CompactionTier::Native);
        assert_eq!(carbon.compact_at, 640_000);
        assert_eq!(hydrogen.compact_at, 512_000);
        assert_eq!(argon.compact_at, 200_000);

        let astra = plan_for_cap("gpt-6-astra", &cap_or("gpt-6-astra", 1_050_000), CompactionTier::Native);
        let sol = plan_for_cap("gpt-6.1-sol", &cap_or("gpt-6.1-sol", 1_050_000), CompactionTier::Native);
        let luna = plan_for_cap("gpt-6-luna", &cap_or("gpt-6-luna", 1_050_000), CompactionTier::Native);
        assert!(astra.compact_at > sol.compact_at && sol.compact_at > luna.compact_at);
        assert_eq!(astra.compact_at, 512_000);
        assert_eq!(sol.compact_at, 384_000);
        assert_eq!(luna.compact_at, 180_000);

        // DeepSeek / Qwen4 / Kimi-K3 / GLM-5 sweet spot at 272k
        let ds_v4 = plan_for_cap("deepseek-v4.1-flash", &cap_or("deepseek-v4.1-flash", 1_000_000), CompactionTier::Native);
        let qwen4 = plan_for_cap("qwen4-max", &cap_or("qwen4-max", 1_000_000), CompactionTier::Native);
        let glm5 = plan_for_cap("glm-5.3", &cap_or("glm-5.3", 1_048_576), CompactionTier::Native);
        assert_eq!(ds_v4.compact_at, 272_000);
        assert_eq!(qwen4.compact_at, 272_000);
        assert_eq!(glm5.compact_at, 272_000);

        // Legacy, small-parameter, and unknown models are much more conservative
        let ds_r1 = plan_for_cap("deepseek-reasoner", &cap_or("deepseek-reasoner", 65_536), CompactionTier::Native);
        let mini = plan_for_cap("gpt-4o-mini", &cap_or("gpt-4o-mini", 128_000), CompactionTier::Native);
        let qwen_7b = plan_for_cap("qwen2.5-7b-instruct", &cap_or("qwen2.5-7b-instruct", 131_072), CompactionTier::Native);
        let unknown_64k = plan_for_cap("custom-relay", &cap_or("custom-relay", 65_536), CompactionTier::Native);
        let unknown_1m = plan_for_cap("custom-1m", &cap_or("custom-1m", 1_000_000), CompactionTier::Native);
        assert_eq!(ds_r1.compact_at, 36_000);
        assert_eq!(mini.compact_at, 64_000);
        assert_eq!(qwen_7b.compact_at, 65_536);
        assert_eq!(unknown_64k.compact_at, 36_044);
        assert_eq!(unknown_1m.compact_at, 128_000);

        // Focused / Balanced clamp cleanly
        assert_eq!(
            plan_for_cap("claude-fable-5-2", &cap_or("claude-fable-5-2", 1_000_000), CompactionTier::Focused).compact_at,
            200_000
        );
        assert_eq!(
            plan_for_cap("claude-fable-5-2", &cap_or("claude-fable-5-2", 1_000_000), CompactionTier::Balanced).compact_at,
            512_000
        );
    }
}
