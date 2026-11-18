//! Phase 1 stress tests for `envoy-core::Jobs`: high-concurrency queueing,
//! cancellation storms, log flooding, KEEP_JOBS pruning, and state persistence.

use envoy_core::{log, Jobs, KEEP_JOBS};
use envoy_proto::{JobId, SandboxPolicy, SubmitRequest, TaskSpec};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn tmpdir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("envoy-core-stress-{tag}-{n}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn shell_req(command: &str, group: &str) -> SubmitRequest {
    SubmitRequest {
        spec: TaskSpec::Shell {
            command: command.to_string(),
        },
        cwd: None,
        env: Vec::new(),
        label: None,
        group: group.to_string(),
        timeout_s: 30,
        sandbox: SandboxPolicy::Off,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_200_concurrent_jobs_with_cancellations_and_reopen() {
    let dir = tmpdir("jobs-200");
    let jobs = Jobs::open(dir.clone(), 8);
    let started = Instant::now();

    let total: usize = 200;
    let mut submit_handles = Vec::with_capacity(total);
    for idx in 0..total {
        let j = jobs.clone();
        submit_handles.push(tokio::spawn(async move {
            let group = match idx % 4 {
                0 => "default",
                1 => "build",
                2 => "test",
                _ => "lint",
            };
            let cmd = if idx % 4 == 0 {
                // Target for cancellation storm (either queued or running).
                "sleep 15".to_string()
            } else {
                format!("printf 'job-{idx}-ok\\n'; echo 'err-{idx}' >&2")
            };
            j.submit(shell_req(&cmd, group), "stress").unwrap().id
        }));
    }

    let mut ids: Vec<JobId> = Vec::with_capacity(total);
    for h in submit_handles {
        ids.push(h.await.unwrap());
    }

    // Cancel all sleep jobs concurrently while other jobs are actively running.
    let cancel_handles: Vec<_> = ids
        .iter()
        .enumerate()
        .filter(|(idx, _)| idx % 4 == 0)
        .map(|(_, &id)| {
            let j = jobs.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis((id % 7) * 5)).await;
                let _ = j.cancel(id);
            })
        })
        .collect();
    for h in cancel_handles {
        h.await.unwrap();
    }

    // Wait for every job still present in the live table to reach Done.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let list = jobs.list();
        let active = list.iter().filter(|j| !j.state.is_done()).count();
        if active == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "jobs stalled with {active} still active"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let kept = jobs.list();
    assert!(
        kept.len() <= KEEP_JOBS,
        "expected pruning to <= {KEEP_JOBS}, got {}",
        kept.len()
    );
    assert!(!kept.is_empty());
    drop(jobs);

    // Verify persisted state.json survives reopen cleanly with zero active orphans.
    let reopened = Jobs::open(dir.clone(), 8);
    let reopened_list = reopened.list();
    assert!(reopened_list.len() <= KEEP_JOBS);
    assert!(
        reopened_list.iter().all(|j| j.state.is_done()),
        "reopened table still had non-Done jobs"
    );

    eprintln!(
        "[stress_jobs] 200 jobs (50 cancelled) completed in {:?}, kept={}",
        started.elapsed(),
        reopened_list.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_large_log_flood_and_concurrent_readers() {
    let dir = tmpdir("log-flood");
    let jobs = Arc::new(Jobs::open(dir.clone(), 4));

    // Emit ~256 KiB of mixed stdout/stderr quickly across 4 concurrent jobs.
    let mut job_ids = Vec::new();
    for idx in 0..4 {
        let cmd = format!(
            "python3 -c 'import sys; [sys.stdout.write(f\"{idx}-line-{{i}}: \" + \"x\"*120 + \"\\n\") for i in range(500)]'"
        );
        job_ids.push(jobs.submit(shell_req(&cmd, "default"), "flood").unwrap().id);
    }

    // 8 concurrent followers reading from disk while writers append.
    let mut readers = Vec::new();
    for r in 0..8 {
        let j = Arc::clone(&jobs);
        let id = job_ids[r % job_ids.len()];
        readers.push(tokio::spawn(async move {
            let path = j.logs().path(id);
            let mut cursor = 0u64;
            let mut total_bytes = 0usize;
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if let Ok((recs, next)) = log::read_from(&path, cursor, 32 * 1024) {
                    for rec in recs {
                        total_bytes += rec.data.len();
                    }
                    cursor = next;
                }
                let done = j.get(id).map(|job| job.state.is_done()).unwrap_or(true);
                if done {
                    // Drain tail after completion.
                    while let Ok((recs, next)) = log::read_from(&path, cursor, 32 * 1024) {
                        if recs.is_empty() {
                            break;
                        }
                        for rec in recs {
                            total_bytes += rec.data.len();
                        }
                        cursor = next;
                    }
                    break;
                }
                assert!(Instant::now() < deadline, "log reader timed out");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            total_bytes
        }));
    }

    for r in readers {
        let bytes = r.await.unwrap();
        assert!(bytes > 50_000, "expected full log stream, got {bytes} bytes");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
