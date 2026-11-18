//! Render one headless turn's event stream to stdout and stderr.

use crate::agent::AgentEvent;
use crate::runtime::SeqEvent;
use std::io::Write;

use super::OutputFormat;

pub(super) struct TurnStreamDone {
    pub(super) messages: Vec<crate::session::ChatMessage>,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) context_tokens: u64,
    pub(super) tool_calls: usize,
    pub(super) accumulated_text: String,
}

pub(super) async fn collect_events<WOut: Write, WErr: Write>(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SeqEvent>,
    fmt: OutputFormat,
    out: &mut WOut,
    err: &mut WErr,
) -> std::result::Result<TurnStreamDone, String> {
    let mut accumulated_text = String::new();
    let mut tool_calls = 0usize;
    let mut need_step_sep = false;
    let mut wrote_text = false;
    let mut last_char_nl = false;
    let mut saw_approval_denial = false;

    while let Some(seq_ev) = rx.recv().await {
        if fmt == OutputFormat::StreamJson {
            if let Ok(line) = serde_json::to_string(&seq_ev) {
                let _ = writeln!(out, "{line}");
                let _ = out.flush();
            }
        }

        match seq_ev.event {
            AgentEvent::AssistantStart { .. } => {
                if fmt == OutputFormat::Text && wrote_text && need_step_sep {
                    if !last_char_nl {
                        let _ = writeln!(out);
                    }
                    need_step_sep = false;
                }
            }
            AgentEvent::TextDelta(chunk) => {
                accumulated_text.push_str(&chunk);
                if fmt == OutputFormat::Text && !chunk.is_empty() {
                    let _ = write!(out, "{chunk}");
                    let _ = out.flush();
                    wrote_text = true;
                    last_char_nl = chunk.ends_with('\n');
                }
            }
            AgentEvent::ToolStart { name, subtitle, .. } => {
                tool_calls += 1;
                need_step_sep = true;
                if fmt == OutputFormat::Text {
                    let _ = writeln!(err, "[tool] {name} {subtitle}");
                    let _ = err.flush();
                }
            }
            AgentEvent::ToolOutput {
                err: true, text, ..
            } => {
                if !saw_approval_denial && text.contains("requires interactive approval") {
                    saw_approval_denial = true;
                    let _ = writeln!(
                        err,
                        "[hint] tool approval required in headless mode; re-run with --permission-mode accept-edits / auto or --dangerously-skip-permissions"
                    );
                    let _ = err.flush();
                }
            }
            AgentEvent::ToolDone {
                ok, duration_ms, ..
            } => {
                if fmt == OutputFormat::Text && !ok {
                    let _ = writeln!(err, "[tool] failed ({duration_ms}ms)");
                    let _ = err.flush();
                }
            }
            AgentEvent::Say { text, .. } => {
                if fmt == OutputFormat::Text {
                    let _ = writeln!(err, "[say] {text}");
                    let _ = err.flush();
                }
            }
            AgentEvent::TurnDone {
                messages,
                input_tokens,
                output_tokens,
                context_tokens,
            } => {
                if fmt == OutputFormat::Text && wrote_text && !last_char_nl {
                    let _ = writeln!(out);
                    let _ = out.flush();
                }
                return Ok(TurnStreamDone {
                    messages,
                    input_tokens,
                    output_tokens,
                    context_tokens,
                    tool_calls,
                    accumulated_text,
                });
            }
            AgentEvent::TurnError(msg) => {
                if fmt == OutputFormat::Text && wrote_text && !last_char_nl {
                    let _ = writeln!(out);
                    let _ = out.flush();
                }
                return Err(msg);
            }
            _ => {}
        }
    }

    Err("agent event stream closed unexpectedly".into())
}
