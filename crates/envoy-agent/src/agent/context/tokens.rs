//! Token estimation, dynamic calibration, and session usage reporting.

use crate::session::{ChatMessage, ReasoningBlock, Session};

#[derive(Debug, Clone, Copy)]
pub struct TokenCalibration {
    /// Multiplier applied to heuristic estimates based on recent provider
    /// `input_tokens` measurements.
    pub factor: f64,
}

impl Default for TokenCalibration {
    fn default() -> Self {
        Self { factor: 1.0 }
    }
}

impl TokenCalibration {
    /// Update calibration using a measured provider prompt token count against
    /// the raw heuristic estimate for that same request.
    pub fn observe(&mut self, raw_estimate: u64, actual_tokens: u64) {
        if raw_estimate < 400 || actual_tokens < 400 {
            return;
        }
        let observed = (actual_tokens as f64 / raw_estimate as f64).clamp(0.65, 1.65);
        self.factor = if (self.factor - 1.0).abs() < f64::EPSILON {
            observed
        } else {
            (self.factor * 0.6 + observed * 0.4).clamp(0.65, 1.65)
        };
    }

    /// Sample only when `raw_estimate` and `actual_tokens` describe the same
    /// uncompressed request. Overflow compaction must pass `false`.
    pub fn observe_uncompressed(
        &mut self,
        raw_estimate: u64,
        actual_tokens: u64,
        same_uncompressed_request: bool,
    ) {
        if same_uncompressed_request {
            self.observe(raw_estimate, actual_tokens);
        }
    }

    pub fn from_stored(factor: f64) -> Self {
        if (0.65..=1.65).contains(&factor) {
            Self { factor }
        } else {
            Self::default()
        }
    }

    pub fn scale(self, raw_tokens: u64) -> u64 {
        ((raw_tokens as f64) * self.factor).round().max(0.0) as u64
    }
}

/// Estimate tokens for text. ASCII is ~4 bytes/token. CJK and other
/// non-ASCII characters are ~1.8 tokens each (`* 9 / 5`).
pub fn estimate_text_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let mut ascii_bytes = 0u64;
    let mut non_ascii_chars = 0u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_bytes += 1;
        } else {
            non_ascii_chars += 1;
        }
    }
    let ascii_tokens = ascii_bytes.div_ceil(4);
    let non_ascii_tokens = (non_ascii_chars * 9).div_ceil(5);
    ascii_tokens + non_ascii_tokens
}

pub fn estimate_message_tokens(msg: &ChatMessage) -> u64 {
    let mut total = 6u64; // role + framing overhead
    total += estimate_text_tokens(&msg.content);
    // The display string is a transcript copy. Only structured blocks are
    // replayed to the provider, so only those move the compaction trigger.
    total += replayed_reasoning_tokens(&msg.reasoning_blocks);
    for tc in &msg.tool_calls {
        total += 10;
        total += estimate_text_tokens(&tc.name);
        total += estimate_text_tokens(&tc.arguments);
    }
    if let Some(id) = &msg.tool_call_id {
        total += estimate_text_tokens(id);
    }
    total
}

pub fn estimate_history_tokens(messages: &[ChatMessage]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Same unit as the compaction trigger: raw estimate scaled by calibration.
pub fn message_cost(msg: &ChatMessage, calibration: TokenCalibration) -> u64 {
    calibration.scale(estimate_message_tokens(msg))
}

/// Built-in tool schemas measured from the active JSON, plus each extra schema.
pub fn tools_overhead(extra_tools: &[serde_json::Value]) -> u64 {
    builtin_tools_tokens()
        + extra_tools
            .iter()
            .map(|tool| estimate_text_tokens(&tool.to_string()))
            .sum::<u64>()
}

fn builtin_tools_tokens() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let tools = crate::agent::tools::openai_tools_json();
        estimate_text_tokens(&serde_json::to_string(&tools).unwrap_or_default())
    })
}

fn replayed_reasoning_tokens(blocks: &[ReasoningBlock]) -> u64 {
    blocks
        .iter()
        .map(|block| match block {
            ReasoningBlock::Thinking { text, signature } => {
                estimate_text_tokens(text) + estimate_text_tokens(signature)
            }
            ReasoningBlock::Redacted { data } => estimate_text_tokens(data),
            ReasoningBlock::Encrypted { id, content } => {
                estimate_text_tokens(id) + estimate_text_tokens(content)
            }
        })
        .sum()
}

pub fn estimate_turn_raw_tokens(
    system: &str,
    messages: &[ChatMessage],
    tools_overhead: u64,
) -> u64 {
    estimate_text_tokens(system) + tools_overhead + estimate_history_tokens(messages)
}

pub fn estimate_turn_tokens(
    system: &str,
    messages: &[ChatMessage],
    tools_overhead: u64,
    calibration: TokenCalibration,
) -> u64 {
    calibration.scale(estimate_turn_raw_tokens(system, messages, tools_overhead))
}

/// `(tokens, estimated)`. A finished turn stores the provider's reported prompt
/// size; before that, messages are estimated.
pub fn context_used(session: &Session) -> (u64, bool) {
    if session.prompt_tokens > 0 {
        return (session.prompt_tokens, false);
    }
    if session.messages.is_empty() {
        return (0, false);
    }
    (estimate_history_tokens(&session.messages), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ChatRole, ToolCall};

    #[test]
    fn estimates_ascii_and_cjk_and_tool_calls() {
        let ascii = estimate_text_tokens("abcdefgh");
        assert_eq!(ascii, 2);

        let cjk = estimate_text_tokens("你好世界测试长度"); // 8 chars * 1.8 = 14.4 -> 15
        assert_eq!(cjk, 15);

        let msg = ChatMessage {
            role: ChatRole::Assistant,
            content: "checking files".into(),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: r#"{"path":"src/main.rs"}"#.into(),
            }],
            tool_call_id: None,
            tool_name: None,
        
            reasoning_blocks: Vec::new(),};
        assert!(estimate_message_tokens(&msg) >= 20);
        let mut with_reasoning = msg.clone();
        with_reasoning.reasoning = Some("x".repeat(4_000));
        assert_eq!(
            estimate_message_tokens(&msg),
            estimate_message_tokens(&with_reasoning)
        );
    }

    #[test]
    fn pure_chinese_1000_chars_lands_in_the_cjk_band() {
        let text: String = "测".repeat(1000);
        let tokens = estimate_text_tokens(&text);
        assert!((1_500..=2_000).contains(&tokens), "{tokens}");
    }

    #[test]
    fn overflow_sample_does_not_move_calibration() {
        let mut cal = TokenCalibration::default();
        cal.observe_uncompressed(10_000, 4_000, false);
        assert_eq!(cal.factor, 1.0);
        cal.observe_uncompressed(10_000, 13_000, true);
        assert!((cal.factor - 1.3).abs() < 0.01);
    }

    #[test]
    fn tools_overhead_follows_the_schema_not_a_flat_1400() {
        let overhead = tools_overhead(&[]);
        assert!(overhead > 1_400, "{overhead}");
        let extra = serde_json::json!({"name": "mcp", "description": "x".repeat(400)});
        let with_extra = tools_overhead(&[extra]);
        assert!(with_extra > overhead);
    }

    #[test]
    fn replayed_reasoning_blocks_add_tokens() {
        let mut msg = ChatMessage::text(ChatRole::Assistant, "ok");
        let before = estimate_message_tokens(&msg);
        msg.reasoning_blocks.push(crate::session::ReasoningBlock::Thinking {
            text: "想".repeat(100),
            signature: "sig".into(),
        });
        assert!(estimate_message_tokens(&msg) > before);
    }

    #[test]
    fn calibration_adapts_for_denser_tokenizers() {
        let mut cal = TokenCalibration::default();
        // Model reports 30% more tokens than our baseline heuristic (like Claude 5).
        cal.observe(10_000, 13_000);
        assert!((cal.factor - 1.3).abs() < 0.01);
        assert_eq!(cal.scale(10_000), 13_000);
    }
}
