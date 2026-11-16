//! The single agent loop.

use super::config::TurnConfig;
use super::events::AgentEvent;
use super::prompt::build_system_prompt;
use super::providers::{stream_anthropic, stream_openai_chat, stream_openai_codex};
use super::tools::run_tool_call;
use crate::auth::{ensure_fresh_provider, ProviderProtocol};
use crate::daemon;
use crate::session::{ChatMessage, ChatRole};
use anyhow::{bail, Result};
use envoy_service::{dialer_for, AgentDialer, Remote};
use std::time::Duration;
use tokio::sync::mpsc;

const MAX_TOOL_STEPS: usize = 25;

pub async fn run_agent_turn(cfg: TurnConfig, tx: mpsc::UnboundedSender<AgentEvent>) {
    let model_label = cfg.model.display();
    let _ = tx.send(AgentEvent::AssistantStart {
        model: model_label.clone(),
    });
    match execute_turn(cfg, &tx).await {
        Ok((messages, input_tokens, output_tokens, context_tokens)) => {
            let _ = tx.send(AgentEvent::TurnDone {
                messages,
                input_tokens,
                output_tokens,
                context_tokens,
            });
        }
        Err(e) => {
            let _ = tx.send(AgentEvent::TurnError(format!("{e:#}")));
        }
    }
}

async fn execute_turn(
    cfg: TurnConfig,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(Vec<ChatMessage>, u64, u64, u64)> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()?;

    let mut provider = ensure_fresh_provider(&http, &cfg.data_dir, cfg.provider.clone()).await?;
    let mut messages = cfg.history.clone();
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    #[allow(unused_assignments)]
    let mut context_tokens = 0u64;

    if cfg.target == "local" || cfg.target.is_empty() {
        daemon::ensure(&cfg.data_dir).await?;
    }
    let remote: Remote<Box<dyn AgentDialer>> = Remote::new(dialer_for(
        &cfg.data_dir,
        &cfg.target,
        cfg.conduit.clone(),
    ));

    let system = build_system_prompt(&cfg.target, &cfg.cwd);

    for step in 0..MAX_TOOL_STEPS {
        if step > 0 {
            let _ = tx.send(AgentEvent::AssistantStart {
                model: cfg.model.display(),
            });
        }
        provider = ensure_fresh_provider(&http, &cfg.data_dir, provider).await?;

        let reply = match provider.protocol {
            ProviderProtocol::OpenAiChat => {
                stream_openai_chat(&http, &provider, &cfg.model.model_id, &system, &messages, tx)
                    .await?
            }
            ProviderProtocol::OpenAiCodex => {
                stream_openai_codex(&http, &provider, &cfg.model.model_id, &system, &messages, tx)
                    .await?
            }
            ProviderProtocol::Anthropic => {
                stream_anthropic(&http, &provider, &cfg.model.model_id, &system, &messages, tx)
                    .await?
            }
        };

        total_in += reply.input_tokens;
        total_out += reply.output_tokens;
        context_tokens = reply.input_tokens;

        let tool_calls = reply.tool_calls.clone();
        messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: reply.text,
            reasoning: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning)
            },
            tool_calls: tool_calls.clone(),
            tool_call_id: None,
            tool_name: None,
        });

        if tool_calls.is_empty() {
            return Ok((messages, total_in, total_out, context_tokens));
        }

        for tc in tool_calls {
            let result =
                run_tool_call(&remote, &cfg.target, &cfg.cwd, &tc, tx, &cfg.asks).await;
            messages.push(ChatMessage {
                role: ChatRole::Tool,
                content: result,
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: Some(tc.id),
                tool_name: Some(tc.name),
            });
        }

        // Steering: deliver queued user text verbatim before the next model
        // call. No summarising, no reordering.
        let steered: Vec<String> = {
            let mut pending = cfg.steer.lock().unwrap();
            pending.drain(..).collect()
        };
        for text in steered {
            messages.push(ChatMessage {
                role: ChatRole::User,
                content: text,
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                tool_name: None,
            });
        }
    }

    bail!("reached maximum tool steps ({MAX_TOOL_STEPS}) without a final response")
}
