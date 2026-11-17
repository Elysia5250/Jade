//! Daemon-side monitor hub, log cursor poller, and in-session event delivery.

use super::server::Runtime;
use crate::agent::{MonitorBroker, MonitorInfo, MonitorStartSpec};
use crate::session::strip_ansi;
use anyhow::{bail, Result};
use envoy_proto::LogRecord;
use envoy_service::{dialer_for, AgentDialer, Remote};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::debug;

const MAX_ACTIVE_MONITORS: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const MAX_MATCHED_LINES_PER_FIRE: usize = 50;

#[derive(Debug)]
struct MonitorEntry {
    info: MonitorInfo,
    target: String,
    cancel_tx: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl Drop for MonitorEntry {
    fn drop(&mut self) {
        let _ = self.cancel_tx.send(true);
        if let Some(handle) = self.task.take() {
            handle.abort();
        }
    }
}

#[derive(Debug, Default)]
struct HubInner {
    next_id: u64,
    monitors: HashMap<u64, MonitorEntry>,
}

#[derive(Debug)]
pub(crate) struct MonitorHub {
    session_id: String,
    data_dir: PathBuf,
    runtime: Weak<Runtime>,
    inner: Mutex<HubInner>,
}

impl MonitorHub {
    pub(crate) fn new(session_id: String, data_dir: PathBuf, runtime: Weak<Runtime>) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            data_dir,
            runtime,
            inner: Mutex::new(HubInner {
                next_id: 1,
                monitors: HashMap::new(),
            }),
        })
    }

    pub(crate) fn active_list(&self) -> Vec<MonitorInfo> {
        let g = self.inner.lock().unwrap();
        snapshot_locked(&g)
    }

    fn record_trigger(&self, id: u64) -> Option<(MonitorInfo, bool, Vec<MonitorInfo>)> {
        let mut g = self.inner.lock().unwrap();
        let (info, reached_cap) = {
            let entry = g.monitors.get_mut(&id)?;
            if !entry.info.active {
                return None;
            }
            entry.info.trigger_count = entry.info.trigger_count.saturating_add(1);
            let cap = entry
                .info
                .max_triggers
                .is_some_and(|max| entry.info.trigger_count >= max);
            if cap {
                entry.info.active = false;
                let _ = entry.cancel_tx.send(true);
            }
            (entry.info.clone(), cap)
        };
        let snap = snapshot_locked(&g);
        Some((info, reached_cap, snap))
    }

    fn mark_finished(&self, id: u64) -> Option<(MonitorInfo, Vec<MonitorInfo>)> {
        let mut g = self.inner.lock().unwrap();
        let info = {
            let entry = g.monitors.get_mut(&id)?;
            if !entry.info.active {
                return None;
            }
            entry.info.active = false;
            entry.info.clone()
        };
        let snap = snapshot_locked(&g);
        Some((info, snap))
    }
}

fn snapshot_locked(inner: &HubInner) -> Vec<MonitorInfo> {
    let mut out: Vec<MonitorInfo> = inner
        .monitors
        .values()
        .filter(|e| e.info.active)
        .map(|e| e.info.clone())
        .collect();
    out.sort_by_key(|m| m.id);
    out
}

impl MonitorBroker for MonitorHub {
    fn start(&self, spec: MonitorStartSpec) -> Result<MonitorInfo> {
        let (info, snapshot, cancel_rx) = {
            let mut g = self.inner.lock().unwrap();
            let active_count = g.monitors.values().filter(|e| e.info.active).count();
            if active_count >= MAX_ACTIVE_MONITORS {
                bail!("too many active monitors (max {MAX_ACTIVE_MONITORS}); stop one first");
            }
            let id = g.next_id.max(1);
            g.next_id = id + 1;
            let info = MonitorInfo {
                id,
                job_id: spec.job_id,
                label: spec.label.clone(),
                pattern: spec.pattern.clone(),
                command: spec.command.clone(),
                trigger_count: 0,
                max_triggers: spec.max_triggers,
                owns_job: spec.owns_job,
                active: true,
            };
            let (cancel_tx, cancel_rx) = watch::channel(false);
            g.monitors.insert(
                id,
                MonitorEntry {
                    info: info.clone(),
                    target: spec.target.clone(),
                    cancel_tx,
                    task: None,
                },
            );
            if g.monitors.len() > 64 {
                let mut inactive_ids: Vec<u64> = g
                    .monitors
                    .iter()
                    .filter(|(_, e)| !e.info.active)
                    .map(|(&mid, _)| mid)
                    .collect();
                inactive_ids.sort_unstable();
                let to_drop = g.monitors.len().saturating_sub(64);
                for mid in inactive_ids.into_iter().take(to_drop) {
                    g.monitors.remove(&mid);
                }
            }
            (info, snapshot_locked(&g), cancel_rx)
        };

        let weak_rt = self.runtime.clone();
        let session_id = self.session_id.clone();
        let data_dir = self.data_dir.clone();
        let monitor_id = info.id;
        let info_for_task = info.clone();
        let handle = tokio::spawn(async move {
            poll_monitor_loop(
                weak_rt,
                session_id,
                data_dir,
                spec,
                info_for_task,
                cancel_rx,
            )
            .await;
        });

        {
            let mut g = self.inner.lock().unwrap();
            if let Some(entry) = g.monitors.get_mut(&monitor_id) {
                entry.task = Some(handle);
            } else {
                handle.abort();
            }
        }

        if let Some(rt) = self.runtime.upgrade() {
            rt.publish_monitors_sync(&self.session_id, snapshot);
        }
        Ok(info)
    }

    fn list(&self) -> Vec<MonitorInfo> {
        let mut out: Vec<MonitorInfo> = {
            let g = self.inner.lock().unwrap();
            g.monitors.values().map(|e| e.info.clone()).collect()
        };
        out.sort_by_key(|m| m.id);
        out
    }

    fn stop(
        &self,
        id: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<MonitorInfo>>> + Send + '_>> {
        Box::pin(async move {
            let (stopped_info, target, task, snapshot) = {
                let mut g = self.inner.lock().unwrap();
                let Some(entry) = g.monitors.get_mut(&id) else {
                    return Ok(None);
                };
                let was_active = entry.info.active;
                entry.info.active = false;
                let _ = entry.cancel_tx.send(true);
                let task = entry.task.take();
                let info = entry.info.clone();
                let target = entry.target.clone();
                let snap = snapshot_locked(&g);
                if !was_active && task.is_none() {
                    return Ok(Some(info));
                }
                (info, target, task, snap)
            };

            if let Some(handle) = task {
                handle.abort();
            }
            if stopped_info.owns_job {
                let remote: Remote<Box<dyn AgentDialer>> =
                    Remote::new(dialer_for(&self.data_dir, &target, None));
                let _ = remote.cancel(&target, stopped_info.job_id).await;
            }
            if let Some(rt) = self.runtime.upgrade() {
                rt.publish_monitors_sync(&self.session_id, snapshot);
            }
            Ok(Some(stopped_info))
        })
    }
}

async fn poll_monitor_loop(
    weak_rt: Weak<Runtime>,
    session_id: String,
    data_dir: PathBuf,
    spec: MonitorStartSpec,
    initial_info: MonitorInfo,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let remote: Remote<Box<dyn AgentDialer>> =
        Remote::new(dialer_for(&data_dir, &spec.target, None));
    let debounce = Duration::from_millis(spec.debounce_ms.clamp(100, 30_000));
    let mut cursor = 0u64;
    let mut decoder = LineAccumulator::default();
    let mut pending_matches: Vec<String> = Vec::new();
    let mut first_match_at: Option<Instant> = None;
    let mut consecutive_errors = 0u32;

    loop {
        if *cancel_rx.borrow() {
            return;
        }

        let mut chunk = Vec::<u8>::new();
        let poll = remote
            .logs(&spec.target, spec.job_id, cursor, false, |records: &[LogRecord]| {
                for r in records {
                    chunk.extend_from_slice(&r.data);
                }
            })
            .await;

        let finished = match poll {
            Ok((next, fin)) => {
                consecutive_errors = 0;
                cursor = next;
                fin
            }
            Err(e) => {
                consecutive_errors += 1;
                debug!(
                    monitor = initial_info.id,
                    attempt = consecutive_errors,
                    error = %e,
                    "monitor log poll failed"
                );
                if consecutive_errors >= 5 {
                    Some(-1)
                } else {
                    None
                }
            }
        };

        for line in decoder.push_bytes(&chunk, finished.is_some()) {
            if matches_pattern(&line, &spec.pattern) {
                if first_match_at.is_none() {
                    first_match_at = Some(Instant::now());
                }
                if pending_matches.len() < MAX_MATCHED_LINES_PER_FIRE {
                    pending_matches.push(line);
                }
            }
        }

        let debounce_ready = first_match_at.is_some_and(|t| t.elapsed() >= debounce);
        if !pending_matches.is_empty() && (debounce_ready || finished.is_some()) {
            let matched = std::mem::take(&mut pending_matches);
            first_match_at = None;
            let Some(rt) = weak_rt.upgrade() else {
                return;
            };
            let Some(hub) = rt.monitor_hub(&session_id) else {
                return;
            };
            let Some((mut info, reached_cap, mut snap)) = hub.record_trigger(initial_info.id)
            else {
                return;
            };

            let mut stopped = reached_cap;
            if finished.is_some() && !reached_cap {
                if let Some((fin_info, fin_snap)) = hub.mark_finished(initial_info.id) {
                    info = fin_info;
                    snap = fin_snap;
                }
                stopped = true;
            }
            if reached_cap && info.owns_job && finished.is_none() {
                let _ = remote.cancel(&spec.target, info.job_id).await;
            }

            let agent_msg = format_monitor_agent_message(&info, &matched, finished, stopped);
            rt.deliver_monitor_fire(
                &session_id,
                info,
                matched,
                stopped,
                snap,
                agent_msg,
            );
            if stopped {
                return;
            }
        } else if finished.is_some() {
            if let Some(rt) = weak_rt.upgrade() {
                if let Some(hub) = rt.monitor_hub(&session_id) {
                    if let Some((_info, snap)) = hub.mark_finished(initial_info.id) {
                        rt.publish_monitors_sync(&session_id, snap);
                    }
                }
            }
            return;
        }

        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            res = cancel_rx.changed() => {
                if res.is_err() || *cancel_rx.borrow() {
                    return;
                }
            }
        }
    }
}

fn format_monitor_agent_message(
    info: &MonitorInfo,
    matched: &[String],
    exit_code: Option<i32>,
    stopped: bool,
) -> String {
    let mut out = format!(
        "<monitor_event id=\"{}\" label=\"{}\" job_id=\"{}\" pattern=\"{}\" trigger=\"{}\" stopped=\"{}\"",
        info.id,
        info.label.replace('"', "'"),
        info.job_id,
        info.pattern.replace('"', "'"),
        info.trigger_count,
        stopped
    );
    if let Some(code) = exit_code {
        out.push_str(&format!(" job_exit=\"{code}\""));
    }
    out.push_str(">\n");
    for line in matched {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("</monitor_event>");
    out
}

#[derive(Default)]
struct LineAccumulator {
    partial: Vec<u8>,
}

impl LineAccumulator {
    fn push_bytes(&mut self, data: &[u8], flush_tail: bool) -> Vec<String> {
        let mut out = Vec::new();
        for &b in data {
            if b == b'\n' {
                let raw = std::mem::take(&mut self.partial);
                let line = clean_line(&raw);
                if !line.is_empty() {
                    out.push(line);
                }
            } else if b != b'\r' {
                self.partial.push(b);
                if self.partial.len() > 16 * 1024 {
                    let raw = std::mem::take(&mut self.partial);
                    let line = clean_line(&raw);
                    if !line.is_empty() {
                        out.push(line);
                    }
                }
            }
        }
        if flush_tail && !self.partial.is_empty() {
            let raw = std::mem::take(&mut self.partial);
            let line = clean_line(&raw);
            if !line.is_empty() {
                out.push(line);
            }
        }
        out
    }
}

fn clean_line(bytes: &[u8]) -> String {
    strip_ansi(&String::from_utf8_lossy(bytes))
        .trim_end()
        .to_string()
}

pub(crate) fn matches_pattern(line: &str, pattern: &str) -> bool {
    if line.trim().is_empty() {
        return false;
    }
    let alts = split_unescaped_pipes(pattern);
    if alts.is_empty() {
        return false;
    }
    let line_lower = line.to_ascii_lowercase();
    for alt in alts {
        let trimmed = alt.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "*" {
            return true;
        }
        if line_lower.contains(&trimmed.to_ascii_lowercase()) {
            return true;
        }
    }
    false
}

fn split_unescaped_pipes(pattern: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                if next == '|' || next == '\\' {
                    cur.push(next);
                    chars.next();
                    continue;
                }
            }
            cur.push(c);
        } else if c == '|' {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_matcher_handles_case_alternation_wildcard_and_escapes() {
        assert!(matches_pattern("ERROR: failed to bind", "error"));
        assert!(matches_pattern("thread panicked at src/main.rs", "warn|panic|fatal"));
        assert!(!matches_pattern("all tests passed", "error|panic"));
        assert!(matches_pattern("anything here", "*"));
        assert!(!matches_pattern("   ", "*"));
        assert!(matches_pattern("a|b token", r"a\|b"));
        assert!(!matches_pattern("a token", r"a\|b"));
    }

    #[test]
    fn line_accumulator_buffers_partial_lines_and_strips_ansi() {
        let mut acc = LineAccumulator::default();
        assert!(acc.push_bytes(b"\x1b[31mERR", false).is_empty());
        let lines = acc.push_bytes(b"OR\x1b[0m: boom\ntail", false);
        assert_eq!(lines, vec!["ERROR: boom"]);
        let tail = acc.push_bytes(b"", true);
        assert_eq!(tail, vec!["tail"]);
    }
}
