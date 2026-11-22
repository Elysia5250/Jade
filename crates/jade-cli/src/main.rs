//! Standalone `jade` CLI, daemon, and stdio MCP server.
//!
//! Every command talks to the daemon over its Unix socket, so a command
//! survives the terminal that started it: `jade submit` returns a job id and
//! `jade logs -f` can reattach from anywhere.
//!
//! Remote targets (`--on laptop`) go through `topaz run`'s control socket.
//! jade never links topaz: it is an HTTP/WebSocket client of that socket.

mod auth_cli;
mod cli_ops;
mod headless;
mod mcp_cli;
mod plugin_cli;
mod tui;

mod cli_args;
pub(crate) use cli_args::{AuthCmd, Commands, DaemonCmd};

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser};
use cli_ops::{
    cmd_allow, cmd_copy, cmd_deny, cmd_devices, cmd_ls, cmd_models, cmd_read, cmd_set_enabled,
    cmd_status, cmd_write,
};
use jade_agent::{daemon, runtime};
use jade_core::{parse_mode, AgentConfigStore};
use jade_proto::{Job, JobState, SubmitRequest};
use jade_service::{
    default_control_socket, dialer_for, spawn_accept_pool, AgentDialer, AgentServer, Remote,
    TopazClient,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(not(unix))]
compile_error!("jade currently targets Unix: the daemon speaks over a Unix socket");

#[derive(Parser, Debug)]
#[command(
    name = "jade",
    version,
    about = "Jade — daemon-owned coding agent, policy gate, and MCP server"
)]
struct Cli {
    /// Data directory (agent.json, jobs/, jade.sock).
    #[arg(long, global = true, env = "JADE_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Target machine: `local` or a topaz device name/id/ip.
    #[arg(long, global = true, default_value = "local")]
    on: String,

    /// topaz control socket (default: $TOPAZ_CONTROL_SOCK or
    /// ~/.topaz/control.sock).
    #[arg(long, global = true, env = "TOPAZ_CONTROL_SOCK")]
    topaz_socket: Option<PathBuf>,

    /// Working directory for the TUI and local jobs (default: cwd).
    #[arg(long, global = true, env = "JADE_CWD")]
    cwd: Option<PathBuf>,

    /// Continue the most recent session for this directory.
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,

    /// Start a fresh session instead of resuming the directory's latest session.
    #[arg(long = "new", global = true)]
    new_session: bool,

    /// Resume a specific session ID.
    #[arg(short = 's', long = "session")]
    session: Option<String>,

    #[command(flatten)]
    headless: headless::HeadlessArgs,

    /// Subcommand; with none, `jade` opens the terminal console.
    #[command(subcommand)]
    cmd: Option<Commands>,
}

/// Everything a command needs to reach a target.
#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) data_dir: PathBuf,
    pub(crate) on: String,
    pub(crate) topaz: Option<PathBuf>,
    pub(crate) cwd: PathBuf,
}

impl Ctx {
    pub(crate) fn topaz_socket(&self) -> PathBuf {
        self.topaz.clone().unwrap_or_else(default_control_socket)
    }

    pub(crate) async fn remote(&self) -> Result<Remote<AnyDialer>> {
        if self.on == "local" || self.on.is_empty() {
            daemon::ensure(&self.data_dir).await?;
        }
        Ok(Remote::new(dialer_for(
            &self.data_dir,
            &self.on,
            Some(self.topaz_socket()),
        )))
    }

    pub(crate) fn remote_for(&self, target: &str) -> Remote<AnyDialer> {
        Remote::new(dialer_for(
            &self.data_dir,
            target,
            Some(self.topaz_socket()),
        ))
    }

    /// Like [`Ctx::remote_for`], but starts the local daemon when the target is
    /// this machine (used by commands that may touch both ends).
    pub(crate) async fn remote_to(&self, target: &str) -> Result<Remote<AnyDialer>> {
        if target == "local" || target.is_empty() {
            daemon::ensure(&self.data_dir).await?;
        }
        Ok(self.remote_for(target))
    }
}

fn tui_unsupported_flag(
    args: &headless::HeadlessArgs,
    matches: &clap::ArgMatches,
) -> Option<&'static str> {
    let model_from_cli =
        matches.value_source("model") == Some(clap::parser::ValueSource::CommandLine);
    tui_unsupported_flag_with(args, model_from_cli)
}

fn tui_unsupported_flag_with(
    args: &headless::HeadlessArgs,
    model_from_cli: bool,
) -> Option<&'static str> {
    if args.model.is_some() && model_from_cli {
        return Some("--model");
    }
    if args.permission_mode.is_some() {
        return Some("--permission-mode");
    }
    if args.sandbox.is_some() {
        return Some("--sandbox");
    }
    if args.max_steps.is_some() {
        return Some("--max-steps");
    }
    if args.dangerously_skip_permissions {
        return Some("--dangerously-skip-permissions");
    }
    if args.output_format != headless::OutputFormat::Text {
        return Some("--output-format");
    }
    None
}

fn init_tracing(stderr_logs: bool, tui_log: Option<&Path>) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("jade=info,jade_service=info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if let Some(path) = tui_log {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let path = std::sync::Arc::new(path.to_path_buf());
        builder
            .with_ansi(false)
            .with_writer(move || -> Box<dyn std::io::Write + Send> {
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path.as_ref())
                {
                    Ok(file) => Box::new(file),
                    Err(_) => Box::new(std::io::sink()),
                }
            })
            .init();
    } else if stderr_logs {
        // MCP, `ask`, and `-p`/piped output write clean responses/JSON on stdout; logs stay on stderr.
        builder.with_writer(std::io::stderr).with_ansi(false).init();
    } else {
        builder.init();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    let is_mcp = matches!(cli.cmd.as_ref(), Some(Commands::Mcp { .. }));
    let piped_stdin = cli.cmd.is_none() && !std::io::IsTerminal::is_terminal(&std::io::stdin());
    let is_ask = matches!(cli.cmd.as_ref(), Some(Commands::Ask { .. }));
    let is_headless = cli.headless.is_active() || piped_stdin || is_ask;
    let opening_tui = match cli.cmd.as_ref() {
        None => !is_headless,
        Some(Commands::Ui) => !cli.headless.is_active(),
        Some(_) => false,
    };
    if opening_tui {
        if let Some(flag) = tui_unsupported_flag(&cli.headless, &matches) {
            bail!(
                "{flag} applies to headless -p/--print runs and is rejected when opening the TUI"
            );
        }
    }
    let data_dir = cli
        .data_dir
        .clone()
        .unwrap_or_else(AgentConfigStore::default_data_dir);
    let tui_log = opening_tui.then(|| data_dir.join("logs").join("tui.log"));
    init_tracing(is_mcp || is_headless, tui_log.as_deref());
    let cwd = cli
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let ctx = Ctx {
        data_dir,
        on: cli.on.clone(),
        topaz: cli.topaz_socket.clone(),
        cwd,
    };
    let pick_workspace =
        cli.cwd.is_none() && !cli.continue_last && cli.session.is_none() && !cli.new_session;
    let cont = cli.continue_last && !cli.new_session;

    let Some(command) = cli.cmd else {
        if is_headless {
            let h = headless::piped_headless_args(cli.headless);
            return headless::run(&ctx, cont, cli.session, h).await;
        }
        return tui::run(
            ctx.data_dir,
            ctx.on,
            ctx.topaz,
            ctx.cwd,
            cli.continue_last,
            cli.session,
            pick_workspace,
            cli.new_session,
        )
        .await;
    };
    if cli.headless.is_active() {
        bail!("--print / -p cannot be combined with a subcommand");
    }

    match command {
        Commands::Ask { prompt } => {
            headless::run_ask(&ctx, cont, cli.session, cli.headless, prompt).await
        }
        Commands::Status => cmd_status(&ctx).await,
        Commands::Login => auth_cli::cmd_auth_login(&ctx.data_dir).await,
        Commands::Auth { cmd } => match cmd {
            AuthCmd::Login => auth_cli::cmd_auth_login(&ctx.data_dir).await,
            AuthCmd::Add {
                id,
                base_url,
                protocol,
                api_key,
                models,
                headers,
            } => auth_cli::cmd_auth_add(
                &ctx.data_dir,
                id,
                base_url,
                protocol,
                api_key,
                models,
                headers,
            ),
            AuthCmd::List => auth_cli::cmd_auth_list(&ctx.data_dir).await,
            AuthCmd::Logout { provider } => {
                auth_cli::cmd_auth_logout(&ctx.data_dir, provider).await
            }
        },
        Commands::Models => cmd_models(&ctx.data_dir).await,
        Commands::Enable => cmd_set_enabled(&ctx.data_dir, true),
        Commands::Disable => cmd_set_enabled(&ctx.data_dir, false),
        Commands::Allow { caller_id } => cmd_allow(&ctx.data_dir, caller_id),
        Commands::Deny { caller_id } => cmd_deny(&ctx.data_dir, &caller_id),
        Commands::Devices => cmd_devices(&ctx).await,
        Commands::Daemon { cmd } => cmd_daemon(&ctx.data_dir, cmd).await,
        Commands::Serve {
            socket,
            local_only,
            accept_pool,
        } => {
            cmd_serve(
                ctx.data_dir.clone(),
                socket,
                local_only,
                accept_pool,
                ctx.topaz_socket(),
            )
            .await
        }
        Commands::Submit(args) => cmd_submit(&ctx, args.into_request()?).await,
        Commands::Run(args) => cmd_run(&ctx, args.into_request()?).await,
        Commands::Jobs => cmd_jobs(&ctx).await,
        Commands::Logs { id, follow, offset } => cmd_logs(&ctx, id, offset, follow).await,
        Commands::Cancel { id } => cmd_cancel(&ctx, id).await,
        Commands::Read {
            offset,
            limit,
            path,
        } => cmd_read(&ctx, &path, offset, limit).await,
        Commands::Write {
            mode,
            path,
            content,
        } => cmd_write(&ctx, &path, content, &mode).await,
        Commands::Ls { path } => cmd_ls(&ctx, &path).await,
        Commands::Copy {
            from,
            from_path,
            to,
            to_path,
            mode,
        } => cmd_copy(&ctx, &from, &from_path, &to, &to_path, &mode).await,
        Commands::Ui => {
            tui::run(
                ctx.data_dir,
                ctx.on,
                ctx.topaz,
                ctx.cwd,
                cli.continue_last,
                cli.session,
                pick_workspace,
                cli.new_session,
            )
            .await
        }
        Commands::Mcp { cmd } => {
            mcp_cli::cmd_mcp(ctx.data_dir.clone(), cmd, ctx.topaz.clone()).await
        }
        Commands::Plugin { cmd } => plugin_cli::cmd_plugin(&ctx.data_dir, &ctx.cwd, cmd),
    }
}

// ---------------------------------------------------------------- plumbing

type AnyDialer = Box<dyn AgentDialer>;

pub(crate) fn mode_option(mode: &Option<String>) -> Result<Option<u32>> {
    match mode {
        None => Ok(None),
        Some(m) => Ok(Some(parse_mode(m)?)),
    }
}

fn state_cell(job: &Job) -> String {
    match &job.state {
        JobState::Queued { .. } => "queued".into(),
        JobState::Running { .. } => "running".into(),
        JobState::Done { result, .. } => format!("done({})", result.code()),
    }
}

fn print_jobs(jobs: &[Job]) {
    if jobs.is_empty() {
        println!("no jobs");
        return;
    }
    println!(
        "{:>4}  {:<10} {:<10} {:<10} {:>10}  COMMAND",
        "ID", "STATE", "GROUP", "OWNER", "LOG"
    );
    for job in jobs {
        println!(
            "{:>4}  {:<10} {:<10} {:<10} {:>10}  {}",
            job.id,
            state_cell(job),
            job.group,
            job.owner,
            format!("{}B", job.log_bytes),
            job.command()
        );
    }
}

// ------------------------------------------------------------------- daemon

async fn cmd_daemon(data_dir: &Path, cmd: DaemonCmd) -> Result<()> {
    match cmd {
        DaemonCmd::Install => {
            let exe = std::env::current_exe().context("locating the jade binary")?;
            println!("{}", daemon::install(&exe, data_dir)?);
            Ok(())
        }
        DaemonCmd::Uninstall => {
            println!("{}", daemon::uninstall()?);
            Ok(())
        }
        DaemonCmd::Start => {
            if daemon::is_running(data_dir).await {
                println!("daemon already running");
                return Ok(());
            }
            daemon::ensure(data_dir).await?;
            println!(
                "daemon started ({})",
                daemon::socket_path(data_dir).display()
            );
            Ok(())
        }
        DaemonCmd::Stop => {
            if !daemon::is_running(data_dir).await {
                println!("daemon not running");
                return Ok(());
            }
            if !daemon::stop(data_dir)? {
                bail!("no pid file; stop the daemon manually");
            }
            for _ in 0..60 {
                if !daemon::is_running(data_dir).await {
                    println!("daemon stopped");
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            bail!("the daemon ignored SIGTERM for 3s");
        }
        DaemonCmd::Status => {
            println!("autostart: {}", daemon::autostart_line());
            if daemon::is_running(data_dir).await {
                println!("running");
            } else {
                println!("stopped");
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

async fn cmd_serve(
    data_dir: PathBuf,
    socket: Option<PathBuf>,
    local_only: bool,
    accept_pool: usize,
    topaz_socket: PathBuf,
) -> Result<()> {
    // Serialize instances before touching the socket: a second daemon would
    // otherwise unlink the live one and share the job store with it.
    jade_core::fs::harden_process_dumpable();
    jade_core::fs::ensure_private_dir(&data_dir)?;
    let _lock = daemon::InstanceLock::acquire(&data_dir)?;

    // Share the in-process AgentServer with Runtime so local tool calls and
    // monitors never self-dial `jade.sock`.
    let server = AgentServer::open_store(data_dir.clone())?;

    // agent.sock must accept before the job socket and pid file advertise readiness.
    let agent_path = runtime::socket_path(&data_dir);
    let _ = std::fs::remove_file(&agent_path);
    let agent_listener = tokio::net::UnixListener::bind(&agent_path)
        .with_context(|| format!("binding {}", agent_path.display()))?;
    jade_core::fs::set_mode_strict(&agent_path, 0o600)?;
    let sessions = runtime::Runtime::with_local_server(
        data_dir.clone(),
        Some(topaz_socket.clone()),
        Some(server.clone()),
    );
    tokio::spawn(runtime::serve(sessions.clone(), agent_listener));

    let path = socket.unwrap_or_else(|| daemon::socket_path(&data_dir));
    if let Some(parent) = path.parent() {
        if parent == data_dir || parent.starts_with(&data_dir) {
            jade_core::fs::ensure_private_dir(parent)?;
        } else {
            std::fs::create_dir_all(parent)?;
        }
    }
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("binding {}", path.display()))?;
    jade_core::fs::set_mode_strict(&path, 0o600)?;
    jade_core::fs::write_atomic_sync(
        &daemon::pid_file(&data_dir),
        std::process::id().to_string().as_bytes(),
        0o600,
    )?;

    tracing::info!(socket = %path.display(), "jade daemon listening");
    println!("jade daemon listening on {}", path.display());

    let mut pool = None;
    if local_only {
        println!("topaz: disabled (--local-only)");
    } else {
        println!(
            "topaz: watching {} ({} accept slot(s))",
            topaz_socket.display(),
            accept_pool
        );
        pool = Some(spawn_accept_pool(
            server.clone(),
            TopazClient::new(topaz_socket),
            accept_pool,
        ));
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let pid_file = daemon::pid_file(&data_dir);
    let sessions_shutdown = sessions.clone();
    daemon::run_server(server, listener, path, Some(pid_file), async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        sessions_shutdown.shutdown_all().await;
    })
    .await;
    if let Some(pool) = pool {
        pool.abort();
    }
    let _ = std::fs::remove_file(runtime::socket_path(&data_dir));
    Ok(())
}

// -------------------------------------------------------------------- jobs

async fn cmd_submit(ctx: &Ctx, mut req: SubmitRequest) -> Result<()> {
    // A local agent should behave like the shell you are standing in.
    if req.cwd.is_none() && ctx.on == "local" {
        req.cwd = Some(ctx.cwd.display().to_string());
    }
    let remote = ctx.remote().await?;
    let job = remote.submit(&ctx.on, req).await?;
    println!("{}", job.id);
    eprintln!("queued #{} on {}: {}", job.id, ctx.on, job.command());
    Ok(())
}

async fn cmd_run(ctx: &Ctx, mut req: SubmitRequest) -> Result<()> {
    if req.cwd.is_none() && ctx.on == "local" {
        req.cwd = Some(ctx.cwd.display().to_string());
    }
    let remote = ctx.remote().await?;
    let job = remote.submit(&ctx.on, req).await?;
    eprintln!("#{} {} ({})", job.id, job.command(), ctx.on);

    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let (_cursor, finished) = remote
        .logs(&ctx.on, job.id, 0, true, |records| {
            for r in records {
                if r.fd == 1 {
                    let _ = err.write_all(&r.data);
                } else {
                    let _ = out.write_all(&r.data);
                }
            }
        })
        .await?;
    let _ = out.flush();
    let _ = err.flush();
    eprintln!("exit={}", finished.unwrap_or(1));
    match finished.unwrap_or(1) {
        0 => Ok(()),
        n => std::process::exit(n.clamp(1, 125)),
    }
}

async fn cmd_jobs(ctx: &Ctx) -> Result<()> {
    let remote = ctx.remote().await?;
    print_jobs(&remote.jobs(&ctx.on).await?);
    Ok(())
}

async fn cmd_logs(ctx: &Ctx, id: u64, offset: u64, follow: bool) -> Result<()> {
    let remote = ctx.remote().await?;
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let (cursor, finished) = remote
        .logs(&ctx.on, id, offset, follow, |records| {
            for r in records {
                if r.fd == 1 {
                    let _ = err.write_all(&r.data);
                } else {
                    let _ = out.write_all(&r.data);
                }
            }
        })
        .await?;
    let _ = out.flush();
    let _ = err.flush();
    if let Some(code) = finished {
        eprintln!("[#{} exit {code}]", id);
    } else {
        eprintln!("[#{} resumed at {cursor}; not following]", id);
    }
    Ok(())
}

async fn cmd_cancel(ctx: &Ctx, id: u64) -> Result<()> {
    let remote = ctx.remote().await?;
    remote.cancel(&ctx.on, id).await?;
    println!("cancel requested for #{id}");
    Ok(())
}

#[cfg(test)]
mod cli_tests;
