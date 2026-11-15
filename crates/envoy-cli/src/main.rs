//! Standalone `envoy` CLI, daemon, and stdio MCP server.
//!
//! Every command talks to the daemon over its Unix socket, so a command
//! survives the terminal that started it: `envoy submit` returns a job id and
//! `envoy logs -f` can reattach from anywhere.

mod daemon;
mod mcp;
mod tui;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use envoy_core::{parse_mode, AgentConfigStore};
use envoy_proto::{
    EntryKind, Job, JobState, SubmitRequest, TaskSpec, AGENT_VERSION, DEFAULT_GROUP,
};
use envoy_service::{dialer_for, AgentDialer, AgentServer, Remote};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(not(unix))]
compile_error!("envoy currently targets Unix: the daemon speaks over a Unix socket");

#[derive(Parser, Debug)]
#[command(
    name = "envoy",
    about = "Envoy — detached remote-execution agent, policy gate, and MCP server"
)]
struct Cli {
    /// Data directory (agent.json, jobs/, envoy.sock).
    #[arg(long, global = true, env = "ENVOY_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Target machine: `local` or a conduit device name/id.
    #[arg(long, global = true, default_value = "local")]
    on: String,

    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show this device's policy, daemon state, and job counts.
    Status,
    /// Allow listed callers to reach the agent over the fabric.
    Enable,
    /// Refuse remote calls (local calls keep working).
    Disable,
    /// Add a caller (device id or name) to this device's allowlist.
    Allow { caller_id: String },
    /// Remove a caller from this device's allowlist.
    Deny { caller_id: String },
    /// Manage the local daemon.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Run the daemon in the foreground.
    Serve {
        #[arg(long)]
        socket: Option<PathBuf>,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let is_mcp = matches!(cli.cmd, Commands::Mcp { .. });
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
    let on = cli.on.clone();

    match cli.cmd {
        Commands::Status => cmd_status(&data_dir).await,
        Commands::Enable => cmd_set_enabled(&data_dir, true),
        Commands::Disable => cmd_set_enabled(&data_dir, false),
        Commands::Allow { caller_id } => cmd_allow(&data_dir, caller_id),
        Commands::Deny { caller_id } => cmd_deny(&data_dir, &caller_id),
        Commands::Daemon { cmd } => cmd_daemon(&data_dir, cmd).await,
        Commands::Serve { socket } => cmd_serve(data_dir, socket).await,
        Commands::Submit(args) => cmd_submit(&data_dir, &on, args.into_request()?).await,
        Commands::Run(args) => cmd_run(&data_dir, &on, args.into_request()?).await,
        Commands::Jobs => cmd_jobs(&data_dir, &on).await,
        Commands::Logs { id, follow, offset } => {
            cmd_logs(&data_dir, &on, id, offset, follow).await
        }
        Commands::Cancel { id } => cmd_cancel(&data_dir, &on, id).await,
        Commands::Read {
            offset,
            limit,
            path,
        } => cmd_read(&data_dir, &on, &path, offset, limit).await,
        Commands::Write { mode, path, content } => {
            cmd_write(&data_dir, &on, &path, content, &mode).await
        }
        Commands::Ls { path } => cmd_ls(&data_dir, &on, &path).await,
        Commands::Copy {
            from,
            from_path,
            to,
            to_path,
            mode,
        } => cmd_copy(&data_dir, &from, &from_path, &to, &to_path, &mode).await,
        Commands::Ui => tui::run(data_dir, on).await,
        Commands::Mcp { cmd } => mcp::cmd_mcp(data_dir, cmd).await,
    }
}

// ---------------------------------------------------------------- plumbing

type AnyDialer = Box<dyn AgentDialer>;

/// Build a client for `on`, starting the local daemon on demand.
async fn remote_for(data_dir: &Path, on: &str) -> Result<Remote<AnyDialer>> {
    if on == "local" || on.is_empty() {
        daemon::ensure(data_dir).await?;
    }
    Ok(Remote::new(dialer_for(data_dir, on)))
}

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

async fn cmd_status(data_dir: &Path) -> Result<()> {
    {
        let store = AgentConfigStore::open(data_dir.to_path_buf())?;
        print_store(&store);
        println!();
        if daemon::is_running(&data_dir).await {
            println!("daemon:   running ({})", daemon::socket_path(&data_dir).display());
            let remote = Remote::new(dialer_for(&data_dir, "local"));
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
            println!("daemon:   stopped ({})", daemon::socket_path(&data_dir).display());
        }
        if daemon::bridge_socket_path(&data_dir).exists() {
            println!(
                "bridge:   socket present ({})",
                daemon::bridge_socket_path(&data_dir).display()
            );
        } else {
            println!("bridge:   not running (start `envoy-bridge`)");
        }
        println!("protocol: v{AGENT_VERSION}");
        Ok(())
    }
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

// ------------------------------------------------------------------- daemon

async fn cmd_daemon(data_dir: &Path, cmd: DaemonCmd) -> Result<()> {
    match cmd {
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

async fn cmd_serve(data_dir: PathBuf, socket: Option<PathBuf>) -> Result<()> {
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
    Ok(())
}

// -------------------------------------------------------------------- jobs

async fn cmd_submit(data_dir: &Path, on: &str, req: SubmitRequest) -> Result<()> {
    let remote = remote_for(data_dir, on).await?;
    let job = remote.submit(on, req).await?;
    println!("{}", job.id);
    eprintln!("queued #{} on {on}: {}", job.id, job.command());
    Ok(())
}

async fn cmd_run(data_dir: &Path, on: &str, req: SubmitRequest) -> Result<()> {
    let remote = remote_for(data_dir, on).await?;
    let job = remote.submit(on, req).await?;
    eprintln!("#{} {} ({on})", job.id, job.command());

    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let (_cursor, finished) = remote
        .logs(on, job.id, 0, true, |records| {
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

async fn cmd_jobs(data_dir: &Path, on: &str) -> Result<()> {
    let remote = remote_for(data_dir, on).await?;
    print_jobs(&remote.jobs(on).await?);
    Ok(())
}

async fn cmd_logs(data_dir: &Path, on: &str, id: u64, offset: u64, follow: bool) -> Result<()> {
    let remote = remote_for(data_dir, on).await?;
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let (cursor, finished) = remote
        .logs(on, id, offset, follow, |records| {
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

async fn cmd_cancel(data_dir: &Path, on: &str, id: u64) -> Result<()> {
    let remote = remote_for(data_dir, on).await?;
    remote.cancel(on, id).await?;
    println!("cancel requested for #{id}");
    Ok(())
}

// -------------------------------------------------------------------- files

async fn cmd_read(
    data_dir: &Path,
    on: &str,
    path: &str,
    offset: u64,
    limit: u64,
) -> Result<()> {
    let remote = remote_for(data_dir, on).await?;
    let (size, mode, data) = remote.read(on, path, offset, limit).await?;
    let mut out = std::io::stdout();
    out.write_all(&data)?;
    out.flush()?;
    eprintln!(
        "{} bytes of {} (mode {:o}) from {on}",
        data.len(),
        size,
        mode & 0o777
    );
    Ok(())
}

async fn cmd_write(
    data_dir: &Path,
    on: &str,
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
    let remote = remote_for(data_dir, on).await?;
    let bytes = remote.write(on, path, &data, mode).await?;
    println!("wrote {bytes} bytes to {path} on {on}");
    Ok(())
}

async fn cmd_ls(data_dir: &Path, on: &str, path: &str) -> Result<()> {
    let remote = remote_for(data_dir, on).await?;
    let entries = remote.list(on, path).await?;
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
    data_dir: &Path,
    from: &str,
    from_path: &str,
    to: &str,
    to_path: &str,
    mode: &Option<String>,
) -> Result<()> {
    let mode = mode_option(mode)?;
    let src = remote_for(data_dir, from).await?;
    let dst = remote_for(data_dir, to).await?;

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
