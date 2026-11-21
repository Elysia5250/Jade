//! Render one headless turn's event stream to stdout and stderr.

use envoy_agent::agent::AgentEvent;
use envoy_agent::runtime::SeqEvent;
use std::io::Write;

use super::OutputFormat;

pub(super) struct TurnStreamDone {
    pub(super) messages: Vec<envoy_agent::session::ChatMessage>,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) context_tokens: u64,
    pub(super) tool_calls: usize,
    pub(super) accumulated_text: String,
}

#[derive(Debug)]
pub(super) struct TurnFailure {
    pub(super) message: String,
    pub(super) tool_calls: usize,
}

pub(super) trait PullEvents {
    fn pull(&mut self) -> impl std::future::Future<Output = Option<SeqEvent>> + Send;
}

impl PullEvents for tokio::sync::mpsc::Receiver<SeqEvent> {
    async fn pull(&mut self) -> Option<SeqEvent> {
        self.recv().await
    }
}

impl PullEvents for tokio::sync::mpsc::UnboundedReceiver<SeqEvent> {
    async fn pull(&mut self) -> Option<SeqEvent> {
        self.recv().await
    }
}

pub(super) async fn collect_events<R, WOut, WErr>(
    rx: &mut R,
    fmt: OutputFormat,
    out: &mut WOut,
    err: &mut WErr,
) -> std::result::Result<TurnStreamDone, TurnFailure>
where
    R: PullEvents,
    WOut: Write,
    WErr: Write,
{
    let mut accumulated_text = String::new();
    let mut tool_calls = 0usize;
    let mut need_step_sep = false;
    let mut wrote_text = false;
    let mut last_char_nl = false;
    let mut saw_approval_denial = false;

    while let Some(seq_ev) = rx.pull().await {
        if fmt == OutputFormat::StreamJson {
            if let Ok(line) = serde_json::to_string(&seq_ev) {
                if let Err(e) = writeln!(out, "{line}").and_then(|()| out.flush()) {
                    return Err(TurnFailure {
                        message: format!("stdout write failed: {e}"),
                        tool_calls,
                    });
                }
            }
        }

        match seq_ev.event {
            AgentEvent::AssistantStart { .. } => {
                if fmt == OutputFormat::Text && wrote_text && need_step_sep {
                    if !last_char_nl {
                        if let Err(e) = writeln!(out) {
                            return Err(TurnFailure {
                                message: format!("stdout write failed: {e}"),
                                tool_calls,
                            });
                        }
                    }
                    need_step_sep = false;
                }
            }
            AgentEvent::TextDelta(chunk) => {
                accumulated_text.push_str(&chunk);
                if fmt == OutputFormat::Text && !chunk.is_empty() {
                    if let Err(e) = write!(out, "{chunk}").and_then(|()| out.flush()) {
                        return Err(TurnFailure {
                            message: format!("stdout write failed: {e}"),
                            tool_calls,
                        });
                    }
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
                        "[hint] tool approval required in headless mode; re-run with --permission-mode accept-edits / auto / turbo or --dangerously-skip-permissions"
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
                cache_read_tokens,
                cache_create_tokens,
                ..
            } => {
                if fmt == OutputFormat::Text && (cache_read_tokens > 0 || cache_create_tokens > 0) {
                    let _ = writeln!(
                        err,
                        "[cache] read={cache_read_tokens} create={cache_create_tokens}"
                    );
                }
                if fmt == OutputFormat::Text && wrote_text && !last_char_nl {
                    if let Err(e) = writeln!(out).and_then(|()| out.flush()) {
                        return Err(TurnFailure {
                            message: format!("stdout write failed: {e}"),
                            tool_calls,
                        });
                    }
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
                    if let Err(e) = writeln!(out).and_then(|()| out.flush()) {
                        return Err(TurnFailure {
                            message: format!("stdout write failed: {e}"),
                            tool_calls,
                        });
                    }
                }
                return Err(TurnFailure {
                    message: msg,
                    tool_calls,
                });
            }
            _ => {}
        }
    }

    Err(TurnFailure {
        message: "agent event stream closed unexpectedly".into(),
        tool_calls,
    })
}
