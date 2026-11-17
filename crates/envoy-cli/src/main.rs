//! Standalone `envoy` CLI, daemon, and stdio MCP server.
//!
//! Every command talks to the daemon over its Unix socket, so a command
//! survives the terminal that started it: `envoy submit` returns a job id and
//! `envoy logs -f` can reattach from anywhere.
//!
//! Remote targets (`--on laptop`) go through `conduit run`'s control socket.
//! envoy never links conduit: it is an HTTP/WebSocket client of that socket.

mod agent;
mod auth;
mod cli_ops;
mod daemon;
mod llm;
pub mod mcp;
pub mod permissions;
mod runtime;
mod session;
pub mod skills;
mod tui;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use cli_ops::{
    cmd_allow, cmd_copy, cmd_deny, cmd_devices, cmd_ls, cmd_models, cmd_read, cmd_set_enabled,
    cmd_status, cmd_write,
};
use envoy_core::{parse_mode, AgentConfigStore};
use envoy_proto::{Job, JobState, SandboxPolicy, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{
    default_control_socket, dialer_for, spawn_accept_pool, AgentDialer, AgentServer, ConduitClient,
    Remote,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(not(unix))]
compile_error!("envoy currently targets Unix: the daemon speaks over a Unix socket");

#[derive(Parser, Debug)]
#[command(
    name = "envoy",
    version,
    about = "Envoy — detached remote-execution agent, policy gate, and MCP server"
)]
struct Cli {
    /// Data directory (agent.json, jobs/, envoy.sock).
    #[arg(long, global = true, env = "ENVOY_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Target machine: `local` or a conduit device name/id/ip.
    #[arg(long, global = true, default_value = "local")]
    on: String,

    /// conduit control socket (default: $CONDUIT_CONTROL_SOCK or
    /// ~/.conduit/control.sock).
    #[arg(long, global = true, env = "CONDUIT_CONTROL_SOCK")]
    conduit_socket: Option<PathBuf>,

    /// Working directory for the TUI and local jobs (default: cwd).
    #[arg(long, global = true, env = "ENVOY_CWD")]
    cwd: Option<PathBuf>,

    /// Continue the most recent session for this directory.
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,

    /// Resume a specific session ID.
    #[arg(short = 's', long = "session")]
    session: Option<String>,

    /// Subcommand; with none, `envoy` opens the terminal console.
    #[command(subcommand)]
    cmd: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show policy, daemon state, conduit status, and job counts.
    Status,
    /// Connect an LLM provider (API key, OAuth, or custom endpoint).
    Login,
    /// Manage LLM provider credentials.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// List available models across connected providers.
    Models,
    /// Allow listed callers to reach the agent over the fabric.
    Enable,
    /// Refuse remote calls (local calls keep working).
    Disable,
    /// Add a caller (device id) to this device's allowlist.
    Allow { caller_id: String },
    /// Remove a caller from this device's allowlist.
    Deny { caller_id: String },
    /// List the devices in this conduit account.
    Devices,
    /// Manage the local daemon.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Run the daemon in the foreground.
    Serve {
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Local use only: do not expose service 16 to conduit.
        #[arg(long, alias = "no-conduit")]
        local_only: bool,
        /// Idle accept connections parked on conduit.
        #[arg(long, default_value_t = envoy_service::DEFAULT_POOL)]
        accept_pool: usize,
    },
    /// Queue a job and print its id (never waits for it).
    Submit(TaskArgs),
    /// Queue a job, follow its output, and exit with its code.
    Run(TaskArgs),
    /// List known jobs.
    Jobs,
    /// Print a job's output (with `--follow`, keep watching).
    Logs {
        id: u64,
        #[arg(long)]
        follow: bool,
        #[arg(long, default_value_t = 0)]
        offset: u64,
    },
    /// Cancel a queued or running job.
    Cancel { id: u64 },
    /// Read bytes from a file (default: up to 4 MiB) to stdout.
    Read {
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 0)]
        limit: u64,
        path: String,
    },
    /// Write a file from an argument or stdin (`-`).
    Write {
        #[arg(long)]
        mode: Option<String>,
        path: String,
        content: Option<String>,
    },
    /// List one directory level.
    Ls {
        #[arg(default_value = ".")]
        path: String,
    },
    /// Copy one file between targets, streaming in pages.
    Copy {
        from: String,
        from_path: String,
        to: String,
        to_path: String,
        #[arg(long)]
        mode: Option<String>,
    },
    /// Terminal console for jobs, output and cancellation.
    Ui,
    /// Model Context Protocol (MCP) server over stdio.
    Mcp {
        #[command(subcommand)]
        cmd: mcp::McpCmd,
    },
}

#[derive(Subcommand, Debug)]
enum AuthCmd {
    /// Connect an LLM provider (API key, OAuth, or custom endpoint).
    Login,
    /// List connected providers and credentials.
    List,
    /// Remove stored credentials for a provider.
    Logout { provider: Option<String> },
}

#[derive(Subcommand, Debug)]
enum DaemonCmd {
    /// Install user-level autostart (launchd / systemd --user).
    Install,
    /// Remove the autostart unit.
    Uninstall,
    /// Start the daemon in the background.
    Start,
    /// Ask the daemon to shut down.
    Stop,
    /// Report whether the daemon is running.
    Status,
}

#[derive(Args, Debug, Clone)]
struct TaskArgs {
    /// Working directory on the target.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Extra environment variables, `K=V` (repeatable).
    #[arg(long = "env")]
    env: Vec<String>,
    /// Free-form label shown in job listings.
    #[arg(long)]
    label: Option<String>,
    /// Concurrency group.
    #[arg(long)]
    group: Option<String>,
    /// Kill the job after this many seconds (0 = agent default).
    #[arg(long, default_value_t = 0)]
    timeout: u64,
    /// Always run through `sh -c`.
    #[arg(long)]
    shell: bool,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    argv: Vec<String>,
}

impl TaskArgs {
    fn into_request(self) -> Result<SubmitRequest> {
        let mut argv = self.argv;
        if argv.first().map(|a| a == "--").unwrap_or(false) {
            argv.remove(0);
        }
        if argv.is_empty() {
            bail!("no command given");
        }
        // A single argument is a command line ("echo hi | wc -l"); several are
        // already split words and run without a shell unless --shell is given.
        let spec = if self.shell || argv.len() == 1 {
            TaskSpec::Shell {
                command: argv.remove(0),
            }
        } else {
            TaskSpec::Argv { argv }
        };
        Ok(SubmitRequest {
            spec,
            cwd: self.cwd.map(|p| p.display().to_string()),
            env: parse_env(&self.env)?,
            label: self.label,
            group: self.group.unwrap_or_else(|| DEFAULT_GROUP.to_string()),
            timeout_s: self.timeout,
            sandbox: SandboxPolicy::Off,
        })
    }
}

/// Everything a command needs to reach a target.
#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) data_dir: PathBuf,
    pub(crate) on: String,
    pub(crate) conduit: Option<PathBuf>,
    pub(crate) cwd: PathBuf,
}

impl Ctx {
    pub(crate) fn conduit_socket(&self) -> PathBuf {
        self.conduit.clone().unwrap_or_else(default_control_socket)
    }

    pub(crate) async fn remote(&self) -> Result<Remote<AnyDialer>> {
        if self.on == "local" || self.on.is_empty() {
            daemon::ensure(&self.data_dir).await?;
        }
        Ok(Remote::new(dialer_for(
            &self.data_dir,
            &self.on,
            Some(self.conduit_socket()),
        )))
    }

    pub(crate) fn remote_for(&self, target: &str) -> Remote<AnyDialer> {
        Remote::new(dialer_for(
            &self.data_dir,
            target,
            Some(self.conduit_socket()),
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let is_mcp = matches!(cli.cmd.as_ref(), Some(Commands::Mcp { .. }));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("envoy=info,envoy_service=info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if is_mcp {
        // MCP speaks JSON-RPC on stdout; logs must stay on stderr.
        builder.with_writer(std::io::stderr).init();
    } else {
        builder.init();
    }

    let data_dir = cli
        .data_dir
        .unwrap_or_else(AgentConfigStore::default_data_dir);
    let cwd = cli
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let ctx = Ctx {
        data_dir,
        on: cli.on.clone(),
        conduit: cli.conduit_socket.clone(),
        cwd,
    };

    let Some(command) = cli.cmd else {
        // Bare `envoy` is the opencode-style entry point: open the console in
        // the current directory.
        return tui::run(
            ctx.data_dir.clone(),
            ctx.on.clone(),
            ctx.conduit.clone(),
            ctx.cwd.clone(),
            cli.continue_last,
            cli.session.clone(),
            cli.cwd.is_none() && !cli.continue_last && cli.session.is_none(),
        )
        .await;
    };

    match command {
        Commands::Status => cmd_status(&ctx).await,
        Commands::Login => auth::cmd_auth_login(&ctx.data_dir).await,
        Commands::Auth { cmd } => match cmd {
            AuthCmd::Login => auth::cmd_auth_login(&ctx.data_dir).await,
            AuthCmd::List => auth::cmd_auth_list(&ctx.data_dir).await,
            AuthCmd::Logout { provider } => auth::cmd_auth_logout(&ctx.data_dir, provider).await,
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
                ctx.conduit_socket(),
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
                ctx.data_dir.clone(),
                ctx.on.clone(),
                ctx.conduit.clone(),
                ctx.cwd.clone(),
                cli.continue_last,
                cli.session.clone(),
                cli.cwd.is_none() && !cli.continue_last && cli.session.is_none(),
            )
            .await
        }
        Commands::Mcp { cmd } => mcp::cmd_mcp(ctx.data_dir.clone(), cmd, ctx.conduit.clone()).await,
    }
}

// ---------------------------------------------------------------- plumbing

type AnyDialer = Box<dyn AgentDialer>;

fn parse_env(pairs: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .with_context(|| format!("env `{p}` is not K=V"))?;
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

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
        "{:>4}  {:<10} {:<10} {:<10} {:>10}  {}",
        "ID", "STATE", "GROUP", "OWNER", "LOG", "COMMAND"
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
            let exe = std::env::current_exe().context("locating the envoy binary")?;
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
    conduit_socket: PathBuf,
) -> Result<()> {
    // Serialize instances before touching the socket: a second daemon would
    // otherwise unlink the live one and share the job store with it.
    let _lock = daemon::InstanceLock::acquire(&data_dir)?;
    let path = socket.unwrap_or_else(|| daemon::socket_path(&data_dir));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::create_dir_all(&data_dir)?;
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("binding {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::write(daemon::pid_file(&data_dir), std::process::id().to_string()).ok();

    // Console control plane: agent turns live here, not in the TUI.
    let agent_path = runtime::socket_path(&data_dir);
    let _ = std::fs::remove_file(&agent_path);
    let agent_listener = tokio::net::UnixListener::bind(&agent_path)
        .with_context(|| format!("binding {}", agent_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&agent_path, std::fs::Permissions::from_mode(0o600));
    }
    let sessions = runtime::Runtime::new(data_dir.clone());
    tokio::spawn(runtime::serve(sessions, agent_listener));

    let server = AgentServer::open_store(data_dir.clone())?;
    tracing::info!(socket = %path.display(), "envoy daemon listening");
    println!("envoy daemon listening on {}", path.display());

    let mut pool = None;
    if local_only {
        println!("conduit: disabled (--local-only)");
    } else {
        println!(
            "conduit: watching {} ({} accept slot(s))",
            conduit_socket.display(),
            accept_pool
        );
        pool = Some(spawn_accept_pool(
            server.clone(),
            ConduitClient::new(conduit_socket),
            accept_pool,
        ));
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let pid_file = daemon::pid_file(&data_dir);
    daemon::run_server(server, listener, path, Some(pid_file), async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
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
