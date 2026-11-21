//! Phase 5: prompt-cache freshness, compaction fidelity, reasoning replay.

use super::context::{
    estimate_turn_tokens, prepare_context, skip_inplace_mask, BudgetPlan, CompactionTier,
    TokenCalibration,
};
use super::prompt::{
    sync_environment, sync_pre_model_tail, ENV_PREFIX, ENV_UPDATE_PREFIX, PRE_MODEL_PREFIX,
};
use super::providers::{
    anthropic_messages, chat_body, chat_messages, codex_request_body, mark_last_tool,
    prompt_cache_key, system_blocks,
};
use super::tools::{openai_tools_json_filtered, ToolFilter};
use crate::auth::{Credential, ProviderProtocol, StoredProvider};
use crate::session::{
    strip_prior_turn_reasoning, ChatMessage, ChatRole, ReasoningBlock, Session, ToolCall,
};
use serde_json::{json, Value};
use std::time::Duration;

fn user(text: &str) -> ChatMessage {
    ChatMessage::text(ChatRole::User, text)
}

fn assistant_call(id: &str, name: &str, arguments: &str) -> ChatMessage {
    let mut message = ChatMessage::text(ChatRole::Assistant, "");
    message.tool_calls = vec![ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }];
    message
}

fn tool_result(id: &str, name: &str, body: &str) -> ChatMessage {
    let mut message = ChatMessage::text(ChatRole::Tool, body);
    message.tool_call_id = Some(id.into());
    message.tool_name = Some(name.into());
    message
}

fn roles(messages: &[Value]) -> Vec<&str> {
    messages
        .iter()
        .map(|message| message["role"].as_str().unwrap_or(""))
        .collect()
}

fn text_blocks(value: &Value) -> Vec<&str> {
    let mut found = Vec::new();
    if let Some(text) = value["text"].as_str() {
        found.push(text);
    }
    if let Some(items) = value.as_array() {
        for item in items {
            found.extend(text_blocks(item));
        }
    } else if let Some(object) = value.as_object() {
        for (key, child) in object {
            if key != "text" {
                found.extend(text_blocks(child));
            }
        }
    }
    found
}

fn cache_types(value: &Value) -> Vec<&str> {
    let mut found = Vec::new();
    if let Some(kind) = value["cache_control"]["type"].as_str() {
        found.push(kind);
    }
    match value {
        Value::Array(items) => {
            for item in items {
                found.extend(cache_types(item));
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                if key != "cache_control" {
                    found.extend(cache_types(child));
                }
            }
        }
        _ => {}
    }
    found
}

fn stable_prefix(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        if message.content.starts_with(ENV_UPDATE_PREFIX)
            || message.content.starts_with(PRE_MODEL_PREFIX)
        {
            break;
        }
        out.push_str(&format!("{:?}\n{}\n", message.role, message.content));
    }
    out
}

fn provider() -> StoredProvider {
    StoredProvider {
        id: "test".into(),
        name: "Test".into(),
        protocol: ProviderProtocol::OpenAiChat,
        base_url: "http://127.0.0.1:9/v1".into(),
        credential: Credential::ApiKey {
            key: "sk-test".into(),
        },
        custom_models: Vec::new(),
        fetched_models: Vec::new(),
        model_routes: Default::default(),
        updated_ms: 0,
    }
}

#[test]
fn anthropic_body_merges_users_strips_empty_text_and_marks_cache() {
    let history = vec![
        user("alpha"),
        user(""),
        user("beta"),
        ChatMessage::text(ChatRole::Assistant, ""),
        user("gamma"),
        assistant_call("1", "exec", "{}"),
        tool_result("1", "exec", "ok"),
        user("steer"),
        user("<environment_update>\ntodos changed\n</environment_update>"),
    ];
    let messages = anthropic_messages(&history);
    let rendered = roles(&messages);
    for pair in rendered.windows(2) {
        assert!(pair != ["user", "user"], "{rendered:?}");
    }
    let body = serde_json::to_string(&messages).unwrap();
    assert!(!body.contains("\"text\":\"\""), "{body}");
    assert!(text_blocks(&json!(messages))
        .iter()
        .all(|text| !text.is_empty()));

    assert!(messages.iter().any(
        |message| message["content"].as_array().is_some_and(|blocks| blocks
            .iter()
            .any(|block| block.get("cache_control").is_some()))
    ));
    assert!(cache_types(&json!(messages))
        .iter()
        .all(|kind| *kind == "ephemeral"));
    for message in &messages {
        let Some(blocks) = message["content"].as_array() else {
            continue;
        };
        for block in blocks {
            if block.get("cache_control").is_some() {
                let text = block["text"].as_str().unwrap_or("");
                assert!(!text.starts_with("<environment_update>"), "{block}");
            }
        }
    }

    let system = system_blocks("You are Jade");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    let mut tools = vec![json!({"name": "read"}), json!({"name": "exec"})];
    mark_last_tool(&mut tools);
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn anthropic_tool_continuation_replays_thinking_signature_and_redacted() {
    let mut assistant = ChatMessage::text(ChatRole::Assistant, "calling");
    assistant.reasoning_blocks = vec![
        ReasoningBlock::Thinking {
            text: "plan".into(),
            signature: "sig-1".into(),
        },
        ReasoningBlock::Redacted {
            data: "redacted-blob".into(),
        },
        ReasoningBlock::Encrypted {
            id: "rs".into(),
            content: "not-anthropic".into(),
        },
    ];
    assistant.tool_calls = vec![ToolCall {
        id: "tu_1".into(),
        name: "read".into(),
        arguments: r#"{"path":"a.rs"}"#.into(),
    }];
    let history = vec![
        user("read a"),
        assistant,
        tool_result("tu_1", "read", "file"),
    ];
    let messages = anthropic_messages(&history);
    let blocks = messages[1]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["thinking"], "plan");
    assert_eq!(blocks[0]["signature"], "sig-1");
    assert_eq!(blocks[1]["type"], "redacted_thinking");
    assert_eq!(blocks[1]["data"], "redacted-blob");
    assert_eq!(blocks[2]["type"], "text");
    assert_eq!(blocks[2]["text"], "calling");
    assert_eq!(blocks[3]["type"], "tool_use");
    assert_eq!(blocks[3]["id"], "tu_1");
    assert!(blocks.iter().all(|block| block["type"] != "encrypted"));
}

#[test]
fn prefix_bytes_stay_identical_from_step_2_through_5_at_65_percent() {
    let system = "stable system prompt";
    let mut messages = vec![
        user("hello"),
        assistant_call("c1", "exec", r#"{"command":"echo"}"#),
        tool_result("c1", "exec", &format!("{}\n", "Z".repeat(80)).repeat(40)),
        user("latest"),
    ];
    sync_environment(
        &mut messages,
        Some("skills\n## Active session checklist\n- first"),
    );
    let plan_probe = estimate_turn_tokens(system, &messages, 0, TokenCalibration::default());
    let compact_at = (plan_probe * 100 / 65).max(plan_probe);
    let plan = BudgetPlan {
        tier: CompactionTier::Custom(compact_at),
        hard_window: compact_at.max(16_384),
        compact_at,
        prune_at: compact_at * 60 / 100,
        tail_keep_tokens: 80,
        reserve_output: 256,
        harness_profile: "test",
        source: "test".into(),
    };
    assert!(plan_probe > plan.prune_at && plan_probe <= plan.compact_at);
    assert!(!skip_inplace_mask(false, plan_probe, &plan));
    super::context::mask_old_observations(
        &mut messages,
        plan.tail_keep_tokens,
        TokenCalibration::default(),
    );
    assert!(messages
        .iter()
        .any(|message| message.content.contains("[masked ")));

    let mut seen = Vec::new();
    for step in 2..=5 {
        let note = format!("skills\n## Active session checklist\n- todo {step}");
        sync_environment(&mut messages, Some(&note));
        assert!(skip_inplace_mask(true, plan_probe, &plan));
        assert!(!skip_inplace_mask(true, plan.compact_at + 1, &plan));
        sync_pre_model_tail(&mut messages, Some(&format!("hook {step}")));
        seen.push((system.to_string(), stable_prefix(&messages)));
    }
    assert!(seen.windows(2).all(|pair| pair[0] == pair[1]));
    assert!(messages[0].content.starts_with(ENV_PREFIX));
    assert!(!messages[0].content.contains("todo 5"));
    assert!(messages
        .iter()
        .any(|message| message.content.contains("todo 5")));
    assert!(messages
        .iter()
        .any(|message| message.content.starts_with(PRE_MODEL_PREFIX)));
    let masked = messages
        .iter()
        .find(|message| message.role == ChatRole::Tool)
        .unwrap();
    assert!(masked.content.contains("[masked "));
    assert!(masked.content.matches('Z').count() < 3_000);
}

#[test]
fn dossier_keeps_multiline_user_requirements_including_the_tail() {
    let text = "need the parser\nto keep every line\nTAIL REQUIREMENT: preserve this last line";
    let (_, dossier) = super::context::summary::condense_for_summary(&[user(text)]);
    assert!(dossier.contains("need the parser"), "{dossier}");
    assert!(dossier.contains("to keep every line"), "{dossier}");
    assert!(
        dossier.contains("TAIL REQUIREMENT: preserve this last line"),
        "{dossier}"
    );
}

#[test]
fn summary_request_is_one_user_message_and_sends_no_tools() {
    let messages = super::context::summary::summary_request_messages("BODY", "INSTRUCTION");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, ChatRole::User);
    assert!(messages[0].content.contains("BODY"));
    assert!(messages[0].content.contains("INSTRUCTION"));
    let tools = openai_tools_json_filtered(None, &ToolFilter::none(), &[]);
    assert!(tools.is_empty(), "{tools:?}");
}

#[test]
fn merge_summary_caps_at_16_000_chars() {
    let previous = "甲".repeat(20_000);
    let summary = super::context::summary::merge_summary(Some(&previous), "");
    assert!(
        summary.chars().count() <= 16_000,
        "{}",
        summary.chars().count()
    );
}

#[tokio::test]
async fn force_mask_returns_pruned_only_without_a_provider_call() {
    let dir = std::env::temp_dir().join(format!(
        "jade-p5-mask-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut messages = vec![
        user("please run"),
        assistant_call("c1", "exec", r#"{"command":"echo hi"}"#),
        tool_result("c1", "exec", &"stdout line\n".repeat(400)),
        user("latest"),
    ];
    let plan = BudgetPlan {
        tier: CompactionTier::Custom(1_000_000),
        hard_window: 1_000_000,
        compact_at: 1_000_000,
        prune_at: 0,
        tail_keep_tokens: 80,
        reserve_output: 256,
        harness_profile: "test",
        source: "test".into(),
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        prepare_context(
            &reqwest::Client::new(),
            &dir,
            &provider(),
            "test-model",
            "local",
            &dir,
            "system",
            &mut messages,
            TokenCalibration::default(),
            &plan,
            0,
            true,
            None,
            None,
            None,
            "sess",
        ),
    )
    .await
    .expect("force mask tried to call the provider")
    .unwrap()
    .expect("mask freed space");
    assert!(outcome.pruned_only);
    assert!(outcome.after_tokens < outcome.before_tokens);
    assert!(messages
        .iter()
        .any(|message| message.content.contains("[masked ")));
}

#[test]
fn codex_body_requests_encrypted_reasoning_and_replays_it() {
    let mut assistant = ChatMessage::text(ChatRole::Assistant, "done");
    assistant.reasoning_blocks = vec![ReasoningBlock::Encrypted {
        id: "rs_1".into(),
        content: "enc-blob".into(),
    }];
    assistant.tool_calls = vec![ToolCall {
        id: "c1".into(),
        name: "exec".into(),
        arguments: "{}".into(),
    }];
    let history = vec![user("go"), assistant, tool_result("c1", "exec", "ok")];
    let body = codex_request_body(
        "gpt-5",
        "system",
        &history,
        None,
        &ToolFilter::default(),
        &[],
        "sess-9",
    );
    assert_eq!(body["store"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["prompt_cache_key"], prompt_cache_key("sess-9"));
    let input = body["input"].as_array().unwrap();
    let reasoning = input
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("encrypted reasoning item");
    assert_eq!(reasoning["id"], "rs_1");
    assert_eq!(reasoning["encrypted_content"], "enc-blob");
    assert_eq!(reasoning["summary"], json!([]));
    let assistant_at = input
        .iter()
        .position(|item| item["role"] == "assistant")
        .unwrap();
    assert_eq!(input[assistant_at - 1]["type"], "reasoning");
}

#[test]
fn gateway_deepseek_r1_keeps_reasoning_until_the_next_user_turn() {
    let mut assistant = ChatMessage::text(ChatRole::Assistant, "noted");
    assistant.reasoning = Some("chain of thought".into());
    assistant.tool_calls = vec![ToolCall {
        id: "c1".into(),
        name: "exec".into(),
        arguments: "{}".into(),
    }];
    let mut history = vec![assistant, tool_result("c1", "exec", "ok")];
    let same_turn = chat_messages("openrouter/deepseek-ai/deepseek-r1", "sys", &history);
    let replayed = same_turn
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(replayed["reasoning_content"], "chain of thought");

    let plain = chat_messages("openai/gpt-4.1", "sys", &history);
    assert!(plain
        .iter()
        .all(|message| message.get("reasoning_content").is_none()));

    history.push(user("new question"));
    let next_turn = chat_messages("deepseek-r1", "sys", &history);
    assert!(next_turn
        .iter()
        .all(|message| message.get("reasoning_content").is_none()));

    strip_prior_turn_reasoning(&mut history);
    assert!(history.iter().all(|message| message.reasoning.is_none()));
    assert!(history
        .iter()
        .all(|message| message.reasoning_blocks.is_empty()));
}

#[test]
fn prompt_cache_key_is_stable_and_old_sessions_default_the_new_fields() {
    assert_eq!(prompt_cache_key("session-1"), prompt_cache_key("session-1"));
    assert_ne!(prompt_cache_key("session-1"), prompt_cache_key("session-2"));
    let key = prompt_cache_key("session-1");
    assert!(key.starts_with("jade-"));
    assert_eq!(key.len(), "jade-".len() + 16);

    let session: Session = serde_json::from_str(
        r#"{"id":"s","title":"t","cwd":"/tmp","target":"local","created_ms":1,"updated_ms":1}"#,
    )
    .unwrap();
    assert_eq!(session.calibration_factor, 1.0);
    assert_eq!(session.cache_read_tokens, 0);
    assert_eq!(session.cache_create_tokens, 0);
    assert!(session.messages.is_empty());

    let message: ChatMessage =
        serde_json::from_str(r#"{"role":"assistant","content":"hi"}"#).unwrap();
    assert!(message.reasoning_blocks.is_empty());
    assert_eq!(TokenCalibration::from_stored(0.0).factor, 1.0);
    assert_eq!(TokenCalibration::from_stored(1.3).factor, 1.3);
}

#[test]
fn mid_turn_synthetic_env_preserves_reasoning_and_empty_tools_omitted() {
    let mut assistant = ChatMessage::text(ChatRole::Assistant, "thinking");
    assistant.reasoning = Some("step-by-step".into());
    assistant.tool_calls = vec![ToolCall {
        id: "c1".into(),
        name: "exec".into(),
        arguments: "{}".into(),
    }];
    let history = vec![
        assistant,
        tool_result("c1", "exec", "ok"),
        user("<environment_update>\ntodos updated\n</environment_update>"),
    ];
    for model in [
        "deepseek-r1",
        "moonshotai/kimi-k2-thinking",
        "z-ai/glm-5",
        "qwen3-235b-thinking",
    ] {
        let msgs = chat_messages(model, "sys", &history);
        let replayed = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(replayed["reasoning_content"], "step-by-step", "{model}");
    }
    let body = chat_body(
        "gpt-5",
        "sys",
        &history,
        None,
        &ToolFilter {
            depth: 0,
            allowed: Some(Vec::new()),
        },
        &[],
        "sess-empty",
    );
    assert!(body.get("tools").is_none(), "{body}");
}
