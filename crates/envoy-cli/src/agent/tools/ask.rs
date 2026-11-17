//! The `ask` user-decision tool.
use crate::agent::asks::AskBroker;
use crate::agent::events::{AgentEvent, AskOption};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub(crate) async fn ask_tool(
    asks: &Arc<AskBroker>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    args: &Value,
) -> Result<String> {
    let question = args["question"]
        .as_str()
        .filter(|q| !q.trim().is_empty())
        .context("missing `question`")?;
    let list = args["options"]
        .as_array()
        .context("`ask` needs at least one option")?;
    let mut options = Vec::new();
    for item in list {
        if let Some(label) = item["label"].as_str() {
            options.push(AskOption {
                label: label.to_string(),
                description: item["description"].as_str().map(str::to_string),
                recommended: item["recommended"].as_bool().unwrap_or(false),
            });
        }
    }
    if options.is_empty() {
        bail!("`ask` needs at least one option");
    }
    let recommended = options
        .iter()
        .position(|o| o.recommended)
        .unwrap_or(0);
    let timeout_s = args["timeout_s"].as_u64().unwrap_or(300).clamp(5, 3600);
    let _gate = asks.acquire_prompt().await;
    let id = asks.next_id();
    let (send, recv) = oneshot::channel();
    asks.register(id, send);
    let _ = tx.send(AgentEvent::Ask {
        id,
        question: question.to_string(),
        options: options.clone(),
        recommended,
        timeout_ms: timeout_s * 1000,
    });

    let answer = tokio::time::timeout(Duration::from_secs(timeout_s), recv).await;
    asks.forget(id);
    let (label, via) = match answer {
        Ok(Ok(label)) => (label, "user"),
        // Timeout or the UI went away: take the recommended option.
        _ => {
            let _ = tx.send(AgentEvent::AskClosed { id });
            (options[recommended].label.clone(), "timeout")
        }
    };
    Ok(json!({ "answer": label, "via": via }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn ask_tool_returns_the_chosen_option() {
        let asks = AskBroker::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let answerer = asks.clone();
        tokio::spawn(async move {
            if let Some(AgentEvent::Ask { id, .. }) = rx.recv().await {
                answerer.answer(id, "B".into());
            }
        });
        let out = ask_tool(
            &asks,
            &tx,
            &json!({
                "question": "which?",
                "options": [{"label": "A"}, {"label": "B", "recommended": true}],
                "timeout_s": 5
            }),
        )
        .await
        .unwrap();
        assert!(out.contains("\"answer\":\"B\""), "{out}");
        assert!(out.contains("\"via\":\"user\""), "{out}");
    }

}
