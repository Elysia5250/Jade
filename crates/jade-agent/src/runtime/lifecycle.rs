//! Daemon L2 Cool Hibernation reaper, manual sleep, and unified process snapshot.

use super::live::LivePhase;
use super::protocol::{
    ActiveMonitorEntry, LiveSessionEntry, McpProcessEntry, RuntimeProcessList,
};
use super::server::Runtime;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, info};

/// Interval between background L2 Cool Hibernation sweeps.
pub(crate) const REAPER_TICK: Duration = Duration::from_secs(60);
/// Idle TTL before pooled MCP child processes are proactively reaped.
pub(crate) const MCP_IDLE_TTL: Duration = Duration::from_secs(15 * 60);
/// Idle TTL before evictable in-memory sessions are dehydrated to disk.
pub(crate) const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);

impl Runtime {
    /// Spawn the background L2 Cool Hibernation reaper task.
    ///
    /// Runs every `REAPER_TICK`, reclaiming idle MCP child processes and
    /// dehydrating evictable idle sessions without exiting the daemon.
    pub fn spawn_reaper(self: &Arc<Self>) -> JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(REAPER_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick so boot stays silent.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(rt) = weak.upgrade() else {
                    break;
                };
                let (sessions, mcp) = rt.reap_idle(MCP_IDLE_TTL, SESSION_IDLE_TTL).await;
                if sessions > 0 || mcp > 0 {
                    info!(
                        sessions,
                        mcp_servers = mcp,
                        "L2 reaper reclaimed idle resources"
                    );
                } else {
                    debug!("L2 reaper tick: nothing to reclaim");
                }
            }
        })
    }

    /// Reap idle MCP servers older than `mcp_ttl` and dehydrate evictable idle
    /// sessions older than `session_ttl`.
    pub(crate) async fn reap_idle(
        &self,
        mcp_ttl: Duration,
        session_ttl: Duration,
    ) -> (usize, usize) {
        let mcp = self.mcp_pool.reap_idle(mcp_ttl).await;
        let sessions = self.dehydrate_idle_sessions(Some(session_ttl));
        (sessions, mcp)
    }

    /// Immediately enter L2 Cool Hibernation: dehydrate all evictable idle
    /// sessions and stop pooled MCP servers not used by an active turn.
    pub async fn sleep_now(&self) -> (usize, usize) {
        let active_cwds = self.active_turn_cwds();
        let mcp = self.mcp_pool.stop_except(&active_cwds).await;
        let sessions = self.dehydrate_idle_sessions(None);
        info!(
            sessions,
            mcp_servers = mcp,
            "daemon entered L2 cool hibernation"
        );
        (sessions, mcp)
    }

    /// Stop a named pooled MCP server (or all when `name` is `None`).
    pub async fn stop_mcp(&self, name: Option<&str>) -> usize {
        self.mcp_pool.stop(name).await
    }

    /// Unified snapshot of daemon-managed sessions, monitors, MCP servers, and jobs.
    pub async fn process_list(&self) -> RuntimeProcessList {
        let (sessions, monitors) = self.snapshot_sessions_and_monitors();
        let mcp_servers: Vec<McpProcessEntry> = self
            .mcp_pool
            .list_active()
            .await
            .into_iter()
            .map(|m| McpProcessEntry {
                name: m.name,
                scope: m.scope,
                transport: m.transport,
                pid: m.pid,
                cwd: m.cwd,
                idle_secs: m.idle_secs,
            })
            .collect();
        let jobs = self
            .local_server
            .as_ref()
            .map(|s| s.jobs().list())
            .unwrap_or_default();
        let any_active_session = sessions
            .iter()
            .any(|s| s.running || s.active_monitors > 0 || s.side_running);
        let any_active_job = jobs.iter().any(|j| !j.state.is_done());
        let sleeping = !any_active_session && !any_active_job && mcp_servers.is_empty();

        RuntimeProcessList {
            daemon_pid: std::process::id(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            sleeping,
            sessions,
            monitors,
            mcp_servers,
            jobs,
        }
    }

    fn dehydrate_idle_sessions(&self, min_idle: Option<Duration>) -> usize {
        let now = Instant::now();
        let mut map = self.sessions.lock().unwrap();
        let before = map.len();
        map.retain(|_, live| {
            let state = live.lock().unwrap();
            if !state.is_evictable() {
                return true;
            }
            match min_idle {
                Some(ttl) => now.duration_since(state.last_used) < ttl,
                None => false,
            }
        });
        before.saturating_sub(map.len())
    }

    fn active_turn_cwds(&self) -> Vec<PathBuf> {
        let map = self.sessions.lock().unwrap();
        let mut cwds = Vec::new();
        for live in map.values() {
            let state = live.lock().unwrap();
            if state.running
                || state.phase != LivePhase::Idle
                || state.side.as_ref().is_some_and(|s| s.running)
            {
                cwds.push(state.cwd.clone());
            }
        }
        cwds
    }

    fn snapshot_sessions_and_monitors(&self) -> (Vec<LiveSessionEntry>, Vec<ActiveMonitorEntry>) {
        let now = Instant::now();
        let map = self.sessions.lock().unwrap();
        let mut sessions = Vec::with_capacity(map.len());
        let mut monitors = Vec::new();
        for (id, live) in map.iter() {
            let state = live.lock().unwrap();
            let active_mons = state.monitors.active_list();
            let subscribers = state.subs.iter().filter(|tx| !tx.is_closed()).count();
            sessions.push(LiveSessionEntry {
                id: id.clone(),
                cwd: state.cwd.clone(),
                target: state.target.clone(),
                running: state.running,
                queued: state.queued.len(),
                idle_secs: now.duration_since(state.last_used).as_secs(),
                active_monitors: active_mons.len(),
                subscribers,
                side_running: state.side.as_ref().is_some_and(|s| s.running),
            });
            for m in active_mons {
                monitors.push(ActiveMonitorEntry {
                    session_id: id.clone(),
                    id: m.id,
                    job_id: m.job_id,
                    label: m.label,
                    pattern: m.pattern,
                    trigger_count: m.trigger_count,
                    owns_job: m.owns_job,
                });
            }
        }
        sessions.sort_by(|a, b| b.running.cmp(&a.running).then(a.id.cmp(&b.id)));
        monitors.sort_by(|a, b| a.session_id.cmp(&b.session_id).then(a.id.cmp(&b.id)));
        (sessions, monitors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::MonitorInfo;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "jade-lifecycle-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn sleep_now_dehydrates_idle_sessions_but_preserves_monitored_and_running() {
        let dir = tmpdir("sleep");
        let rt = Runtime::new(dir.clone());

        let idle = rt.ensure("s-idle".into(), dir.clone(), "local".into(), vec![]);
        idle.lock().unwrap().last_used = Instant::now() - Duration::from_secs(120);

        let running = rt.ensure("s-run".into(), dir.clone(), "local".into(), vec![]);
        running.lock().unwrap().running = true;

        let monitored = rt.ensure("s-mon".into(), dir.clone(), "local".into(), vec![]);
        monitored
            .lock()
            .unwrap()
            .monitors
            .seed_active_for_test(MonitorInfo {
                id: 1,
                job_id: 42,
                label: "build".into(),
                pattern: "error".into(),
                command: "cargo watch".into(),
                trigger_count: 0,
                max_triggers: None,
                owns_job: false,
                active: true,
            });

        let before = rt.process_list().await;
        assert_eq!(before.sessions.len(), 3);
        assert_eq!(before.monitors.len(), 1);
        assert!(!before.sleeping);

        let (dehydrated, _) = rt.sleep_now().await;
        assert_eq!(dehydrated, 1, "only truly idle session should dehydrate");
        assert!(rt.session("s-idle").is_none());
        assert!(rt.session("s-run").is_some());
        assert!(rt.session("s-mon").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reap_idle_respects_ttl_and_reports_sleeping_when_empty() {
        let dir = tmpdir("reap");
        let rt = Runtime::new(dir.clone());

        let fresh = rt.ensure("s-fresh".into(), dir.clone(), "local".into(), vec![]);
        fresh.lock().unwrap().last_used = Instant::now();

        let stale = rt.ensure("s-stale".into(), dir.clone(), "local".into(), vec![]);
        stale.lock().unwrap().last_used = Instant::now() - Duration::from_secs(3600);

        let (reaped, _) = rt
            .reap_idle(Duration::from_secs(600), Duration::from_secs(1800))
            .await;
        assert_eq!(reaped, 1);
        assert!(rt.session("s-stale").is_none());
        assert!(rt.session("s-fresh").is_some());

        // Sleep clears the remaining idle session, transitioning to sleeping state.
        let (slept, _) = rt.sleep_now().await;
        assert_eq!(slept, 1);
        let snap = rt.process_list().await;
        assert!(snap.sleeping);
        assert!(snap.sessions.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
