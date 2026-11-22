//! Clap types for the `jade` binary. Dispatch stays in `main`.

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use jade_proto::{SandboxPolicy, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use std::path::PathBuf;

use crate::mcp_cli;
use crate::plugin_cli;

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// Ask the AI agent a one-shot prompt (headless mode).
    Ask {
        #[arg(trailing_var_arg = true, required = true)]
        prompt: Vec<String>,
    },
    /// Show policy, daemon state, topaz status, and job counts.
    Status,
    /// Show all daemon-managed background processes (sessions, jobs, monitors, MCP servers).
    Ps {
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
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
    /// Add a remote device id to this device's caller allowlist (not a tool rule).
    Allow { caller_id: String },
    /// Remove a remote device id from this device's caller allowlist.
    Deny { caller_id: String },
    /// List the devices in this topaz account.
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
        /// Local use only: do not expose service 16 to topaz.
        #[arg(long, alias = "no-topaz")]
        local_only: bool,
        /// Idle accept connections parked on topaz.
        #[arg(long, default_value_t = jade_service::DEFAULT_POOL)]
        accept_pool: usize,
    },
    /// Queue a job and print its id (never waits for it).
    Submit(TaskArgs),
    /// Queue a shell job, follow its output, and exit with its code.
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
        cmd: mcp_cli::McpCmd,
    },
    /// Manage declarative plugin bundles (`plugin.json`).
    Plugin {
        #[command(subcommand)]
        cmd: plugin_cli::PluginCmd,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum AuthCmd {
    /// Connect an LLM provider (API key, OAuth, or custom endpoint).
    Login,
    /// Add or update a named custom provider endpoint non-interactively.
    Add {
        /// Provider identifier (e.g. `ollama`, `vllm`, `corp-gateway`).
        id: String,
        /// Base URL for the provider API.
        #[arg(long)]
        base_url: String,
        /// Wire protocol (`openai`, `anthropic`, or `codex`).
        #[arg(long, default_value = "openai")]
        protocol: String,
        /// Optional API key (or empty for local servers like Ollama).
        #[arg(long, default_value = "")]
        api_key: String,
        /// Default model ID (repeatable).
        #[arg(long = "model")]
        models: Vec<String>,
        /// Extra HTTP header `Key=Value` (repeatable; supports `${SESSION_ID}`).
        #[arg(long = "header")]
        headers: Vec<String>,
    },
    /// List connected providers and credentials.
    List,
    /// Remove stored credentials for a provider.
    Logout { provider: Option<String> },
}

#[derive(Subcommand, Debug)]
pub(crate) enum DaemonCmd {
    /// Install user-level autostart (launchd / systemd --user).
    Install,
    /// Remove the autostart unit.
    Uninstall,
    /// Start the daemon in the background.
    Start,
    /// Ask the daemon to shut down.
    Stop,
    /// Restart the daemon cleanly.
    Restart,
    /// Enter L2 cool hibernation (dehydrate idle sessions and stop idle MCP servers).
    Sleep,
    /// Report whether the daemon is running.
    Status,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct TaskArgs {
    /// Working directory on the target.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,
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
    pub(crate) fn into_request(self) -> Result<SubmitRequest> {
        let mut argv = self.argv;
        if argv.first().map(|a| a == "--").unwrap_or(false) {
            argv.remove(0);
        }
        if argv.is_empty() {
            bail!("no command given");
        }
        // `--shell` joins every word. A single argument is already a command
        // line ("echo hi | wc -l"); several words without --shell stay argv.
        let spec = if self.shell {
            TaskSpec::Shell {
                command: argv.join(" "),
            }
        } else if argv.len() == 1 {
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
