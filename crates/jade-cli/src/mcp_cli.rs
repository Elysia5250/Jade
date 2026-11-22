//! `jade mcp` CLI subcommands (`serve`, `list`, `tools`).

use anyhow::{bail, Result};
use clap::Subcommand;
use jade_agent::mcp::{client, config, server};
use jade_agent::permissions::PermissionMode;
use jade_agent::runtime::{AgentReq, AgentResp};
use jade_agent::{daemon, runtime};
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub(crate) enum McpCmd {
    /// Speak MCP on stdin/stdout (for an MCP client's server config).
    Serve {
        /// Optional permission mode (`default`, `accept-edits`, `plan`, `auto`).
        #[arg(long)]
        mode: Option<String>,
    },
    /// List configured MCP servers and workspace trust status.
    List,
    /// Connect to configured MCP servers and list discovered tools.
    Tools,
    /// Connect to configured MCP servers and list exposed resources.
    Resources {
        /// Optional MCP server name filter.
        server: Option<String>,
    },
    /// Connect to configured MCP servers and list exposed prompts.
    Prompts {
        /// Optional MCP server name filter.
        server: Option<String>,
    },
    /// List live pooled MCP server processes managed by the daemon.
    Ps,
    /// Stop a live pooled MCP server process (or all with `--all`).
    Stop {
        /// Server name to stop.
        name: Option<String>,
        /// Stop all live pooled MCP server processes.
        #[arg(long)]
        all: bool,
    },
}

pub(crate) async fn cmd_mcp(data_dir: PathBuf, cmd: McpCmd, topaz: Option<PathBuf>) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| data_dir.clone());
    match cmd {
        McpCmd::Serve { mode } => {
            let parsed = mode
                .as_deref()
                .and_then(PermissionMode::parse)
                .unwrap_or(PermissionMode::Default);
            server::serve(data_dir, topaz, parsed).await
        }
        McpCmd::List => {
            let report = config::load_mcp_configs(&data_dir, &cwd);
            if !report.skipped_untrusted.is_empty() {
                println!(
                    "skipped untrusted project MCP configs (trust workspace via TUI `/trust on`):"
                );
                for p in &report.skipped_untrusted {
                    println!("  - {}", p.display());
                }
            }
            if report.servers.is_empty() {
                println!("no MCP servers configured");
                return Ok(());
            }
            for s in report.servers {
                let kind = match s.transport {
                    config::McpTransportConfig::Stdio { .. } => "stdio",
                    config::McpTransportConfig::Http { .. } => "http",
                };
                println!(
                    "{:<18} scope={:<7} transport={:<5} disabled={}",
                    s.name,
                    s.scope.label(),
                    kind,
                    s.disabled
                );
            }
            Ok(())
        }
        McpCmd::Tools => {
            let mgr = client::McpManager::connect_configured(&data_dir, &cwd).await;
            for s in mgr.statuses() {
                if let Some(err) = &s.error {
                    println!("{} [{}] error: {}", s.name, s.transport_label, err);
                } else {
                    println!(
                        "{} [{}] ({} tools)",
                        s.name, s.transport_label, s.tool_count
                    );
                }
            }
            for t in mgr.tools() {
                println!(
                    "  {} -> {}:{}  {}",
                    t.qualified_name, t.server, t.raw_name, t.description
                );
            }
            Ok(())
        }
        McpCmd::Resources { server } => {
            let mgr = client::McpManager::connect_configured(&data_dir, &cwd).await;
            let items = mgr.list_resources(server.as_deref()).await?;
            if items.is_empty() {
                println!("no MCP resources available");
                return Ok(());
            }
            for r in items {
                println!("  [{}] {} ({})  {}", r.server, r.uri, r.name, r.description);
            }
            Ok(())
        }
        McpCmd::Prompts { server } => {
            let mgr = client::McpManager::connect_configured(&data_dir, &cwd).await;
            let items = mgr.list_prompts(server.as_deref()).await?;
            if items.is_empty() {
                println!("no MCP prompts available");
                return Ok(());
            }
            for p in items {
                println!("  [{}] {}  {}", p.server, p.name, p.description);
            }
            Ok(())
        }
        McpCmd::Ps => {
            if !daemon::is_running(&data_dir).await {
                println!("daemon not running (no live MCP server processes)");
                return Ok(());
            }
            let list = match runtime::request(&data_dir, &AgentReq::ProcessList).await? {
                AgentResp::ProcessList(list) => list,
                AgentResp::Error(err) => bail!("{err}"),
                other => bail!("unexpected daemon reply: {other:?}"),
            };
            if list.mcp_servers.is_empty() {
                println!("no live MCP server processes");
                return Ok(());
            }
            for m in list.mcp_servers {
                let pid = m
                    .pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<18} {:<6} {:<7} pid={:<7} idle={:<6} {}",
                    m.name,
                    m.transport,
                    m.scope,
                    pid,
                    crate::ps_cli::format_duration_short(m.idle_secs),
                    m.cwd.display()
                );
            }
            Ok(())
        }
        McpCmd::Stop { name, all } => {
            if !all && name.is_none() {
                bail!("specify a server name or pass `--all` to stop all live MCP servers");
            }
            if !daemon::is_running(&data_dir).await {
                println!("daemon not running");
                return Ok(());
            }
            let target = if all { None } else { name };
            match runtime::request(&data_dir, &AgentReq::StopMcp { name: target }).await? {
                AgentResp::Stopped { count } => {
                    println!("stopped {count} MCP server process(es)");
                    Ok(())
                }
                AgentResp::Error(err) => bail!("{err}"),
                other => bail!("unexpected daemon reply: {other:?}"),
            }
        }
    }
}
