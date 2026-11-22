//! The daemon's job table: queueing, concurrency, cancellation, persistence.
//!
//! A job outlives the request that created it and every stream that watches
//! it. `Jobs` is the execution layer; nothing in here knows about transports,
//! callers' sockets, or protocol framing.

use crate::exec::{self, ExecSpec};
use crate::fs;
use crate::log::{self, JobLogs, LogWriter};
use anyhow::{bail, Result};
use jade_proto::{
    Job, JobId, JobResult, JobState, SandboxPolicy, SubmitRequest, DEFAULT_GROUP, MONITOR_GROUP,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// Finished jobs kept in the table (and their logs on disk).
pub const KEEP_JOBS: usize = 100;
/// Independent concurrency cap for long-running background monitor jobs.
pub const MAX_MONITOR_JOBS: usize = 16;
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
    sandbox: SandboxPolicy,
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
    /// Cross-group cap on jobs in `Running`. Also the per-group default.
    max_jobs: AtomicUsize,
    /// Once set, `schedule` starts nothing and `submit` fails.
    shutting_down: AtomicBool,
    table: Mutex<Table>,
    persist_lock: Mutex<()>,
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
        let max_jobs = max_jobs.clamp(1, 64);

        let mut table = Table {
            next_id: 1,
            jobs: BTreeMap::new(),
            groups: BTreeMap::new(),
        };
        table.groups.insert(DEFAULT_GROUP.to_string(), max_jobs);
        table
            .groups
            .insert(MONITOR_GROUP.to_string(), MAX_MONITOR_JOBS);

        let state_path = logs.dir().join(STATE_FILE);
        // A corrupt state must not be treated as an empty table: pruning would
        // delete every existing log before the next id is ever used.
        let mut state_loaded = false;
        if let Ok(text) = std::fs::read_to_string(&state_path) {
            match serde_json::from_str::<Vec<Job>>(&text) {
                Ok(jobs) => {
                    state_loaded = true;
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
                                sandbox: SandboxPolicy::Off,
                                pid_slot: None,
                                cancel: None,
                                cancelled: false,
                            },
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "job state.json is unreadable; log file ids are preserved"
                    );
                }
            }
        }
        // Never reuse an id that already has a log file. A corrupt or missing
        // state.json used to reset `next_id` to 1 and overwrite `1.log`.
        let floor = max_existing_log_id(&logs.dir());
        if floor >= table.next_id {
            table.next_id = floor + 1;
        }

        let jobs = Self {
            inner: Arc::new(Inner {
                logs,
                max_jobs: AtomicUsize::new(max_jobs),
                shutting_down: AtomicBool::new(false),
                table: Mutex::new(table),
                persist_lock: Mutex::new(()),
                wake: Arc::new(Notify::new()),
            }),
        };
        if state_loaded {
            jobs.prune();
        }
        jobs
    }

    pub fn logs(&self) -> &JobLogs {
        &self.inner.logs
    }

    /// Shared wake-up signal for log followers.
    pub fn wake(&self) -> Arc<Notify> {
        self.inner.wake.clone()
    }

    /// Apply a live policy change to every group's concurrency limit and the
    /// cross-group cap. Lowering it never kills running jobs; it only stops
    /// starting new ones.
    pub fn set_max_jobs(&self, max_jobs: usize) {
        let max_jobs = max_jobs.clamp(1, 64);
        let previous = self.inner.max_jobs.swap(max_jobs, Ordering::SeqCst);
        let changed = {
            let mut t = self.inner.table.lock().unwrap();
            let mut changed = previous != max_jobs;
            if t.groups.is_empty() {
                t.groups.insert(DEFAULT_GROUP.to_string(), max_jobs);
                changed = true;
            }
            for (group, limit) in t.groups.iter_mut() {
                if group == MONITOR_GROUP {
                    continue;
                }
                if *limit != max_jobs {
                    *limit = max_jobs;
                    changed = true;
                }
            }
            changed
        };
        if changed {
            self.schedule();
        }
    }

    /// Enqueue a job and start it as soon as its group has a slot.
    pub fn submit(&self, req: SubmitRequest, owner: &str) -> Result<Job> {
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            bail!("job table is shutting down");
        }
        let SubmitRequest {
            spec,
            cwd,
            env,
            label,
            group,
            timeout_s,
            sandbox,
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
                    sandbox,
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
        let mut jobs: Vec<Job> = {
            let t = self.inner.table.lock().unwrap();
            t.jobs.values().map(snapshot_entry).collect()
        };
        for job in &mut jobs {
            self.attach_log_bytes(job);
        }
        jobs
    }

    pub fn get(&self, id: JobId) -> Option<Job> {
        let mut job = {
            let t = self.inner.table.lock().unwrap();
            t.jobs.get(&id).map(snapshot_entry)
        }?;
        self.attach_log_bytes(&mut job);
        Some(job)
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

    /// Stop accepting work, kill what is running, and persist terminal states.
    ///
    /// The short pause lets in-flight tasks record their own result on a
    /// multi-thread runtime. Whatever is still open is then forced to
    /// `Killed` so `state.json` does not claim a live job after exit.
    pub fn shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let ids: Vec<JobId> = {
            let t = self.inner.table.lock().unwrap();
            t.jobs
                .iter()
                .filter(|(_, e)| !e.job.state.is_done())
                .map(|(id, _)| *id)
                .collect()
        };
        for id in ids {
            let _ = self.cancel(id);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        while std::time::Instant::now() < deadline {
            let busy = {
                let t = self.inner.table.lock().unwrap();
                t.jobs.values().any(|e| !e.job.state.is_done())
            };
            if !busy {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let stuck: Vec<JobId> = {
            let t = self.inner.table.lock().unwrap();
            t.jobs
                .iter()
                .filter(|(_, e)| !e.job.state.is_done())
                .map(|(id, _)| *id)
                .collect()
        };
        for id in stuck {
            self.finish(id, JobResult::Killed);
        }
        self.persist();
    }

    fn attach_log_bytes(&self, job: &mut Job) {
        job.log_bytes = std::fs::metadata(self.inner.logs.path(job.id))
            .map(|m| m.len())
            .unwrap_or(0);
    }

    /// Start every queued job whose group has capacity and whose start would
    /// not push the process-wide running count past `max_jobs`.
    fn schedule(&self) {
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        type ScheduledStart = (JobId, Job, Vec<(String, String)>, u64, SandboxPolicy);
        let global_limit = self.inner.max_jobs.load(Ordering::SeqCst).max(1);
        let starts: Vec<ScheduledStart> = {
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
                    let default = if e.job.group == MONITOR_GROUP {
                        MAX_MONITOR_JOBS
                    } else {
                        global_limit
                    };
                    let limit = t.groups.get(&e.job.group).copied().unwrap_or(default);
                    (e.job.group.clone(), limit.max(1))
                };
                if group != MONITOR_GROUP {
                    let interactive_running = t
                        .jobs
                        .values()
                        .filter(|e| {
                            e.job.group != MONITOR_GROUP
                                && matches!(e.job.state, JobState::Running { .. })
                        })
                        .count();
                    if interactive_running >= global_limit {
                        continue;
                    }
                }
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
                starts.push((
                    id,
                    e.job.clone(),
                    e.env.clone(),
                    e.timeout_s,
                    e.sandbox.clone(),
                ));
            }
            starts
        };
        if starts.is_empty() {
            return;
        }
        self.persist();
        for (id, job, env, timeout_s, sandbox) in starts {
            self.spawn(id, job, env, timeout_s, sandbox);
        }
    }

    fn spawn(
        &self,
        id: JobId,
        job: Job,
        env: Vec<(String, String)>,
        timeout_s: u64,
        sandbox: SandboxPolicy,
    ) {
        let cancel = Arc::new(Notify::new());
        let pid_slot = Arc::new(AtomicU32::new(0));
        let already_cancelled = {
            let mut t = self.inner.table.lock().unwrap();
            match t.jobs.get_mut(&id) {
                Some(e) if e.cancelled => true,
                Some(e) => {
                    e.cancel = Some(cancel.clone());
                    e.pid_slot = Some(pid_slot.clone());
                    false
                }
                None => true,
            }
        };
        if already_cancelled {
            self.finish(id, JobResult::Killed);
            return;
        }

        let spec = ExecSpec {
            argv: job.spec.program(),
            cwd: job.cwd,
            env,
            timeout_s,
            sandbox,
        };
        let path = self.inner.logs.path(id);
        let wake = self.inner.wake.clone();
        let jobs = self.clone();
        tokio::spawn(async move {
            if jobs.inner.shutting_down.load(Ordering::SeqCst) {
                jobs.finish(id, JobResult::Killed);
                return;
            }
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
        self.prune();
        self.persist();
        self.inner.wake.notify_waiters();
        self.schedule();
    }

    fn persist(&self) {
        let _guard = self.inner.persist_lock.lock().unwrap();
        let mut jobs: Vec<Job> = {
            let t = self.inner.table.lock().unwrap();
            t.jobs.values().map(snapshot_entry).collect()
        };
        for job in &mut jobs {
            self.attach_log_bytes(job);
        }
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
            if t.jobs.len() > KEEP_JOBS {
                let mut excess = t.jobs.len() - KEEP_JOBS;
                let done_ids: Vec<JobId> = t
                    .jobs
                    .iter()
                    .filter(|(_, e)| e.job.state.is_done())
                    .map(|(id, _)| *id)
                    .collect();
                for id in done_ids {
                    if excess == 0 {
                        break;
                    }
                    t.jobs.remove(&id);
                    excess -= 1;
                }
            }
            t.jobs.keys().copied().collect()
        };
        self.inner.logs.prune(&keep, |id| {
            self.inner.table.lock().unwrap().jobs.contains_key(&id)
        });
    }
}

fn max_existing_log_id(dir: &std::path::Path) -> JobId {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<JobId>().ok())
        })
        .max()
        .unwrap_or(0)
}

/// Tail helper shared by the server and tests: read until nothing is left,
/// with the job's completion state as the stop condition.
pub fn read_available(
    path: &std::path::Path,
    offset: u64,
    budget: usize,
) -> std::io::Result<(Vec<jade_proto::LogRecord>, u64)> {
    log::read_from(path, offset, budget)
}

fn snapshot_entry(entry: &Entry) -> Job {
    let mut job = entry.job.clone();
    if let Some(slot) = entry.pid_slot.as_ref() {
        let pid = slot.load(Ordering::SeqCst);
        if pid != 0 {
            job.pid = Some(pid);
        }
    }
    job
}

#[cfg(test)]
#[path = "job_tests.rs"]
mod job_tests;
