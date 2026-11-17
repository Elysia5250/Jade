//! Bidirectional MCP support: `envoy mcp serve` server + client (`stdio` & Streamable HTTP).

pub mod client;
pub mod config;
pub mod server;
mod server_tools;
pub mod transport;

use crate::permissions::PermissionMode;
use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum McpCmd {
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
}

pub async fn cmd_mcp(
    data_dir: PathBuf,
    cmd: McpCmd,
    conduit: Option<PathBuf>,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| data_dir.clone());
    match cmd {
        McpCmd::Serve { mode } => {
            let parsed = mode
                .as_deref()
                .and_then(PermissionMode::parse)
                .unwrap_or(PermissionMode::Default);
            server::serve(data_dir, conduit, parsed).await
        }
        McpCmd::List => {
            let report = config::load_mcp_configs(&data_dir, &cwd);
            if !report.skipped_untrusted.is_empty() {
                println!("skipped untrusted project MCP configs (trust workspace via TUI `/trust on`):");
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
                    println!("{} [{}] ({} tools)", s.name, s.transport_label, s.tool_count);
                }
            }
            for t in mgr.tools() {
                println!("  {} -> {}:{}  {}", t.qualified_name, t.server, t.raw_name, t.description);
            }
            Ok(())
        }
    }
}
