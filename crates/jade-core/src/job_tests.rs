use super::*;
use jade_proto::TaskSpec;
use std::time::Duration;

fn tmpdir(tag: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("jade-jobs-{tag}-{n}"));
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
        sandbox: SandboxPolicy::Off,
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
    assert!(matches!(
        jobs.get(a.id).unwrap().state,
        JobState::Running { .. }
    ));
    assert!(matches!(
        jobs.get(b.id).unwrap().state,
        JobState::Queued { .. }
    ));

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
    assert!(matches!(
        jobs.get(b.id).unwrap().state,
        JobState::Queued { .. }
    ));

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
    assert!(
        matches!(done.state.result(), Some(JobResult::Killed)),
        "{:?}",
        done.state
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cancel_of_a_queued_job_never_spawns_it() {
    let dir = tmpdir("cancel-queued");
    let jobs = Jobs::open(dir.clone(), 1);
    let running = jobs.submit(shell("sleep 0.3"), "local").unwrap();
    let queued = jobs
        .submit(shell("touch /tmp/should-not-exist-jade"), "local")
        .unwrap();
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

fn shell_in(command: &str, group: &str) -> SubmitRequest {
    let mut req = shell(command);
    req.group = group.into();
    req
}

#[tokio::test]
async fn global_cap_limits_running_jobs_across_groups() {
    let dir = tmpdir("global-cap");
    let jobs = Jobs::open(dir.clone(), 1);
    let a = jobs
        .submit(shell_in("sleep 0.4", "alpha"), "local")
        .unwrap();
    let b = jobs.submit(shell_in("echo beta", "beta"), "local").unwrap();
    assert!(
        matches!(jobs.get(a.id).unwrap().state, JobState::Running { .. }),
        "first group should run"
    );
    assert!(
        matches!(jobs.get(b.id).unwrap().state, JobState::Queued { .. }),
        "a second group must not bypass the global cap: {:?}",
        jobs.get(b.id).unwrap().state
    );
    jobs.set_max_jobs(2);
    assert_eq!(wait_done(&jobs, b.id).await.state.exit_code(), Some(0));
    assert_eq!(wait_done(&jobs, a.id).await.state.exit_code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn corrupt_state_does_not_reuse_existing_log_ids() {
    let dir = tmpdir("corrupt-state");
    let jobs_dir = dir.join("jobs");
    std::fs::create_dir_all(&jobs_dir).unwrap();
    std::fs::write(jobs_dir.join("15.log"), b"old-log").unwrap();
    std::fs::write(jobs_dir.join("state.json"), b"{not json").unwrap();
    let jobs = Jobs::open(dir.clone(), 2);
    let job = jobs.submit(shell("echo fresh"), "local").unwrap();
    assert!(job.id >= 16, "reused id {}", job.id);
    assert_eq!(std::fs::read(jobs_dir.join("15.log")).unwrap(), b"old-log");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn shutdown_persists_terminal_state_and_rejects_submit() {
    let dir = tmpdir("shutdown");
    let jobs = Jobs::open(dir.clone(), 2);
    let job = jobs.submit(shell("sleep 30"), "local").unwrap();
    for _ in 0..200 {
        if jobs.get(job.id).and_then(|j| j.pid).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    jobs.shutdown();
    let done = jobs.get(job.id).expect("job retained");
    assert!(done.state.is_done(), "{:?}", done.state);
    assert!(
        matches!(done.state.result(), Some(JobResult::Killed)),
        "{:?}",
        done.state
    );
    assert!(jobs.submit(shell("echo no"), "local").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn monitor_jobs_do_not_starve_interactive_jobs() {
    let dir = tmpdir("monitor-pool");
    let jobs = Jobs::open(dir.clone(), 1);
    let mut m1_req = shell("sleep 30");
    m1_req.group = MONITOR_GROUP.to_string();
    let mut m2_req = shell("sleep 30");
    m2_req.group = MONITOR_GROUP.to_string();

    let m1 = jobs.submit(m1_req, "local").unwrap();
    let m2 = jobs.submit(m2_req, "local").unwrap();
    assert!(matches!(m1.state, JobState::Running { .. }));
    assert!(matches!(m2.state, JobState::Running { .. }));

    // Even with 2 running monitors and max_jobs=1, interactive exec starts immediately.
    let interactive = jobs.submit(shell("echo ok"), "local").unwrap();
    assert!(
        matches!(interactive.state, JobState::Running { .. }),
        "monitor jobs must not block interactive exec slots: {:?}",
        interactive.state
    );

    let mut finished = false;
    for _ in 0..200 {
        if let Some(j) = jobs.get(interactive.id) {
            if matches!(j.state.result(), Some(JobResult::Success)) {
                finished = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    jobs.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(finished, "interactive job should finish while monitors run");
}
