//! Daemon-side Runtime operations for background monitors and session checkpoints.

use super::monitors::MonitorHub;
use super::server::{Live, LivePhase, PhaseGuard, Runtime};
use crate::agent::checkpoint::{apply_file_baselines, CheckpointStore};
use crate::agent::{AgentEvent, MonitorBroker, MonitorInfo};
use crate::auth::resolve_provider_with_overrides;
use crate::session::{ChatMessage, ChatRole};
use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

impl Runtime {
    pub(crate) fn monitor_hub(&self, id: &str) -> Option<Arc<MonitorHub>> {
        self.session(id)
            .map(|live| live.lock().unwrap().monitors.clone())
    }

    pub(crate) fn publish_monitors_sync(&self, id: &str, monitors: Vec<MonitorInfo>) {
        let Some(live) = self.session(id) else {
            return;
        };
        let mut state = live.lock().unwrap();
        self.publish(&mut state, AgentEvent::MonitorsSync { monitors });
    }

    pub(crate) fn deliver_monitor_fire(
        self: &Arc<Self>,
        id: &str,
        info: MonitorInfo,
        matched_lines: Vec<String>,
        stopped: bool,
        snapshot: Vec<MonitorInfo>,
        agent_message: String,
    ) {
        let Some(live) = self.session(id) else {
            return;
        };
        let should_start_turn = {
            let mut state = live.lock().unwrap();
            self.publish(
                &mut state,
                AgentEvent::MonitorFired {
                    id: info.id,
                    label: info.label,
                    job_id: info.job_id,
                    matched_lines,
                    trigger_count: info.trigger_count,
                    max_triggers: info.max_triggers,
                    stopped,
                },
            );
            self.publish(&mut state, AgentEvent::MonitorsSync { monitors: snapshot });

            if state.phase != LivePhase::Idle || state.running {
                state.steer.lock().unwrap().push(agent_message);
                false
            } else if state.claim(LivePhase::RunningTurn) {
                crate::session::strip_prior_turn_reasoning(&mut state.messages);
                state.messages.push(ChatMessage {
                    role: ChatRole::User,
                    content: agent_message,
                    reasoning: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,

                    reasoning_blocks: Vec::new(),
                });
                true
            } else {
                state.steer.lock().unwrap().push(agent_message);
                false
            }
        };

        if should_start_turn {
            self.start_turn(id, live, None);
        }
    }

    pub async fn stop_monitor(&self, id: &str, monitor_id: u64) -> Result<()> {
        let Some(hub) = self.monitor_hub(id) else {
            bail!("no such session: {id}");
        };
        let Some(_) = hub.stop(monitor_id).await? else {
            bail!("no monitor #{monitor_id} in session {id}");
        };
        Ok(())
    }

    pub fn list_checkpoints(
        &self,
        id: &str,
    ) -> Result<Vec<crate::agent::checkpoint::CheckpointSummary>> {
        let Some(live) = self.session(id) else {
            bail!("no such session: {id}");
        };
        let state = live.lock().unwrap();
        Ok(state.checkpoints.list_summaries())
    }

    pub async fn rewind(&self, id: &str, checkpoint_id: Option<u64>) -> Result<()> {
        let Some(live) = self.session(id) else {
            bail!("no such session: {id}");
        };

        {
            let mut state = live.lock().unwrap();
            if !state.claim(LivePhase::Rewinding) {
                bail!("session is busy; wait for the active turn to finish or interrupt first");
            }
        }
        let _phase = PhaseGuard::new(Arc::clone(&live), LivePhase::Rewinding);
        let (target, local_socket, net_baselines) = {
            let state = live.lock().unwrap();
            let (idx, net) = state.checkpoints.plan_rewind(checkpoint_id)?;
            (state.target.clone(), state.local_socket.clone(), (idx, net))
        };

        let (idx, net) = net_baselines;
        let remote = super::remote_for(
            &self.data_dir,
            &target,
            self.conduit.clone(),
            local_socket,
            self.local_server.clone(),
        );
        let (restored_files, deleted_files, skipped_files) =
            apply_file_baselines(&remote, &target, &net).await;

        let (cp_id, queued) = {
            let mut state = live.lock().unwrap();
            if state.phase != LivePhase::Rewinding {
                bail!("session became busy during rewind");
            }
            let cp = state.checkpoints.take_rewind(idx);
            let queued = state.checkpoints.queued_write().ok();
            let cp_id = cp.id;
            state.messages = cp.messages.clone();
            state.user_turns = cp.remaining_user_turns;
            state.active_checkpoint_id = None;
            self.publish(
                &mut state,
                AgentEvent::CheckpointRewound {
                    checkpoint_id: cp.id,
                    label: cp.label,
                    messages: cp.messages,
                    remaining_user_turns: cp.remaining_user_turns,
                    restored_files,
                    deleted_files,
                    skipped_files,
                },
            );
            (cp_id, queued)
        };
        if let Some((path, bytes)) = queued {
            let _ = CheckpointStore::write_queued(&path, &bytes);
        }
        self.run_post_rewind_hook(id, &live, cp_id).await;
        Ok(())
    }

    pub async fn probe_mcp(&self, cwd: PathBuf) -> Vec<String> {
        let mgr = crate::mcp::client::McpManager::connect_configured(&self.data_dir, &cwd).await;
        let mut lines = Vec::new();
        for p in mgr.skipped_untrusted() {
            lines.push(format!(
                "skipped untrusted project MCP config: {}",
                p.display()
            ));
        }
        for s in mgr.statuses() {
            if let Some(err) = &s.error {
                lines.push(format!("{} [{}] error: {}", s.name, s.transport_label, err));
            } else {
                lines.push(format!(
                    "{} [{}] connected ({} tools)",
                    s.name, s.transport_label, s.tool_count
                ));
            }
        }
        if lines.is_empty() {
            lines.push("no MCP servers configured".into());
        }
        lines
    }

    pub async fn refresh_models(
        &self,
    ) -> Result<(String, Vec<crate::agent::context::ListedModel>)> {
        let Some((prov, _)) = resolve_provider_with_overrides(&self.data_dir, None) else {
            bail!("no active provider configured");
        };
        let http = crate::agent::shared_http_client()?;
        let bg_http = http.clone();
        let bg_dir = self.data_dir.clone();
        tokio::spawn(async move {
            crate::agent::context::refresh_models_dev_if_stale(&bg_http, &bg_dir).await;
        });
        let list =
            crate::agent::context::fetch_provider_models(&http, Some(&self.data_dir), &prov).await;
        Ok((prov.id, list))
    }
}

pub(crate) fn seed_checkpoint_user_turns(
    checkpoints: &crate::agent::checkpoint::CheckpointStore,
    history: &[ChatMessage],
) -> usize {
    if let Some(last) = checkpoints.list_summaries().last() {
        return last.remaining_user_turns + 1;
    }
    history
        .iter()
        .filter(|m| {
            m.role == ChatRole::User
                && !m.content.starts_with(crate::agent::prompt::ENV_PREFIX)
                && !m.content.starts_with("<monitor_event")
                && !m.content.starts_with("<compacted_context>")
                && !m.content.starts_with("<handoff_context>")
        })
        .count()
}

pub(crate) fn record_turn_checkpoint(live: &Arc<Mutex<Live>>, prompt: Option<&str>) {
    let Some(text) = prompt else {
        return;
    };
    let queued = {
        let mut state = live.lock().unwrap();
        let turns = state.user_turns;
        let snapshot = state.messages.clone();
        let cp_id = state.checkpoints.insert_turn(text, snapshot, turns);
        state.active_checkpoint_id = Some(cp_id);
        state.user_turns = turns + 1;
        state.checkpoints.queued_write().ok()
    };
    if let Some((path, bytes)) = queued {
        let _ = CheckpointStore::write_queued(&path, &bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::MonitorInfo;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "envoy-ops-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn busy_monitor_fire_steers_and_rewind_restores_files_and_history() {
        let dir = tmpdir("flow");
        let work = dir.join("ws");
        std::fs::create_dir_all(&work).unwrap();
        let tracked = work.join("main.rs");
        let extra = work.join("new.rs");
        std::fs::write(&tracked, "fn v0() {}\n").unwrap();

        let rt = Runtime::new(dir.clone());
        let live = rt.ensure("s1".into(), work.clone(), "local".into(), vec![]);

        // Record Turn 0 checkpoint and file edits
        record_turn_checkpoint(&live, Some("first turn"));
        {
            let mut st = live.lock().unwrap();
            let cp = st.active_checkpoint_id.unwrap();
            st.checkpoints.record_file_change(
                cp,
                tracked.display().to_string(),
                true,
                "fn v0() {}\n".into(),
                true,
            );
            st.checkpoints.record_file_change(
                cp,
                extra.display().to_string(),
                false,
                String::new(),
                true,
            );
            st.messages.push(ChatMessage {
                role: ChatRole::User,
                content: "first turn".into(),
                reasoning: None,
                tool_calls: vec![],
                tool_call_id: None,
                tool_name: None,

                reasoning_blocks: Vec::new(),
            });
            st.active_checkpoint_id = None;
        }
        std::fs::write(&tracked, "fn v1() {}\n").unwrap();
        std::fs::write(&extra, "pub fn x() {}\n").unwrap();

        // Simulate busy turn -> monitor fire steers without starting a duplicate turn
        {
            let mut st = live.lock().unwrap();
            st.running = true;
        }
        let info = MonitorInfo {
            id: 1,
            job_id: 9,
            label: "watch".into(),
            pattern: "ERR".into(),
            command: "tail -f app.log".into(),
            trigger_count: 1,
            max_triggers: None,
            owns_job: true,
            active: true,
        };
        rt.deliver_monitor_fire(
            "s1",
            info.clone(),
            vec!["ERR failed".into()],
            false,
            vec![info],
            "<monitor_event>ERR failed</monitor_event>".into(),
        );
        {
            let mut st = live.lock().unwrap();
            let steered = st.steer.lock().unwrap().clone();
            assert_eq!(steered, vec!["<monitor_event>ERR failed</monitor_event>"]);
            st.running = false;
        }

        // Now rewind -> restores main.rs, deletes new.rs, resets messages + user_turns
        rt.rewind("s1", None).await.unwrap();
        assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "fn v0() {}\n");
        assert!(!extra.exists());
        {
            let st = live.lock().unwrap();
            assert!(st.messages.is_empty());
            assert_eq!(st.user_turns, 0);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
