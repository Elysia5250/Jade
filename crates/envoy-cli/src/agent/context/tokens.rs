//! Token estimation, dynamic calibration, and session usage reporting.

use crate::session::{ChatMessage, Session};

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

    pub fn scale(self, raw_tokens: u64) -> u64 {
        ((raw_tokens as f64) * self.factor).round().max(0.0) as u64
    }
}

/// Estimate tokens for text using ASCII ~4 bytes/token and CJK/non-ASCII ~1.5 chars/token.
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
    let non_ascii_tokens = (non_ascii_chars * 2).div_ceil(3);
    ascii_tokens + non_ascii_tokens
}

pub fn estimate_message_tokens(msg: &ChatMessage) -> u64 {
    let mut total = 6u64; // role + framing overhead
    total += estimate_text_tokens(&msg.content);
    if let Some(reasoning) = &msg.reasoning {
        total += estimate_text_tokens(reasoning);
    }
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

/// Conservative baseline overhead for Envoy's 8 built-in tool schemas.
pub const BUILTIN_TOOLS_TOKENS_ESTIMATE: u64 = 1_400;

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

        let cjk = estimate_text_tokens("你好世界测试长度"); // 8 chars -> ceil(16/3) = 6
        assert_eq!(cjk, 6);

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
        };
        assert!(estimate_message_tokens(&msg) >= 20);
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
