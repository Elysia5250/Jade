//! The daemon's job table: queueing, concurrency, cancellation, persistence.
//!
//! A job outlives the request that created it and every stream that watches
//! it. `Jobs` is the execution layer; nothing in here knows about transports,
//! callers' sockets, or protocol framing.

use crate::exec::{self, ExecSpec};
use crate::fs;
use crate::log::{self, JobLogs, LogWriter};
use anyhow::{bail, Result};
use envoy_proto::{Job, JobId, JobResult, JobState, SubmitRequest, DEFAULT_GROUP};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// Finished jobs kept in the table (and their logs on disk).
pub const KEEP_JOBS: usize = 100;
const STATE_FILE: &str = "state.json";

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct Entry {
    job: Job,
    env: Vec<(String, String)>,
    timeout_s: u64,
    pid_slot: Option<Arc<AtomicU32>>,
    cancel: Option<Arc<Notify>>,
    cancelled: bool,
}

struct Table {
    next_id: JobId,
    jobs: BTreeMap<JobId, Entry>,
    /// Per-group concurrency limits. Unknown groups fall back to `max_jobs`.
    groups: BTreeMap<String, usize>,
}

struct Inner {
    logs: JobLogs,
    max_jobs: usize,
    table: Mutex<Table>,
    /// Wakes log followers and anything else waiting on job state.
    wake: Arc<Notify>,
}

/// Cloneable handle to the job table. Shared by the server, the CLI daemon and
/// tests.
#[derive(Clone)]
pub struct Jobs {
    inner: Arc<Inner>,
}

impl Jobs {
    /// Open (or recover) the table under `root`.
    ///
    /// Jobs that were mid-flight when the daemon died are recovered as
    /// `Errored`: their processes are gone, and pretending otherwise would make
    /// `attach` hang forever.
    pub fn open(root: PathBuf, max_jobs: usize) -> Self {
        let logs = JobLogs::new(root);
        let _ = std::fs::create_dir_all(logs.dir());
        let max_jobs = max_jobs.max(1);

        let mut table = Table {
            next_id: 1,
            jobs: BTreeMap::new(),
            groups: BTreeMap::new(),
        };
        table.groups.insert(DEFAULT_GROUP.to_string(), max_jobs);

        let state_path = logs.dir().join(STATE_FILE);
        if let Ok(text) = std::fs::read_to_string(&state_path) {
            if let Ok(jobs) = serde_json::from_str::<Vec<Job>>(&text) {
                for mut job in jobs {
                    if !job.state.is_done() {
                        job.state = JobState::Done {
                            at_ms: now_ms(),
                            result: JobResult::Errored {
                                error: "daemon restarted while the job was in flight".into(),
                            },
                        };
                        job.pid = None;
                    }
                    table.next_id = table.next_id.max(job.id + 1);
                    table.jobs.insert(
                        job.id,
                        Entry {
                            job,
                            env: Vec::new(),
                            timeout_s: 0,
                            pid_slot: None,
                            cancel: None,
                            cancelled: false,
                        },
                    );
                }
            }
        }

        let jobs = Self {
            inner: Arc::new(Inner {
                logs,
                max_jobs,
                table: Mutex::new(table),
                wake: Arc::new(Notify::new()),
            }),
        };
        jobs.prune();
        jobs
    }

    pub fn logs(&self) -> &JobLogs {
        &self.inner.logs
    }

    /// Shared wake-up signal for log followers.
    pub fn wake(&self) -> Arc<Notify> {
        self.inner.wake.clone()
    }

    /// Apply a live policy change to the default group's concurrency limit.
    /// Lowering it never kills running jobs; it only stops starting new ones.
    pub fn set_max_jobs(&self, max_jobs: usize) {
        let max_jobs = max_jobs.max(1);
        let changed = {
            let mut t = self.inner.table.lock().unwrap();
            t.groups.insert(DEFAULT_GROUP.to_string(), max_jobs) != Some(max_jobs)
        };
        if changed {
            self.schedule();
        }
    }

    /// Enqueue a job and start it as soon as its group has a slot.
    pub fn submit(&self, req: SubmitRequest, owner: &str) -> Result<Job> {
        let SubmitRequest {
            spec,
            cwd,
            env,
            label,
            group,
            timeout_s,
        } = req;
        if spec.is_empty() {
            bail!("nothing to run");
        }
        let group = if group.trim().is_empty() {
            DEFAULT_GROUP.to_string()
        } else {
            group.trim().to_string()
        };
        let cwd = match cwd {
            Some(c) if !c.trim().is_empty() => fs::resolve(c.trim()).display().to_string(),
            _ => fs::home_dir(),
        };
        let label = label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty());

        let submitted = {
            let mut t = self.inner.table.lock().unwrap();
            let id = t.next_id;
            t.next_id += 1;
            let job = Job {
                id,
                owner: owner.to_string(),
                spec,
                cwd,
                label,
                group,
                created_ms: now_ms(),
                state: JobState::Queued { since_ms: now_ms() },
                pid: None,
                log_bytes: 0,
            };
            t.jobs.insert(
                id,
                Entry {
                    job: job.clone(),
                    env,
                    timeout_s,
                    pid_slot: None,
                    cancel: None,
                    cancelled: false,
                },
            );
            job
        };
        self.persist();
        self.schedule();
        Ok(self.get(submitted.id).unwrap_or(submitted))
    }

    /// All retained jobs, oldest first.
    pub fn list(&self) -> Vec<Job> {
        let t = self.inner.table.lock().unwrap();
        t.jobs.values().map(|e| self.snapshot(e)).collect()
    }

    pub fn get(&self, id: JobId) -> Option<Job> {
        let t = self.inner.table.lock().unwrap();
        t.jobs.get(&id).map(|e| self.snapshot(e))
    }

    pub fn log_path(&self, id: JobId) -> Option<PathBuf> {
        self.get(id).map(|_| self.inner.logs.path(id))
    }

    /// Cancel a queued or running job. Idempotent only for unknown ids: a
    /// finished job is reported as such instead of silently succeeding.
    pub fn cancel(&self, id: JobId) -> Result<()> {
        let (pid, cancel, queued) = {
            let mut t = self.inner.table.lock().unwrap();
            let Some(entry) = t.jobs.get_mut(&id) else {
                bail!("no such job: {id}");
            };
            if entry.job.state.is_done() {
                bail!("job {id} already finished");
            }
            entry.cancelled = true;
            let queued = matches!(entry.job.state, JobState::Queued { .. });
            let pid = entry
                .pid_slot
                .as_ref()
                .map(|s| s.load(Ordering::SeqCst))
                .unwrap_or(0);
            (pid, entry.cancel.clone(), queued)
        };

        if queued {
            // Never started: settle the state here, no process to kill.
            self.finish(id, JobResult::Killed);
            return Ok(());
        }
        if pid > 0 {
            exec::kill_group(pid);
        }
        if let Some(cancel) = cancel {
            cancel.notify_one();
        }
        Ok(())
    }

    /// Kill every running job (daemon shutdown). Orphaned children would
    /// otherwise keep running with no way to reach them.
    pub fn shutdown(&self) {
        let pids: Vec<u32> = {
            let t = self.inner.table.lock().unwrap();
            t.jobs
                .values()
                .filter(|e| matches!(e.job.state, JobState::Running { .. }))
                .filter_map(|e| e.pid_slot.as_ref().map(|s| s.load(Ordering::SeqCst)))
                .filter(|p| *p > 0)
                .collect()
        };
        for pid in pids {
            exec::kill_group(pid);
        }
    }

    fn snapshot(&self, entry: &Entry) -> Job {
        let mut job = entry.job.clone();
        if let Some(slot) = entry.pid_slot.as_ref() {
            let pid = slot.load(Ordering::SeqCst);
            if pid != 0 {
                job.pid = Some(pid);
            }
        }
        job.log_bytes = std::fs::metadata(self.inner.logs.path(job.id))
            .map(|m| m.len())
            .unwrap_or(0);
        job
    }

    /// Start every queued job whose group has capacity.
    fn schedule(&self) {
        let starts: Vec<(JobId, Job, Vec<(String, String)>, u64)> = {
            let mut t = self.inner.table.lock().unwrap();
            let queued: Vec<JobId> = t
                .jobs
                .iter()
                .filter(|(_, e)| matches!(e.job.state, JobState::Queued { .. }))
                .map(|(id, _)| *id)
                .collect();
            let mut starts = Vec::new();
            for id in queued {
                let (group, limit) = {
                    let e = t.jobs.get(&id).expect("id from the same map");
                    let limit = t
                        .groups
                        .get(&e.job.group)
                        .copied()
                        .unwrap_or(self.inner.max_jobs);
                    (e.job.group.clone(), limit.max(1))
                };
                let running = t
                    .jobs
                    .values()
                    .filter(|e| {
                        e.job.group == group && matches!(e.job.state, JobState::Running { .. })
                    })
                    .count();
                if running >= limit {
                    continue;
                }
                let e = t.jobs.get_mut(&id).expect("id from the same map");
                e.job.state = JobState::Running { since_ms: now_ms() };
                starts.push((id, e.job.clone(), e.env.clone(), e.timeout_s));
            }
            starts
        };
        if starts.is_empty() {
            return;
        }
        self.persist();
        for (id, job, env, timeout_s) in starts {
            self.spawn(id, job, env, timeout_s);
        }
    }

    fn spawn(&self, id: JobId, job: Job, env: Vec<(String, String)>, timeout_s: u64) {
        let cancel = Arc::new(Notify::new());
        let pid_slot = Arc::new(AtomicU32::new(0));
        {
            let mut t = self.inner.table.lock().unwrap();
            if let Some(e) = t.jobs.get_mut(&id) {
                e.cancel = Some(cancel.clone());
                e.pid_slot = Some(pid_slot.clone());
            }
        }

        let spec = ExecSpec {
            argv: job.spec.program(),
            cwd: job.cwd,
            env,
            timeout_s,
        };
        let path = self.inner.logs.path(id);
        let wake = self.inner.wake.clone();
        let jobs = self.clone();
        tokio::spawn(async move {
            let result = match LogWriter::create(path, wake) {
                Ok(log) => exec::run_job(spec, log, pid_slot, cancel).await,
                Err(e) => JobResult::Errored {
                    error: format!("opening the job log: {e}"),
                },
            };
            jobs.finish(id, result);
        });
    }

    fn finish(&self, id: JobId, mut result: JobResult) {
        let known = {
            let mut t = self.inner.table.lock().unwrap();
            match t.jobs.get_mut(&id) {
                Some(e) => {
                    if e.cancelled {
                        result = JobResult::Killed;
                    }
                    e.job.state = JobState::Done {
                        at_ms: now_ms(),
                        result,
                    };
                    e.job.pid = None;
                    e.cancel = None;
                    true
                }
                None => false,
            }
        };
        if !known {
            return;
        }
        self.persist();
        self.inner.wake.notify_waiters();
        self.schedule();
    }

    fn persist(&self) {
        let jobs: Vec<Job> = {
            let t = self.inner.table.lock().unwrap();
            t.jobs.values().map(|e| self.snapshot(e)).collect()
        };
        let dir = self.inner.logs.dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let Ok(text) = serde_json::to_string(&jobs) else {
            return;
        };
        let tmp = dir.join(format!(".state.json.tmp-{}", std::process::id()));
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(STATE_FILE));
        }
    }

    fn prune(&self) {
        let keep: Vec<JobId> = {
            let mut t = self.inner.table.lock().unwrap();
            let ids: Vec<JobId> = t.jobs.keys().copied().collect();
            if ids.len() > KEEP_JOBS {
                for id in &ids[..ids.len() - KEEP_JOBS] {
                    t.jobs.remove(id);
                }
            }
            t.jobs.keys().copied().collect()
        };
        self.inner.logs.prune(&keep);
    }
}

/// Tail helper shared by the server and tests: read until nothing is left,
/// with the job's completion state as the stop condition.
pub fn read_available(
    path: &std::path::Path,
    offset: u64,
    budget: usize,
) -> std::io::Result<(Vec<envoy_proto::LogRecord>, u64)> {
    log::read_from(path, offset, budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use envoy_proto::TaskSpec;
    use std::time::Duration;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-jobs-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn shell(command: &str) -> SubmitRequest {
        SubmitRequest {
            spec: TaskSpec::Shell {
                command: command.into(),
            },
            cwd: None,
            env: Vec::new(),
            label: None,
            group: DEFAULT_GROUP.into(),
            timeout_s: 10,
        }
    }

    async fn wait_done(jobs: &Jobs, id: JobId) -> Job {
        for _ in 0..600 {
            let job = jobs.get(id).expect("job exists");
            if job.is_done() {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} never finished");
    }

    #[tokio::test]
    async fn submit_returns_before_the_command_finishes() {
        let dir = tmpdir("detached");
        let jobs = Jobs::open(dir.clone(), 2);
        let job = jobs.submit(shell("sleep 0.3; echo late"), "local").unwrap();
        assert!(job.id > 0);
        // The call returned while the child is still running.
        assert!(!job.is_done(), "{:?}", job.state);
        let done = wait_done(&jobs, job.id).await;
        assert_eq!(done.state.exit_code(), Some(0));

        let (records, _) = log::read_from(&jobs.logs().path(job.id), 0, 64 * 1024).unwrap();
        let text: String = records
            .iter()
            .map(|r| String::from_utf8_lossy(&r.data).to_string())
            .collect();
        assert!(text.contains("late"), "{text}");
        assert!(done.log_bytes > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_group_limit_queues_instead_of_dropping() {
        let dir = tmpdir("queue");
        let jobs = Jobs::open(dir.clone(), 1);
        let a = jobs.submit(shell("sleep 0.25"), "local").unwrap();
        let b = jobs.submit(shell("echo second"), "local").unwrap();
        assert!(matches!(jobs.get(a.id).unwrap().state, JobState::Running { .. }));
        assert!(matches!(jobs.get(b.id).unwrap().state, JobState::Queued { .. }));

        let done_b = wait_done(&jobs, b.id).await;
        assert_eq!(done_b.state.exit_code(), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn raising_the_limit_starts_queued_jobs() {
        let dir = tmpdir("limit");
        let jobs = Jobs::open(dir.clone(), 1);
        let a = jobs.submit(shell("sleep 0.2"), "local").unwrap();
        let b = jobs.submit(shell("echo second"), "local").unwrap();
        assert!(matches!(jobs.get(b.id).unwrap().state, JobState::Queued { .. }));

        jobs.set_max_jobs(2);
        for _ in 0..200 {
            if matches!(
                jobs.get(b.id).unwrap().state,
                JobState::Running { .. } | JobState::Done { .. }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(wait_done(&jobs, b.id).await.state.exit_code(), Some(0));
        assert_eq!(wait_done(&jobs, a.id).await.state.exit_code(), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancel_kills_a_running_job_and_marks_it_killed() {
        let dir = tmpdir("cancel");
        let jobs = Jobs::open(dir.clone(), 2);
        let job = jobs.submit(shell("sleep 30"), "local").unwrap();
        // Wait until the pid is published.
        for _ in 0..200 {
            if jobs.get(job.id).and_then(|j| j.pid).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        jobs.cancel(job.id).unwrap();
        let done = wait_done(&jobs, job.id).await;
        assert!(matches!(
            done.state.result(),
            Some(JobResult::Killed)
        ), "{:?}", done.state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancel_of_a_queued_job_never_spawns_it() {
        let dir = tmpdir("cancel-queued");
        let jobs = Jobs::open(dir.clone(), 1);
        let running = jobs.submit(shell("sleep 0.3"), "local").unwrap();
        let queued = jobs.submit(shell("touch /tmp/should-not-exist-envoy"), "local").unwrap();
        jobs.cancel(queued.id).unwrap();
        let done = wait_done(&jobs, queued.id).await;
        assert!(matches!(done.state.result(), Some(JobResult::Killed)));
        wait_done(&jobs, running.id).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn state_survives_reopen_and_orphans_are_marked() {
        let dir = tmpdir("persist");
        let jobs = Jobs::open(dir.clone(), 2);
        let done = jobs.submit(shell("echo persisted"), "local").unwrap();
        wait_done(&jobs, done.id).await;
        drop(jobs);

        let reopened = Jobs::open(dir.clone(), 2);
        let job = reopened.get(done.id).expect("job survived the restart");
        assert_eq!(job.state.exit_code(), Some(0));
        assert!(job.command().contains("persisted"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failures_are_reported_not_hidden() {
        let dir = tmpdir("fail");
        let jobs = Jobs::open(dir.clone(), 2);
        let job = jobs.submit(shell("exit 7"), "local").unwrap();
        let done = wait_done(&jobs, job.id).await;
        assert_eq!(done.state.result(), Some(&JobResult::Failed { code: 7 }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
