//! Unified background process inspection (`jade ps`), daemon restart/sleep,
//! and shared formatting for TUI `/ps`.

use anyhow::{bail, Result};
use jade_agent::runtime::{AgentReq, AgentResp, RuntimeProcessList};
use jade_agent::{daemon, runtime};
use jade_proto::JobState;
use std::path::Path;
use std::time::Duration;

pub(crate) async fn cmd_ps(data_dir: &Path, json: bool) -> Result<()> {
    if !daemon::is_running(data_dir).await {
        if json {
            println!("{{\"running\":false}}");
        } else {
            println!("daemon: stopped");
        }
        return Ok(());
    }
    let list = match runtime::request(data_dir, &AgentReq::ProcessList).await? {
        AgentResp::ProcessList(list) => list,
        AgentResp::Error(err) => bail!("{err}"),
        other => bail!("unexpected daemon reply: {other:?}"),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
    } else {
        print!("{}", format_process_list(&list));
    }
    Ok(())
}

pub(crate) async fn cmd_daemon_restart(data_dir: &Path) -> Result<()> {
    if daemon::is_running(data_dir).await {
        if !daemon::stop(data_dir)? {
            bail!("no pid file; stop the daemon manually");
        }
        let mut stopped = false;
        for _ in 0..60 {
            if !daemon::is_running(data_dir).await {
                stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !stopped {
            bail!("the daemon ignored SIGTERM for 3s");
        }
    }
    daemon::ensure(data_dir).await?;
    println!(
        "daemon restarted ({})",
        daemon::socket_path(data_dir).display()
    );
    Ok(())
}

pub(crate) async fn cmd_daemon_sleep(data_dir: &Path) -> Result<()> {
    if !daemon::is_running(data_dir).await {
        println!("daemon not running");
        return Ok(());
    }
    match runtime::request(data_dir, &AgentReq::Sleep).await? {
        AgentResp::Slept {
            sessions,
            mcp_servers,
        } => {
            println!(
                "daemon entered L2 cool sleep: dehydrated {sessions} session(s), stopped {mcp_servers} MCP server(s)"
            );
            Ok(())
        }
        AgentResp::Error(err) => bail!("{err}"),
        other => bail!("unexpected daemon reply: {other:?}"),
    }
}

pub(crate) fn format_process_list(list: &RuntimeProcessList) -> String {
    let mut out = String::new();
    let active_jobs: Vec<_> = list.jobs.iter().filter(|j| !j.state.is_done()).collect();
    let tier = if list.sleeping {
        "cool (sleeping)"
    } else {
        "active"
    };
    out.push_str(&format!(
        "daemon: pid={}  state={}  uptime={}\n",
        list.daemon_pid,
        tier,
        format_duration_short(list.uptime_secs)
    ));
    out.push_str(&format!(
        "summary: {} live session(s), {} active job(s), {} monitor(s), {} MCP server(s)\n",
        list.sessions.len(),
        active_jobs.len(),
        list.monitors.len(),
        list.mcp_servers.len()
    ));

    if !list.sessions.is_empty() {
        out.push_str("\nSESSIONS\n");
        for s in &list.sessions {
            let state = if s.running {
                "running"
            } else if s.side_running {
                "sidechat"
            } else {
                "idle"
            };
            let short_id: String = s.id.chars().take(12).collect();
            out.push_str(&format!(
                "  {short_id:<12} {state:<8} q={} mon={} sub={} idle={:<6} {}\n",
                s.queued,
                s.active_monitors,
                s.subscribers,
                format_duration_short(s.idle_secs),
                s.cwd.display()
            ));
        }
    }

    if !active_jobs.is_empty() {
        out.push_str("\nACTIVE JOBS\n");
        for j in &active_jobs {
            let state = match j.state {
                JobState::Queued { .. } => "queued",
                JobState::Running { .. } => "running",
                JobState::Done { .. } => "done",
            };
            let pid = j
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into());
            out.push_str(&format!(
                "  #{:<4} {:<8} {:<8} pid={:<7} {}\n",
                j.id,
                j.group,
                state,
                pid,
                j.command()
            ));
        }
    }

    if !list.monitors.is_empty() {
        out.push_str("\nMONITORS\n");
        for m in &list.monitors {
            let short_sid: String = m.session_id.chars().take(8).collect();
            out.push_str(&format!(
                "  #{} [{}] job=#{} triggers={} pattern=`{}` {}\n",
                m.id, short_sid, m.job_id, m.trigger_count, m.pattern, m.label
            ));
        }
    }

    if !list.mcp_servers.is_empty() {
        out.push_str("\nMCP SERVERS\n");
        for m in &list.mcp_servers {
            let pid = m
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into());
            out.push_str(&format!(
                "  {:<18} {:<6} {:<7} pid={:<7} idle={:<6} {}\n",
                m.name,
                m.transport,
                m.scope,
                pid,
                format_duration_short(m.idle_secs),
                m.cwd.display()
            ));
        }
    }

    out
}

pub(crate) fn format_duration_short(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jade_agent::runtime::{ActiveMonitorEntry, LiveSessionEntry, McpProcessEntry};
    use std::path::PathBuf;

    #[test]
    fn format_process_list_renders_all_sections_and_sleeping_state() {
        let sleeping = RuntimeProcessList {
            daemon_pid: 100,
            uptime_secs: 45,
            sleeping: true,
            sessions: vec![],
            monitors: vec![],
            mcp_servers: vec![],
            jobs: vec![],
        };
        let text = format_process_list(&sleeping);
        assert!(text.contains("state=cool (sleeping)"));
        assert!(text.contains("uptime=45s"));

        let active = RuntimeProcessList {
            daemon_pid: 200,
            uptime_secs: 3725,
            sleeping: false,
            sessions: vec![LiveSessionEntry {
                id: "sess-1234567890".into(),
                cwd: PathBuf::from("/repo"),
                target: "local".into(),
                running: true,
                queued: 1,
                idle_secs: 5,
                active_monitors: 1,
                subscribers: 1,
                side_running: false,
            }],
            monitors: vec![ActiveMonitorEntry {
                session_id: "sess-1234567890".into(),
                id: 3,
                job_id: 9,
                label: "watch".into(),
                pattern: "err".into(),
                trigger_count: 2,
                owns_job: true,
            }],
            mcp_servers: vec![McpProcessEntry {
                name: "github".into(),
                scope: "user".into(),
                transport: "stdio".into(),
                pid: Some(4321),
                cwd: PathBuf::from("/repo"),
                idle_secs: 125,
            }],
            jobs: vec![],
        };
        let text = format_process_list(&active);
        assert!(text.contains("state=active"));
        assert!(text.contains("uptime=1h2m"));
        assert!(text.contains("SESSIONS"));
        assert!(text.contains("MONITORS"));
        assert!(text.contains("MCP SERVERS"));
        assert!(text.contains("pid=4321"));
    }
}
