//! Standalone `envoy` CLI and stdio MCP server.

mod mcp;

use envoy_core::{mode_of, resolve, set_mode, AgentConfigStore, Runner};
use envoy_proto::{AgentEvent, EntryKind};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "envoy",
    about = "Envoy — standalone local runner, policy config, and MCP server"
)]
struct Cli {
    /// Data directory (agent.json, agent/audit.log, agent/jobs).
    #[arg(long, global = true, env = "ENVOY_DATA_DIR")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show this device's agent policy and allowlist.
    Status,
    /// Allow listed callers to run tools through the wire server.
    Enable,
    /// Refuse remote calls through the wire server.
    Disable,
    /// Add a caller id to this device's allowlist.
    Allow { caller_id: String },
    /// Remove a caller id from this device's allowlist.
    Deny { caller_id: String },
    /// Run a command; stdout/stderr stream live.
    Run {
        #[arg(long, default_value = "local")]
        on: String,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value_t = 0)]
        timeout: u64,
        #[arg(long = "env")]
        env: Vec<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
    },
    /// Read bytes from a file (default: up to 4 MiB) to stdout.
    Read {
        #[arg(long, default_value = "local")]
        on: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 0)]
        limit: u64,
        path: String,
    },
    /// Write a file from an argument or stdin (`-`).
    Write {
        #[arg(long, default_value = "local")]
        on: String,
        #[arg(long)]
        mode: Option<String>,
        path: String,
        content: Option<String>,
    },
    /// List one directory level.
    Ls {
        #[arg(long, default_value = "local")]
        on: String,
        #[arg(default_value = ".")]
        path: String,
    },
    /// Copy one file (`local` <-> `local` in standalone mode).
    Copy {
        from: String,
        from_path: String,
        to: String,
        to_path: String,
        #[arg(long)]
        mode: Option<String>,
    },
    /// Model Context Protocol (MCP) server over stdio.
    Mcp {
        #[command(subcommand)]
        cmd: mcp::McpCmd,
    },
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

    match cli.cmd {
        Commands::Status => cmd_status(&data_dir),
        Commands::Enable => cmd_set_enabled(&data_dir, true),
        Commands::Disable => cmd_set_enabled(&data_dir, false),
        Commands::Allow { caller_id } => cmd_allow(&data_dir, caller_id),
        Commands::Deny { caller_id } => cmd_deny(&data_dir, &caller_id),
        Commands::Run {
            on,
            cwd,
            timeout,
            env,
            argv,
        } => cmd_run(&data_dir, &on, cwd, timeout, env, argv).await,
        Commands::Read {
            on,
            offset,
            limit,
            path,
        } => cmd_read(&data_dir, &on, &path, offset, limit).await,
        Commands::Write {
            on,
            mode,
            path,
            content,
        } => cmd_write(&data_dir, &on, &path, content, &mode).await,
        Commands::Ls { on, path } => cmd_ls(&data_dir, &on, &path).await,
        Commands::Copy {
            from,
            from_path,
            to,
            to_path,
            mode,
        } => cmd_copy(&from, &from_path, &to, &to_path, &mode).await,
        Commands::Mcp { cmd } => mcp::cmd_mcp(data_dir, cmd).await,
    }
}

fn require_local(on: &str) -> Result<()> {
    if on != "local" {
        bail!(
            "target `{on}` is not reachable: standalone `envoy` has no remote transport attached"
        );
    }
    Ok(())
}

fn print_store(store: &AgentConfigStore) {
    println!("data_dir: {}", store.root.display());
    println!(
        "enabled:  {}",
        if store.config.enabled {
            "yes"
        } else {
            "no — wire calls are refused"
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

fn cmd_status(data_dir: &Path) -> Result<()> {
    let store = AgentConfigStore::open(data_dir.to_path_buf())?;
    print_store(&store);
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

fn parse_mode(mode: &Option<String>) -> Result<Option<u32>> {
    match mode {
        None => Ok(None),
        Some(m) => {
            let digits = m.strip_prefix("0o").unwrap_or(m);
            Ok(Some(u32::from_str_radix(digits, 8).with_context(|| {
                format!("mode `{m}` is not octal (e.g. 644 or 0755)")
            })?))
        }
    }
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

async fn cmd_run(
    data_dir: &Path,
    on: &str,
    cwd: Option<String>,
    timeout: u64,
    env: Vec<String>,
    mut argv: Vec<String>,
) -> Result<()> {
    require_local(on)?;
    if argv.first().map(|a| a == "--").unwrap_or(false) {
        argv.remove(0);
    }
    if argv.is_empty() {
        bail!("no command given");
    }
    let env = parse_env(&env)?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let runner = Runner::new(data_dir.to_path_buf());
    let task = tokio::spawn(async move {
        let cancel = tokio::sync::Notify::new();
        let _ = runner
            .exec_into(&argv, cwd, &env, timeout, &tx, &cancel)
            .await;
    });

    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut code = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::Out { fd, data } => {
                if fd == 1 {
                    err.write_all(&data)?;
                } else {
                    out.write_all(&data)?;
                }
            }
            AgentEvent::Done { code: c, ms } => {
                out.flush()?;
                err.flush()?;
                eprintln!("exit={c} ms={ms}");
                code = Some(c);
                break;
            }
            AgentEvent::Failed { error } => {
                out.flush()?;
                err.flush()?;
                bail!("{error}");
            }
            AgentEvent::Denied { reason } => {
                out.flush()?;
                err.flush()?;
                bail!("denied: {reason}");
            }
            _ => {}
        }
    }
    let _ = task.await;
    match code {
        Some(0) => Ok(()),
        Some(n) => std::process::exit(n.clamp(1, 125)),
        None => bail!("the agent stream ended without an exit status"),
    }
}

async fn cmd_read(
    data_dir: &Path,
    on: &str,
    path: &str,
    offset: u64,
    limit: u64,
) -> Result<()> {
    require_local(on)?;
    let (size, mode, data) = Runner::new(data_dir.to_path_buf())
        .read_range(path, offset, limit)
        .await?;
    let mut out = std::io::stdout();
    out.write_all(&data)?;
    out.flush()?;
    eprintln!(
        "{} bytes of {} (mode {:o}) to stdout",
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
    require_local(on)?;
    let mode = parse_mode(mode)?;
    let data = match content.as_deref() {
        Some("-") | None => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            buf
        }
        Some(text) => text.as_bytes().to_vec(),
    };
    let bytes = Runner::new(data_dir.to_path_buf())
        .write_file(path, &data, mode)
        .await?;
    println!("wrote {bytes} bytes to {path} on {on}");
    Ok(())
}

async fn cmd_ls(data_dir: &Path, on: &str, path: &str) -> Result<()> {
    require_local(on)?;
    let entries = Runner::new(data_dir.to_path_buf()).list_dir(path).await?;
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

async fn cmd_copy(
    from: &str,
    from_path: &str,
    to: &str,
    to_path: &str,
    mode: &Option<String>,
) -> Result<()> {
    require_local(from)?;
    require_local(to)?;
    let mode = parse_mode(mode)?;
    let src = resolve(from_path);
    let dst = resolve(to_path);
    let meta = tokio::fs::metadata(&src)
        .await
        .with_context(|| format!("stat {}", src.display()))?;
    let bytes = tokio::fs::copy(&src, &dst)
        .await
        .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    set_mode(&dst, mode.unwrap_or_else(|| mode_of(&meta)));
    println!("copied {bytes} bytes: {src:?} -> {dst:?}");
    Ok(())
}
