//! Standalone `envoy` CLI, daemon, and stdio MCP server.
//!
//! Every command talks to the daemon over its Unix socket, so a command
//! survives the terminal that started it: `envoy submit` returns a job id and
//! `envoy logs -f` can reattach from anywhere.
//!
//! Remote targets (`--on laptop`) go through `conduit run`'s control socket.
//! envoy never links conduit: it is an HTTP/WebSocket client of that socket.

mod daemon;
mod mcp;
mod tui;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use envoy_core::{parse_mode, AgentConfigStore};
use envoy_proto::{
    EntryKind, Job, JobState, SubmitRequest, TaskSpec, AGENT_VERSION, DEFAULT_GROUP,
};
use envoy_service::{
    default_control_socket, dialer_for, spawn_accept_pool, AgentDialer, AgentServer, ConduitClient,
    Remote,
};
use std::io::{Read, Write};
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

    /// Subcommand; with none, `envoy` opens the terminal console.
    #[command(subcommand)]
    cmd: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show policy, daemon state, conduit status, and job counts.
    Status,
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
    cwd: Option<String>,
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
            cwd: self.cwd,
            env: parse_env(&self.env)?,
            label: self.label,
            group: self.group.unwrap_or_else(|| DEFAULT_GROUP.to_string()),
            timeout_s: self.timeout,
        })
    }
}

/// Everything a command needs to reach a target.
#[derive(Clone)]
struct Ctx {
    data_dir: PathBuf,
    on: String,
    conduit: Option<PathBuf>,
    cwd: PathBuf,
}

impl Ctx {
    fn conduit_socket(&self) -> PathBuf {
        self.conduit
            .clone()
            .unwrap_or_else(default_control_socket)
    }

    async fn remote(&self) -> Result<Remote<AnyDialer>> {
        if self.on == "local" || self.on.is_empty() {
            daemon::ensure(&self.data_dir).await?;
        }
        Ok(Remote::new(dialer_for(
            &self.data_dir,
            &self.on,
            Some(self.conduit_socket()),
        )))
    }

    fn remote_for(&self, target: &str) -> Remote<AnyDialer> {
        Remote::new(dialer_for(
            &self.data_dir,
            target,
            Some(self.conduit_socket()),
        ))
    }

    /// Like [`Ctx::remote_for`], but starts the local daemon when the target is
    /// this machine (used by commands that may touch both ends).
    async fn remote_to(&self, target: &str) -> Result<Remote<AnyDialer>> {
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
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("envoy_cli=info,envoy_service=info"));
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
        )
        .await;
    };

    match command {
        Commands::Status => cmd_status(&ctx).await,
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
        Commands::Write { mode, path, content } => {
            cmd_write(&ctx, &path, content, &mode).await
        }
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

fn mode_option(mode: &Option<String>) -> Result<Option<u32>> {
    match mode {
        None => Ok(None),
        Some(m) => Ok(Some(parse_mode(m)?)),
    }
}

fn print_store(store: &AgentConfigStore) {
    println!("data_dir: {}", store.root.display());
    println!(
        "enabled:  {}",
        if store.config.enabled {
            "yes"
        } else {
            "no — remote calls are refused"
        }
    );
    println!("max_jobs: {}", store.config.max_jobs);
    if store.config.allow.is_empty() {
        println!("allow:    (none)");
    } else {
        println!("allow:");
        for a in &store.config.allow {
            println!("          {a}");
        }
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

// ------------------------------------------------------------------- policy

async fn cmd_status(ctx: &Ctx) -> Result<()> {
    let store = AgentConfigStore::open(ctx.data_dir.clone())?;
    print_store(&store);
    println!();
    if daemon::is_running(&ctx.data_dir).await {
        println!(
            "daemon:   running ({})",
            daemon::socket_path(&ctx.data_dir).display()
        );
        let remote = ctx.remote_for("local");
        match remote.jobs("local").await {
            Ok(jobs) => {
                let running = jobs
                    .iter()
                    .filter(|j| matches!(j.state, JobState::Running { .. }))
                    .count();
                let queued = jobs
                    .iter()
                    .filter(|j| matches!(j.state, JobState::Queued { .. }))
                    .count();
                println!(
                    "jobs:     {} known, {running} running, {queued} queued",
                    jobs.len()
                );
            }
            Err(e) => println!("jobs:     unavailable ({e:#})"),
        }
    } else {
        println!(
            "daemon:   stopped ({})",
            daemon::socket_path(&ctx.data_dir).display()
        );
    }

    let conduit = ConduitClient::new(ctx.conduit_socket());
    match conduit.status().await {
        Ok(status) => {
            println!(
                "conduit:  connected ({}), this device {} ({})",
                conduit.socket().display(),
                status.device_name,
                status.device_id
            );
            println!(
                "network:  {} · tun {} · {} peer(s) · vip {}",
                status.connection.as_deref().unwrap_or("unknown"),
                if status.tun.enabled {
                    status.tun.interface.as_deref().unwrap_or("up")
                } else {
                    "off"
                },
                status.peers.len(),
                status.virtual_ip.as_deref().unwrap_or("-")
            );
            match status
                .services
                .iter()
                .find(|s| s.service_id == envoy_proto::AGENT_SERVICE_ID)
            {
                Some(service) => println!(
                    "service:  {} online={} listeners={}",
                    service.name, service.online, service.listeners
                ),
                None => println!("service:  envoy not registered (start `envoy serve`)"),
            }
        }
        Err(_) => println!(
            "conduit:  not running ({})",
            conduit.socket().display()
        ),
    }
    println!("protocol: v{AGENT_VERSION}");
    Ok(())
}

fn cmd_set_enabled(data_dir: &Path, enabled: bool) -> Result<()> {
    let mut store = AgentConfigStore::open(data_dir.to_path_buf())?;
    store.set_enabled(enabled)?;
    print_store(&store);
    Ok(())
}

fn cmd_allow(data_dir: &Path, caller_id: String) -> Result<()> {
    let mut store = AgentConfigStore::open(data_dir.to_path_buf())?;
    store.allow(caller_id)?;
    print_store(&store);
    Ok(())
}

fn cmd_deny(data_dir: &Path, caller_id: &str) -> Result<()> {
    let mut store = AgentConfigStore::open(data_dir.to_path_buf())?;
    store.deny(caller_id)?;
    print_store(&store);
    Ok(())
}

async fn cmd_devices(ctx: &Ctx) -> Result<()> {
    let client = ConduitClient::new(ctx.conduit_socket());
    let reply = client.devices().await?;
    if reply.devices.is_empty() {
        println!("no devices in this account");
        return Ok(());
    }
    // Online is "has a live session"; agent is "advertises service 16".
    let live: Vec<(String, bool)> = client
        .status()
        .await
        .map(|status| {
            status
                .peers
                .into_iter()
                .map(|peer| {
                    let agent = peer.has_service(envoy_proto::AGENT_SERVICE_ID);
                    (peer.device_id, agent)
                })
                .collect()
        })
        .unwrap_or_default();

    println!(
        "{:<20} {:<16} {:<16} {:<14} {:<8} {:<8} {}",
        "ID", "NAME", "DNS", "VIP", "STATUS", "AGENT", "ME"
    );
    for device in reply.devices {
        let agent = match live.iter().find(|(id, _)| id == &device.device_id) {
            Some((_, true)) => "yes",
            Some((_, false)) => "no",
            None => "offline",
        };
        println!(
            "{:<20} {:<16} {:<16} {:<14} {:<8} {:<8} {}",
            device.device_id,
            device.name,
            device.dns_name.unwrap_or_else(|| "-".into()),
            device.virtual_ip.unwrap_or_else(|| "-".into()),
            device.status,
            agent,
            if device.me { "yes" } else { "" }
        );
    }
    Ok(())
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
            println!("daemon started ({})", daemon::socket_path(data_dir).display());
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

// -------------------------------------------------------------------- files

async fn cmd_read(ctx: &Ctx, path: &str, offset: u64, limit: u64) -> Result<()> {
    let remote = ctx.remote().await?;
    let (size, mode, data) = remote.read(&ctx.on, path, offset, limit).await?;
    let mut out = std::io::stdout();
    out.write_all(&data)?;
    out.flush()?;
    eprintln!(
        "{} bytes of {} (mode {:o}) from {}",
        data.len(),
        size,
        mode & 0o777,
        ctx.on
    );
    Ok(())
}

async fn cmd_write(
    ctx: &Ctx,
    path: &str,
    content: Option<String>,
    mode: &Option<String>,
) -> Result<()> {
    let mode = mode_option(mode)?;
    let data = match content.as_deref() {
        Some("-") | None => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            buf
        }
        Some(text) => text.as_bytes().to_vec(),
    };
    let remote = ctx.remote().await?;
    let bytes = remote.write(&ctx.on, path, &data, mode).await?;
    println!("wrote {bytes} bytes to {path} on {}", ctx.on);
    Ok(())
}

async fn cmd_ls(ctx: &Ctx, path: &str) -> Result<()> {
    let remote = ctx.remote().await?;
    let entries = remote.list(&ctx.on, path).await?;
    println!("{:<8} {:>10} {:<5} NAME", "KIND", "SIZE", "MODE");
    for e in entries {
        let kind = match e.kind {
            EntryKind::File => "file",
            EntryKind::Dir => "dir",
            EntryKind::Symlink => "symlink",
            EntryKind::Other => "other",
        };
        println!("{:<8} {:>10} {:<5o} {}", kind, e.size, e.mode & 0o777, e.name);
    }
    Ok(())
}

/// Page size for `copy`: the read limit, so one page per round trip.
const COPY_PAGE: u64 = envoy_proto::READ_LIMIT;

async fn cmd_copy(
    ctx: &Ctx,
    from: &str,
    from_path: &str,
    to: &str,
    to_path: &str,
    mode: &Option<String>,
) -> Result<()> {
    let mode = mode_option(mode)?;
    let src = ctx.remote_to(from).await?;
    let dst = ctx.remote_to(to).await?;

    let (_, src_mode, first) = src.read(from, from_path, 0, COPY_PAGE).await?;
    let mut writer = dst
        .open_write(to, to_path, mode.or(Some(src_mode)))
        .await?;
    let mut total = 0u64;
    let mut page = first;
    let mut offset = 0u64;
    loop {
        total += page.len() as u64;
        writer.send(&page).await?;
        offset += page.len() as u64;
        if (page.len() as u64) < COPY_PAGE {
            break;
        }
        let (_, _, next) = src.read(from, from_path, offset, COPY_PAGE).await?;
        if next.is_empty() {
            break;
        }
        page = next;
    }
    writer.finish().await?;
    println!("copied {total} bytes: {from}:{from_path} -> {to}:{to_path}");
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn bare_envoy_opens_the_console() {
        let cli = Cli::try_parse_from(["envoy"]).expect("parse");
        assert!(cli.cmd.is_none(), "no subcommand should mean the TUI");
    }

    #[test]
    fn subcommands_still_parse() {
        let cli = Cli::try_parse_from(["envoy", "jobs"]).expect("parse");
        assert!(matches!(cli.cmd, Some(Commands::Jobs)));
        let cli = Cli::try_parse_from(["envoy", "--on", "laptop", "run", "echo hi"]).expect("parse");
        assert!(matches!(cli.cmd, Some(Commands::Run(_))));
        assert_eq!(cli.on, "laptop");
    }

    #[test]
    fn cwd_flag_is_global() {
        let cli = Cli::try_parse_from(["envoy", "--cwd", "/tmp/x", "jobs"]).expect("parse");
        assert_eq!(cli.cwd.as_deref(), Some(std::path::Path::new("/tmp/x")));
    }
}
