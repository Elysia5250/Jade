//! Headless / non-interactive execution mode (`envoy -p ...`) for CI,
//! scripts, and containerized agent benchmarks (e.g. Terminal-Bench / Harbor).

use crate::Ctx;
use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use envoy_agent::agent::AskBroker;
use envoy_agent::daemon;
use envoy_agent::permissions::{PermissionMode, SandboxOverride};
use envoy_agent::runtime::{Runtime, SendMode};
use envoy_agent::session::{latest_session_for_cwd, load_session, save_session, ChatRole, Session};
use envoy_service::AgentServer;
use serde::Serialize;
use std::io::{Read, Write};
use std::time::Instant;
use tokio::sync::oneshot;

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    StreamJson,
}

#[derive(Args, Debug, Clone, Default)]
pub struct HeadlessArgs {
    /// Run a single non-interactive prompt and print the response (use `-` for stdin).
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    pub prompt: Option<String>,

    /// Override model (`<provider>/<model>` or `<model>`).
    #[arg(long, global = true, env = "ENVOY_MODEL")]
    pub model: Option<String>,

    /// Permission mode (`default`, `accept-edits`, `plan`, `auto`).
    #[arg(long = "permission-mode", global = true, value_name = "MODE")]
    pub permission_mode: Option<String>,

    /// Bypass interactive permission prompts and default OS sandbox restrictions.
    #[arg(long = "dangerously-skip-permissions", global = true)]
    pub dangerously_skip_permissions: bool,

    /// Override tool OS sandbox (`workspace-write`, `read-only`, `off`, `danger-full-access`).
    #[arg(long = "sandbox", global = true, value_name = "POLICY")]
    pub sandbox: Option<String>,

    /// Maximum LLM/tool steps for this run (1..=250; default 100 in headless mode).
    #[arg(long = "max-steps", global = true, value_name = "N")]
    pub max_steps: Option<usize>,

    /// Output format for `-p/--print` runs.
    #[arg(long = "output-format", global = true, value_enum, default_value_t = OutputFormat::Text)]
    pub output_format: OutputFormat,
}

impl HeadlessArgs {
    pub fn is_active(&self) -> bool {
        self.prompt.is_some()
    }
}

pub(crate) fn piped_headless_args(mut args: HeadlessArgs) -> HeadlessArgs {
    if args.prompt.is_none() {
        args.prompt = Some("-".into());
    }
    args
}

#[derive(Debug, Serialize)]
struct JsonReport {
    session_id: String,
    ok: bool,
    result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    context_tokens: u64,
    tool_calls: usize,
    duration_ms: u64,
}

struct EphemeralServer {
    dir: std::path::PathBuf,
    socket: std::path::PathBuf,
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for EphemeralServer {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl EphemeralServer {
    async fn start(data_dir: &std::path::Path) -> Result<Option<Self>> {
        if daemon::is_running(data_dir).await {
            return Ok(None);
        }
        std::fs::create_dir_all(data_dir)?;

        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let base = std::env::temp_dir();
        let base = if base.to_string_lossy().len() > 60 && std::path::Path::new("/tmp").is_dir() {
            std::path::PathBuf::from("/tmp")
        } else {
            base
        };
        let dir = base.join(format!(
            "envoy-eph-{}-{:02x}{:02x}{:02x}{:02x}",
            std::process::id(),
            b[0],
            b[1],
            b[2],
            b[3]
        ));
        std::fs::create_dir_all(&dir)?;

        let socket = dir.join("envoy.sock");
        let listener = tokio::net::UnixListener::bind(&socket)
            .with_context(|| format!("binding {}", socket.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
        }

        let host = std::sync::Arc::new(envoy_service::StoreHost::open(data_dir.to_path_buf())?);
        let server = AgentServer::new(dir.clone(), host);
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let sock_for_srv = socket.clone();
        let task = tokio::spawn(async move {
            daemon::run_server(server, listener, sock_for_srv, None, async move {
                let _ = stop_rx.await;
            })
            .await;
        });

        Ok(Some(Self {
            dir,
            socket,
            stop_tx: Some(stop_tx),
            task: Some(task),
        }))
    }

    async fn shutdown(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn read_prompt(raw: Option<String>) -> Result<String> {
    let raw = raw.context("missing prompt for headless execution")?;
    let prompt = if raw == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading prompt from stdin")?;
        buf
    } else {
        raw
    };
    if prompt.trim().is_empty() {
        bail!("empty prompt");
    }
    Ok(prompt)
}

pub(crate) async fn run_ask(
    ctx: &Ctx,
    continue_last: bool,
    session_id: Option<String>,
    mut args: HeadlessArgs,
    prompt_words: Vec<String>,
) -> Result<()> {
    args.prompt = Some(prompt_words.join(" "));
    run(ctx, continue_last, session_id, args).await
}

pub(crate) async fn run(
    ctx: &Ctx,
    continue_last: bool,
    session_id: Option<String>,
    args: HeadlessArgs,
) -> Result<()> {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    run_with_writers(ctx, continue_last, session_id, args, &mut out, &mut err).await
}

pub(crate) async fn run_with_writers<WOut: Write, WErr: Write>(
    ctx: &Ctx,
    continue_last: bool,
    session_id: Option<String>,
    args: HeadlessArgs,
    out: &mut WOut,
    err: &mut WErr,
) -> Result<()> {
    let prompt = read_prompt(args.prompt.clone())?;

    let perm_mode = match args.permission_mode.as_deref() {
        Some(raw) => PermissionMode::parse(raw).with_context(|| {
            format!("unknown permission mode `{raw}` (expected: default, accept-edits, plan, auto)")
        })?,
        None if args.dangerously_skip_permissions => PermissionMode::Auto,
        None => PermissionMode::Default,
    };

    let sandbox_override = match args.sandbox.as_deref() {
        Some(raw) => Some(SandboxOverride::parse(raw).with_context(|| {
            format!(
                "unknown sandbox policy `{raw}` (expected: workspace-write, read-only, off, danger-full-access)"
            )
        })?),
        None => None,
    };

    let mut session = if let Some(sid) = session_id {
        load_session(&ctx.data_dir, &sid)?
    } else if continue_last {
        latest_session_for_cwd(&ctx.data_dir, &ctx.cwd)
            .unwrap_or_else(|| Session::new(ctx.cwd.clone(), ctx.on.clone()))
    } else {
        Session::new(ctx.cwd.clone(), ctx.on.clone())
    };
    session.maybe_set_title_from(&prompt);

    let local_target = ctx.on == "local" || ctx.on.is_empty();
    let ephemeral = if local_target {
        EphemeralServer::start(&ctx.data_dir).await?
    } else {
        None
    };

    let started = Instant::now();
    let runtime = Runtime::new(ctx.data_dir.clone());
    let live = runtime
        .ensure_with_hooks(
            session.id.clone(),
            ctx.cwd.clone(),
            ctx.on.clone(),
            session.messages.clone(),
        )
        .await;

    let local_socket = ephemeral.as_ref().map(|s| s.socket.clone());
    let mut rx = {
        let mut state = live.lock().unwrap();
        state.local_socket = local_socket.clone();
        state.monitors.set_local_socket(local_socket);
        state.asks = AskBroker::new_headless();
        state.permissions.set_mode(perm_mode);
        state
            .permissions
            .set_skip_permissions(args.dangerously_skip_permissions);
        state.permissions.set_sandbox_override(sandbox_override);
        state.model_override = args.model.clone();
        state.max_steps = Some(args.max_steps.unwrap_or(100));
        state.subscribe()
    };

    runtime.send(&session.id, prompt, SendMode::Prompt);

    let outcome = collect_events(&mut rx, args.output_format, out, err).await;

    let result = match outcome {
        Ok(done) => {
            let final_text = done
                .messages
                .iter()
                .rev()
                .find(|m| m.role == ChatRole::Assistant && !m.content.trim().is_empty())
                .map(|m| m.content.clone())
                .unwrap_or(done.accumulated_text);

            session.messages = done.messages;
            session.input_tokens += done.input_tokens;
            session.output_tokens += done.output_tokens;
            session.prompt_tokens = done.context_tokens;
            session.touch();
            let _ = save_session(&ctx.data_dir, &session);

            if args.output_format == OutputFormat::Json {
                let report = JsonReport {
                    session_id: session.id.clone(),
                    ok: true,
                    result: final_text,
                    error: None,
                    input_tokens: done.input_tokens,
                    output_tokens: done.output_tokens,
                    context_tokens: done.context_tokens,
                    tool_calls: done.tool_calls,
                    duration_ms: started.elapsed().as_millis() as u64,
                };
                writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
                let _ = out.flush();
            }
            Ok(())
        }
        Err(turn_err) => {
            if args.output_format == OutputFormat::Json {
                let report = JsonReport {
                    session_id: session.id.clone(),
                    ok: false,
                    result: String::new(),
                    error: Some(turn_err.clone()),
                    input_tokens: 0,
                    output_tokens: 0,
                    context_tokens: 0,
                    tool_calls: 0,
                    duration_ms: started.elapsed().as_millis() as u64,
                };
                writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
                let _ = out.flush();
            }
            Err(anyhow::anyhow!("{turn_err}"))
        }
    };

    runtime.shutdown_all().await;
    if let Some(srv) = ephemeral {
        srv.shutdown().await;
    }
    result
}

mod output;
use output::collect_events;

#[cfg(test)]
mod tests {
    use super::*;
    use envoy_agent::agent::AgentEvent;
    use envoy_agent::auth::{
        now_ms, record_provider_login, Credential, ProviderProtocol, StoredProvider,
    };
    use envoy_agent::runtime::SeqEvent;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = PathBuf::from("/tmp").join(format!(
            "eh-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    async fn spawn_mock_sse_server(responses: Vec<String>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(responses)));
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 16_384];
                let _ = sock.read(&mut buf).await;
                let body = queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n".to_string());
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        addr
    }

    fn setup_provider(data: &std::path::Path, addr: std::net::SocketAddr) {
        let prov = StoredProvider {
            id: "openai".into(),
            name: "Mock OpenAI".into(),
            protocol: ProviderProtocol::OpenAiChat,
            base_url: format!("http://{addr}/v1"),
            credential: Credential::ApiKey {
                key: "sk-mock".into(),
            },
            custom_models: vec!["gpt-test".into()],
            model_routes: BTreeMap::new(),
            updated_ms: now_ms(),
        };
        record_provider_login(data, prov, Some("gpt-test".into())).unwrap();
    }

    #[tokio::test]
    async fn headless_exec_and_skip_permissions_with_json_and_continue() {
        let root = tmpdir("e2e");
        let data = root.join("data");
        let work = root.join("work");
        let outside = root.join("outside.txt");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&work).unwrap();

        let sse_tool_call = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\"function\":{{\"name\":\"exec\",\"arguments\":{}}}}}]}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":5}}}}\n\ndata: [DONE]\n\n",
            serde_json::to_string(&format!(
                "{{\"command\":\"echo ok > {}\"}}",
                outside.display()
            ))
            .unwrap()
        );
        let sse_final = "data: {\"choices\":[{\"delta\":{\"content\":\"ALL DONE\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n".to_string();
        let sse_turn2 =
            "data: {\"choices\":[{\"delta\":{\"content\":\"CONTINUED\"}}]}\n\ndata: [DONE]\n\n"
                .to_string();

        let addr = spawn_mock_sse_server(vec![sse_tool_call, sse_final, sse_turn2]).await;
        setup_provider(&data, addr);

        let ctx = Ctx {
            data_dir: data.clone(),
            on: "local".into(),
            conduit: None,
            cwd: work.clone(),
        };

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = HeadlessArgs {
            prompt: Some("write outside file".into()),
            dangerously_skip_permissions: true,
            sandbox: Some("off".into()),
            max_steps: Some(10),
            output_format: OutputFormat::Json,
            ..Default::default()
        };
        run_with_writers(&ctx, false, None, args, &mut out, &mut err)
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&outside).unwrap().trim(), "ok");
        let json_out: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json_out["ok"], true);
        assert_eq!(json_out["result"], "ALL DONE");
        assert_eq!(json_out["tool_calls"], 1);
        assert!(
            !daemon::socket_path(&data).exists(),
            "ephemeral socket cleaned up"
        );

        // Verify `-c` continues the saved session cleanly in text mode.
        let mut out2 = Vec::new();
        let mut err2 = Vec::new();
        let args2 = HeadlessArgs {
            prompt: Some("follow up".into()),
            dangerously_skip_permissions: true,
            sandbox: Some("off".into()),
            output_format: OutputFormat::Text,
            ..Default::default()
        };
        run_with_writers(&ctx, true, None, args2, &mut out2, &mut err2)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(out2).unwrap().trim(), "CONTINUED");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn headless_respects_max_steps_and_fails_fast_without_hanging() {
        let root = tmpdir("maxsteps");
        let data = root.join("data");
        let work = root.join("work");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&work).unwrap();

        let sse_tool_loop = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"ls\",\"arguments\":\"{\\\"path\\\":\\\".\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n".to_string();
        let addr = spawn_mock_sse_server(vec![sse_tool_loop.clone(), sse_tool_loop]).await;
        setup_provider(&data, addr);

        let ctx = Ctx {
            data_dir: data.clone(),
            on: "local".into(),
            conduit: None,
            cwd: work,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = HeadlessArgs {
            prompt: Some("loop".into()),
            dangerously_skip_permissions: true,
            sandbox: Some("off".into()),
            max_steps: Some(1),
            output_format: OutputFormat::StreamJson,
            ..Default::default()
        };
        let res = run_with_writers(&ctx, false, None, args, &mut out, &mut err).await;
        assert!(res.is_ok(), "{res:?}");
        let stream = String::from_utf8(out).unwrap();
        assert!(stream.contains("Stopped after 1 tool steps."), "{stream}");
        assert!(!stream.contains("\"TurnError\""), "{stream}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn collect_events_prints_approval_hint_once_on_stderr() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for n in 1..=2 {
            tx.send(SeqEvent {
                seq: n,
                event: AgentEvent::ToolOutput {
                    call_id: format!("c{n}"),
                    text: "tool `exec` requires interactive approval; rejected".into(),
                    err: true,
                },
            })
            .unwrap();
        }
        tx.send(SeqEvent {
            seq: 3,
            event: AgentEvent::TurnDone {
                messages: Vec::new(),
                input_tokens: 1,
                output_tokens: 1,
                context_tokens: 1,
            },
        })
        .unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        collect_events(&mut rx, OutputFormat::Json, &mut out, &mut err)
            .await
            .unwrap();

        assert!(out.is_empty(), "JSON stdout must stay clean");
        let stderr = String::from_utf8(err).unwrap();
        assert_eq!(
            stderr
                .matches("[hint] tool approval required in headless mode")
                .count(),
            1,
            "stderr:\n{stderr}"
        );
    }
}
