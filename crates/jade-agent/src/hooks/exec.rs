//! Process execution (with PGID timeout cleanup) and pure `HookContext` transition reducer.

use super::types::{
    HookContext, HookDecision, HookEvent, HookOutput, HookRule, HookStepStatus, HookStepTrace,
    StageCapability,
};
use jade_core::ScratchGuard;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const MAX_HOOK_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub enum HookExecOutcome {
    Exited {
        code: Option<i32>,
        stdout: String,
        stderr: String,
        stdout_truncated: bool,
    },
    TimedOut,
    SpawnError(String),
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut rd: R, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut truncated = false;
    while buf.len() < cap {
        match rd.read(&mut chunk).await {
            Ok(0) | Err(_) => return (buf, false),
            Ok(n) => {
                let take = n.min(cap - buf.len());
                buf.extend_from_slice(&chunk[..take]);
                if n > take {
                    truncated = true;
                    break;
                }
            }
        }
    }
    if !truncated && buf.len() >= cap {
        let mut probe = [0u8; 1];
        if matches!(rd.read(&mut probe).await, Ok(n) if n > 0) {
            truncated = true;
        }
    }
    (buf, truncated)
}

struct HookLaunch {
    argv: Vec<String>,
    scratch: Option<PathBuf>,
    cwd: Option<PathBuf>,
    landlock: Option<jade_core::sandbox::LandlockPlan>,
}

fn hook_launch(rule: &HookRule, ctx: &HookContext) -> Result<HookLaunch, String> {
    let raw = vec!["sh".to_string(), "-c".to_string(), rule.command.clone()];
    if rule.sandbox_off {
        return Ok(HookLaunch {
            argv: raw,
            scratch: None,
            cwd: ctx.cwd.exists().then(|| ctx.cwd.clone()),
            landlock: None,
        });
    }
    let cwd = ctx.cwd.display().to_string();
    let prepared = jade_core::prepare_command(
        &raw,
        &cwd,
        &jade_proto::SandboxPolicy::WorkspaceWrite {
            cwd: cwd.clone(),
            extra_rw: vec![],
            allow_net: false,
        },
    )
    .map_err(|err| format!("sandbox setup failed: {err:#}"))?;
    let cwd = prepared
        .cwd_override
        .map(PathBuf::from)
        .or_else(|| ctx.cwd.exists().then(|| ctx.cwd.clone()));
    Ok(HookLaunch {
        argv: prepared.argv,
        scratch: prepared.scratch_dir,
        cwd,
        landlock: prepared.landlock,
    })
}

fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let pgid = pid as libc::pid_t;
        if pgid > 0 {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Run a hook command with the serialized `HookContext` passed on `stdin`.
pub async fn run_hook_process(
    rule: &HookRule,
    ctx: &HookContext,
    timeout_override_ms: Option<u64>,
) -> (HookExecOutcome, u64) {
    let start = Instant::now();
    let timeout_ms = timeout_override_ms
        .map(|o| o.min(rule.timeout_ms))
        .unwrap_or(rule.timeout_ms);

    let launch = match hook_launch(rule, ctx) {
        Ok(launch) => launch,
        Err(err) => {
            return (
                HookExecOutcome::SpawnError(err),
                start.elapsed().as_millis() as u64,
            );
        }
    };
    let _scratch = ScratchGuard(launch.scratch.clone());
    let mut cmd = Command::new(&launch.argv[0]);
    cmd.args(&launch.argv[1..]);
    if let Some(cwd) = launch.cwd {
        cmd.current_dir(cwd);
    }
    cmd.env_clear();
    for key in [
        "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TERM", "TMPDIR",
    ] {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    if launch.landlock.is_some() {
        if let Some(ref scratch) = launch.scratch {
            let s = scratch.display().to_string();
            cmd.env("TMPDIR", &s).env("TMP", &s).env("TEMP", &s);
        }
    }
    cmd.env("JADE_HOOK_EVENT", ctx.event.label())
        .env("JADE_HOOK_ID", &rule.id)
        .env("JADE_SESSION_ID", &ctx.session_id)
        .env("JADE_CWD", ctx.cwd.display().to_string())
        .env("JADE_DEPTH", ctx.depth.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        let landlock = launch.landlock;
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(ref plan) = landlock {
                    jade_core::sandbox::apply_landlock_in_child(plan)?;
                }
                Ok(())
            });
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                HookExecOutcome::SpawnError(format!("spawn failed: {e}")),
                start.elapsed().as_millis() as u64,
            );
        }
    };

    let pid = child.id();
    let stdin_payload = serde_json::to_vec(ctx).unwrap_or_else(|_| b"{}".to_vec());
    let stdin_task = child.stdin.take().map(|mut stdin| {
        tokio::spawn(async move {
            let _ = stdin.write_all(&stdin_payload).await;
            let _ = stdin.shutdown().await;
        })
    });

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let wait_fut = async move {
        let stdout_task = tokio::spawn(async move {
            match stdout_pipe {
                Some(r) => read_capped(r, MAX_HOOK_OUTPUT_BYTES).await,
                None => (Vec::new(), false),
            }
        });
        let stderr_task = tokio::spawn(async move {
            match stderr_pipe {
                Some(r) => read_capped(r, MAX_HOOK_OUTPUT_BYTES).await.0,
                None => Vec::new(),
            }
        });
        let status = child.wait().await;
        let (out_bytes, stdout_truncated) = stdout_task.await.unwrap_or_default();
        let err_bytes = stderr_task.await.unwrap_or_default();
        (status, out_bytes, stdout_truncated, err_bytes)
    };

    let elapsed = || start.elapsed().as_millis() as u64;
    let res = match tokio::time::timeout(Duration::from_millis(timeout_ms), wait_fut).await {
        Ok((Ok(status), out_bytes, stdout_truncated, err_bytes)) => (
            HookExecOutcome::Exited {
                code: status.code(),
                stdout: String::from_utf8_lossy(&out_bytes).to_string(),
                stderr: String::from_utf8_lossy(&err_bytes).to_string(),
                stdout_truncated,
            },
            elapsed(),
        ),
        Ok((Err(e), _, _, _)) => {
            kill_process_group(pid);
            (
                HookExecOutcome::SpawnError(format!("wait failed: {e}")),
                elapsed(),
            )
        }
        Err(_) => {
            kill_process_group(pid);
            (HookExecOutcome::TimedOut, elapsed())
        }
    };
    if let Some(task) = stdin_task {
        task.abort();
    }
    res
}

/// Pure explicit state transition: applies one hook step's outcome onto `HookContext`.
pub fn apply_step_result(
    ctx: &mut HookContext,
    rule: &HookRule,
    outcome: HookExecOutcome,
    duration_ms: u64,
    already_halted: bool,
) {
    let mut mutated_keys = Vec::new();
    match outcome {
        HookExecOutcome::TimedOut => {
            let msg = format!("hook `{}` timed out after {}ms", rule.id, rule.timeout_ms);
            if rule.fail_closed && ctx.can_block && !already_halted {
                ctx.decision = HookDecision::Block {
                    reason: format!("{msg} (fail_closed)"),
                    by: rule.id.clone(),
                    scope: rule.scope,
                };
            }
            ctx.trace.push(HookStepTrace {
                id: rule.id.clone(),
                scope: rule.scope,
                priority: rule.priority,
                decl_index: rule.decl_index,
                status: HookStepStatus::TimedOut,
                exit_code: None,
                duration_ms,
                reason: msg,
                mutated_keys,
            });
        }
        HookExecOutcome::SpawnError(err) => {
            if rule.fail_closed && ctx.can_block && !already_halted {
                ctx.decision = HookDecision::Block {
                    reason: format!("hook `{}` error (fail_closed): {err}", rule.id),
                    by: rule.id.clone(),
                    scope: rule.scope,
                };
            }
            ctx.trace.push(HookStepTrace {
                id: rule.id.clone(),
                scope: rule.scope,
                priority: rule.priority,
                decl_index: rule.decl_index,
                status: HookStepStatus::Errored,
                exit_code: None,
                duration_ms,
                reason: err,
                mutated_keys,
            });
        }
        HookExecOutcome::Exited {
            code: Some(2),
            stderr,
            stdout,
            ..
        } => {
            let reason = first_non_empty(&[&stderr, &stdout])
                .unwrap_or_else(|| format!("blocked by hook `{}` (exit 2)", rule.id));
            if ctx.can_block && !already_halted {
                ctx.decision = HookDecision::Block {
                    reason: reason.clone(),
                    by: rule.id.clone(),
                    scope: rule.scope,
                };
                ctx.trace.push(HookStepTrace {
                    id: rule.id.clone(),
                    scope: rule.scope,
                    priority: rule.priority,
                    decl_index: rule.decl_index,
                    status: HookStepStatus::Blocked,
                    exit_code: Some(2),
                    duration_ms,
                    reason,
                    mutated_keys,
                });
            } else {
                ctx.trace.push(HookStepTrace {
                    id: rule.id.clone(),
                    scope: rule.scope,
                    priority: rule.priority,
                    decl_index: rule.decl_index,
                    status: HookStepStatus::Succeeded,
                    exit_code: Some(2),
                    duration_ms,
                    reason: format!("exit 2 ignored (non-blocking/halted): {reason}"),
                    mutated_keys,
                });
            }
        }
        HookExecOutcome::Exited {
            code: Some(0),
            stdout,
            stdout_truncated,
            ..
        } => {
            apply_zero_exit(
                ctx,
                rule,
                &stdout,
                stdout_truncated,
                duration_ms,
                already_halted,
                &mut mutated_keys,
            );
        }
        HookExecOutcome::Exited {
            code,
            stderr,
            stdout,
            ..
        } => {
            let detail = first_non_empty(&[&stderr, &stdout]).unwrap_or_else(|| {
                format!(
                    "exited with {}",
                    code.map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into())
                )
            });
            if rule.fail_closed && ctx.can_block && !already_halted {
                ctx.decision = HookDecision::Block {
                    reason: format!("hook `{}` failed (fail_closed): {detail}", rule.id),
                    by: rule.id.clone(),
                    scope: rule.scope,
                };
            }
            ctx.trace.push(HookStepTrace {
                id: rule.id.clone(),
                scope: rule.scope,
                priority: rule.priority,
                decl_index: rule.decl_index,
                status: HookStepStatus::Errored,
                exit_code: code,
                duration_ms,
                reason: detail,
                mutated_keys,
            });
        }
    }
}

fn first_non_empty(parts: &[&str]) -> Option<String> {
    for p in parts {
        let t = p.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

fn apply_zero_exit(
    ctx: &mut HookContext,
    rule: &HookRule,
    stdout: &str,
    stdout_truncated: bool,
    duration_ms: u64,
    already_halted: bool,
    mutated_keys: &mut Vec<String>,
) {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        ctx.trace.push(HookStepTrace {
            id: rule.id.clone(),
            scope: rule.scope,
            priority: rule.priority,
            decl_index: rule.decl_index,
            status: HookStepStatus::Succeeded,
            exit_code: Some(0),
            duration_ms,
            reason: String::new(),
            mutated_keys: std::mem::take(mutated_keys),
        });
        return;
    }

    if !trimmed.starts_with('{') {
        if !already_halted && ctx.event != HookEvent::SessionEnd {
            ctx.additional_context.push(trimmed.to_string());
            mutated_keys.push("additional_context".into());
        }
        ctx.trace.push(HookStepTrace {
            id: rule.id.clone(),
            scope: rule.scope,
            priority: rule.priority,
            decl_index: rule.decl_index,
            status: HookStepStatus::Succeeded,
            exit_code: Some(0),
            duration_ms,
            reason: String::new(),
            mutated_keys: std::mem::take(mutated_keys),
        });
        return;
    }

    let parsed: HookOutput = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            let reason = format!(
                "{} JSON stdout ignored: {e}",
                if stdout_truncated {
                    "truncated"
                } else {
                    "invalid"
                }
            );
            let status = if rule.fail_closed {
                if ctx.can_block && !already_halted {
                    ctx.decision = HookDecision::Block {
                        reason: format!("hook `{}` returned {reason} (fail_closed)", rule.id),
                        by: rule.id.clone(),
                        scope: rule.scope,
                    };
                }
                HookStepStatus::Errored
            } else {
                HookStepStatus::Succeeded
            };
            ctx.trace.push(HookStepTrace {
                id: rule.id.clone(),
                scope: rule.scope,
                priority: rule.priority,
                decl_index: rule.decl_index,
                status,
                exit_code: Some(0),
                duration_ms,
                reason,
                mutated_keys: std::mem::take(mutated_keys),
            });
            return;
        }
    };

    if !already_halted {
        for key in &parsed.unset_state_keys {
            if ctx.state.remove(key).is_some() {
                mutated_keys.push(format!("-state.{key}"));
            }
        }
        if let Some(patch) = parsed.state_patch {
            for (k, v) in patch {
                mutated_keys.push(format!("state.{k}"));
                ctx.state.insert(k, v);
            }
        }
        if let Some(input) = parsed.updated_input {
            if ctx.event == HookEvent::PreToolUse {
                ctx.tool_input = Some(input);
                mutated_keys.push("tool_input".into());
            }
        }
        if let Some(prompt) = parsed.updated_prompt {
            if ctx.event == HookEvent::UserPromptSubmit {
                ctx.prompt = Some(prompt);
                mutated_keys.push("prompt".into());
            }
        }
        if let Some(extra) = parsed.additional_context {
            let t = extra.trim();
            if !t.is_empty() && ctx.event != HookEvent::SessionEnd {
                ctx.additional_context.push(t.to_string());
                mutated_keys.push("additional_context".into());
            }
        }
    }

    let mut status = HookStepStatus::Succeeded;
    let mut trace_reason = parsed.reason.clone().unwrap_or_default();

    if let Some(dec) = parsed.decision.as_deref().map(str::trim) {
        let dec_low = dec.to_ascii_lowercase();
        let reason = parsed
            .reason
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("{dec_low} by hook `{}`", rule.id));
        if already_halted || !ctx.can_block {
            if matches!(dec_low.as_str(), "ask" | "block" | "deny") {
                trace_reason = format!("decision `{dec_low}` ignored in non-blocking/halted stage");
            }
        } else {
            match dec_low.as_str() {
                "deny" => {
                    ctx.decision = HookDecision::Deny {
                        reason: reason.clone(),
                        by: rule.id.clone(),
                        scope: rule.scope,
                    };
                    status = HookStepStatus::Denied;
                    trace_reason = reason;
                }
                "block" => {
                    ctx.decision = HookDecision::Block {
                        reason: reason.clone(),
                        by: rule.id.clone(),
                        scope: rule.scope,
                    };
                    status = HookStepStatus::Blocked;
                    trace_reason = reason;
                }
                "ask" if ctx.event.capability() == StageCapability::BlockableWithAsk => {
                    let can_set_ask = match &ctx.decision {
                        HookDecision::Continue => true,
                        HookDecision::Ask { scope: prev, .. } => rule.scope < *prev,
                        HookDecision::Block { .. } | HookDecision::Deny { .. } => false,
                    };
                    if can_set_ask {
                        ctx.decision = HookDecision::Ask {
                            reason: reason.clone(),
                            by: rule.id.clone(),
                            scope: rule.scope,
                        };
                        status = HookStepStatus::Asked;
                        trace_reason = reason;
                    } else {
                        trace_reason = "lower-scope ask preserved higher-scope ask".into();
                    }
                }
                "ask" => {
                    trace_reason = "decision `ask` ignored in this stage".into();
                }
                "continue" | "allow" | "" => {
                    // Never downgrade an existing `Ask` back to `Continue`.
                }
                other => {
                    trace_reason = format!("unknown decision `{other}` ignored");
                }
            }
        }
    }

    ctx.trace.push(HookStepTrace {
        id: rule.id.clone(),
        scope: rule.scope,
        priority: rule.priority,
        decl_index: rule.decl_index,
        status,
        exit_code: Some(0),
        duration_ms,
        reason: trace_reason,
        mutated_keys: std::mem::take(mutated_keys),
    });
}

#[cfg(test)]
mod tests {
    use super::hook_launch;
    use crate::hooks::{HookContext, HookEvent, HookRule, HookScope};
    use std::collections::BTreeMap;

    fn rule(scope: HookScope, sandbox_off: bool) -> HookRule {
        HookRule {
            id: "h".into(),
            scope,
            priority: 0,
            decl_index: 0,
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "echo hi".into(),
            timeout_ms: 1000,
            fail_closed: false,
            always_run: false,
            sandbox_off,
        }
    }

    #[test]
    fn project_hooks_use_the_sandbox_and_user_hooks_do_not() {
        let dir = std::env::temp_dir().join(format!("jade-hook-sb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = HookContext::new(
            "s",
            dir.clone(),
            "local",
            "default",
            HookEvent::PreToolUse,
            BTreeMap::new(),
        );
        let project = hook_launch(&rule(HookScope::Project, false), &ctx);
        match jade_core::sandbox::probe_backend() {
            jade_core::sandbox::SandboxBackend::None => match project {
                Err(err) => assert!(err.contains("sandbox"), "{err}"),
                Ok(_) => panic!("missing backend must refuse the hook"),
            },
            jade_core::sandbox::SandboxBackend::Seatbelt(bin) => {
                let launch = project.expect("seatbelt");
                assert_eq!(launch.argv[0], bin.display().to_string());
                assert!(launch
                    .argv
                    .iter()
                    .any(|arg| arg.contains("(deny network*)")));
                if let Some(scratch) = launch.scratch {
                    let _ = std::fs::remove_dir_all(scratch);
                }
            }
            jade_core::sandbox::SandboxBackend::Bwrap(bin) => {
                let launch = project.expect("bwrap");
                assert_eq!(launch.argv[0], bin.display().to_string());
            }
            jade_core::sandbox::SandboxBackend::Landlock(_) => {
                let launch = project.expect("landlock");
                assert_eq!(launch.argv[0], "sh");
                assert!(launch.landlock.is_some());
                if let Some(scratch) = launch.scratch {
                    let _ = std::fs::remove_dir_all(scratch);
                }
            }
        }
        let user = hook_launch(&rule(HookScope::User, true), &ctx).unwrap();
        assert_eq!(
            user.argv,
            vec!["sh".to_string(), "-c".into(), "echo hi".into()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
