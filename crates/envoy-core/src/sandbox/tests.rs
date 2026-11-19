use super::bwrap::{build_bwrap_with_secrets, find_bwrap};
use super::landlock::{
    build_landlock, handled_fs_mask, FS_READ, FS_WRITE, LANDLOCK_ACCESS_FS_EXECUTE,
    LANDLOCK_ACCESS_FS_IOCTL_DEV, LANDLOCK_ACCESS_FS_REFER, LANDLOCK_ACCESS_FS_REMOVE_DIR,
    LANDLOCK_ACCESS_FS_TRUNCATE,
};
use super::*;

fn tmpdir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("envoy-sb-{tag}-{}-{seq}-{n}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap_or(p)
}

#[test]
fn off_and_danger_leave_argv_untouched() {
    let argv = vec!["sh".into(), "-c".into(), "echo ok".into()];
    let off = prepare_command(&argv, "/tmp", &SandboxPolicy::Off).unwrap();
    assert_eq!(off.argv, argv);
    assert!(off.landlock.is_none());

    let danger = prepare_command(&argv, "/tmp", &SandboxPolicy::DangerFullAccess).unwrap();
    assert_eq!(danger.argv, argv);
}

#[test]
fn sandbox_capabilities_distinguish_landlock_from_bwrap_and_seatbelt() {
    let bwrap_caps = SandboxCapabilities::for_backend(&SandboxBackend::Bwrap("/bin/bwrap".into()));
    assert!(bwrap_caps.enforces_workspace_boundary());
    assert!(bwrap_caps.can_isolate_ipc);

    let seat_caps =
        SandboxCapabilities::for_backend(&SandboxBackend::Seatbelt("/usr/bin/sandbox-exec".into()));
    assert!(seat_caps.enforces_workspace_boundary());

    let ll4 = SandboxCapabilities::for_backend(&SandboxBackend::Landlock(4));
    assert!(!ll4.can_hide_secrets);
    assert!(!ll4.can_protect_subpaths);
    assert!(ll4.can_isolate_net);
    assert!(!ll4.enforces_workspace_boundary());

    let ll3 = SandboxCapabilities::for_backend(&SandboxBackend::Landlock(3));
    assert!(!ll3.can_isolate_net);
    assert!(!ll3.enforces_workspace_boundary());

    let none = SandboxCapabilities::for_backend(&SandboxBackend::None);
    assert!(!none.enforces_workspace_boundary());
}

#[test]
fn landlock_bitmasks_exclude_remove_dir_from_read_and_include_truncate() {
    assert_eq!(FS_READ & LANDLOCK_ACCESS_FS_REMOVE_DIR, 0);
    assert_ne!(FS_WRITE & LANDLOCK_ACCESS_FS_REMOVE_DIR, 0);
    assert_ne!(FS_WRITE & LANDLOCK_ACCESS_FS_TRUNCATE, 0);
    assert_ne!(FS_WRITE & LANDLOCK_ACCESS_FS_IOCTL_DEV, 0);

    let m1 = handled_fs_mask(1);
    assert_eq!(m1 & LANDLOCK_ACCESS_FS_REFER, 0);
    assert_eq!(m1 & LANDLOCK_ACCESS_FS_TRUNCATE, 0);
    assert_eq!(m1 & LANDLOCK_ACCESS_FS_IOCTL_DEV, 0);

    let m3 = handled_fs_mask(3);
    assert_ne!(m3 & LANDLOCK_ACCESS_FS_REFER, 0);
    assert_ne!(m3 & LANDLOCK_ACCESS_FS_TRUNCATE, 0);
    assert_eq!(m3 & LANDLOCK_ACCESS_FS_IOCTL_DEV, 0);

    let m5 = handled_fs_mask(5);
    assert_ne!(m5 & LANDLOCK_ACCESS_FS_IOCTL_DEV, 0);
    assert_ne!(m5 & LANDLOCK_ACCESS_FS_EXECUTE, 0);
}

#[test]
fn landlock_plan_omits_root_and_home_and_creates_private_scratch() {
    let dir = tmpdir("ll-plan");
    let argv = vec!["sh".into(), "-c".into(), "echo ok".into()];
    let prep = build_landlock(4, &argv, &dir, &[], false, false).unwrap();
    let plan = prep.landlock.expect("landlock plan");
    assert!(!plan.ro_paths.iter().any(|p| p == Path::new("/")));
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        assert!(!plan.ro_paths.iter().any(|p| p == &home));
        assert!(!plan.ro_paths.iter().any(|p| p == &home.join(".ssh")));
        assert!(!plan.ro_paths.iter().any(|p| p == &home.join(".envoy")));
    }
    let scratch = prep.scratch_dir.expect("scratch dir");
    assert!(plan.rw_paths.iter().any(|p| p == &scratch));
    let _ = std::fs::remove_dir_all(&scratch);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bwrap_places_namespace_flags_and_masks_secrets_after_bind() {
    let Some(bwrap) = find_bwrap() else {
        return;
    };
    let fake_home = tmpdir("bwrap-home");
    let envoy_dir = fake_home.join(".envoy");
    let conduit_dir = fake_home.join(".conduit");
    let ssh_dir = fake_home.join(".ssh");
    let nested_dir = fake_home.join("proj").join("sub");
    std::fs::create_dir_all(&envoy_dir).unwrap();
    std::fs::create_dir_all(&conduit_dir).unwrap();
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::fs::create_dir_all(&nested_dir).unwrap();
    std::fs::write(envoy_dir.join("auth.json"), "{}").unwrap();
    std::fs::write(nested_dir.join(".env"), "A=1\n").unwrap();
    std::fs::write(nested_dir.join(".ENV.prod"), "B=2\n").unwrap();
    // Simulate git worktree/submodule `.git` regular file.
    std::fs::write(fake_home.join("proj").join(".git"), "gitdir: /tmp/x\n").unwrap();

    let argv = vec!["sh".into(), "-c".into(), "pwd".into()];
    let prep = build_bwrap_with_secrets(
        &bwrap,
        &argv,
        &fake_home,
        &[fake_home.join("proj").display().to_string()],
        true,
        false,
        &[envoy_dir.clone(), conduit_dir.clone(), ssh_dir.clone()],
        &[],
    )
    .unwrap();

    for flag in [
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--new-session",
        "--unshare-net",
        "--die-with-parent",
    ] {
        assert!(
            prep.argv.iter().any(|a| a == flag),
            "missing {flag} in {:?}",
            prep.argv
        );
    }

    let bind_idx = prep
        .argv
        .windows(2)
        .position(|w| w[0] == "--bind" && w[1] == fake_home.display().to_string())
        .expect("missing home bind");
    for secret in [&envoy_dir, &conduit_dir, &ssh_dir] {
        let tmpfs_idx = prep
            .argv
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && w[1] == secret.display().to_string())
            .expect("missing secret tmpfs");
        assert!(
            tmpfs_idx > bind_idx,
            "secret {} masked before bind ({tmpfs_idx} <= {bind_idx})",
            secret.display()
        );
    }

    assert!(prep.argv.windows(3).any(|w| {
        w[0] == "--ro-bind"
            && w[1] == fake_home.join("proj/.git").display().to_string()
            && w[2] == fake_home.join("proj/.git").display().to_string()
    }));
    assert!(prep
        .argv
        .windows(3)
        .any(|w| w[0] == "--ro-bind" && w[2] == nested_dir.join(".env").display().to_string()));
    assert!(prep
        .argv
        .windows(3)
        .any(|w| w[0] == "--ro-bind" && w[2] == nested_dir.join(".ENV.prod").display().to_string()));
    assert!(!prep
        .argv
        .iter()
        .any(|a| a.contains("envoy-sandbox-empty")));

    let _ = std::fs::remove_dir_all(&fake_home);
}

#[test]
fn workspace_write_builds_offline_bwrap_protecting_git_and_env() {
    let Some(bwrap) = find_bwrap() else {
        return;
    };
    let dir = tmpdir("bwrap-args");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();

    let argv = vec!["sh".into(), "-c".into(), "pwd".into()];
    let prep = prepare_command(
        &argv,
        dir.to_str().unwrap(),
        &SandboxPolicy::WorkspaceWrite {
            cwd: dir.display().to_string(),
            extra_rw: vec![],
            allow_net: false,
        },
    )
    .unwrap();

    assert_eq!(prep.argv[0], bwrap.display().to_string());
    assert!(prep.argv.contains(&"--unshare-net".to_string()));
    assert!(prep.argv.contains(&"--ro-bind".to_string()));
    assert!(prep.argv.contains(&dir.join(".git").display().to_string()));
    assert!(prep.argv.contains(&dir.join(".env").display().to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_runtime_blocks_nested_env_and_secret_dir_even_when_parent_bound() {
    let Some(bwrap) = find_bwrap() else {
        return;
    };
    let fake_home = tmpdir("bwrap-exec");
    let envoy_dir = fake_home.join(".envoy");
    let sub = fake_home.join("work/sub");
    std::fs::create_dir_all(&envoy_dir).unwrap();
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(envoy_dir.join("auth.json"), "SECRET_AUTH").unwrap();
    std::fs::write(sub.join(".env"), "SECRET_ENV").unwrap();
    std::fs::write(fake_home.join("work/.git"), "gitdir: ../repo\n").unwrap();

    let script = format!(
        "cat {}/auth.json 2>/dev/null; \
         cat {}/.env 2>/dev/null; \
         echo nope >> {}/.git 2>/dev/null && echo GIT_MUTATED; \
         echo DONE",
        envoy_dir.display(),
        sub.display(),
        fake_home.join("work").display()
    );
    let prep = build_bwrap_with_secrets(
        &bwrap,
        &["/bin/sh".into(), "-c".into(), script],
        &fake_home,
        &[],
        true,
        false,
        std::slice::from_ref(&envoy_dir),
        &[],
    )
    .unwrap();
    let out = std::process::Command::new(&prep.argv[0])
        .args(&prep.argv[1..])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("SECRET_AUTH"), "{stdout}");
    assert!(!stdout.contains("SECRET_ENV"), "{stdout}");
    assert!(!stdout.contains("GIT_MUTATED"), "{stdout}");
    assert!(stdout.contains("DONE"), "{stdout}");
    let _ = std::fs::remove_dir_all(&fake_home);
}

#[cfg(target_os = "linux")]
#[test]
fn landlock_readonly_blocks_rmdir_and_truncate() {
    use std::os::unix::process::CommandExt;
    let Some(abi) = super::landlock::landlock_abi() else {
        return;
    };
    let work = tmpdir("ll-ro");
    let target_dir = work.join("keep_dir");
    let target_file = work.join("keep.txt");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(&target_file, b"untouched").unwrap();

    let prep = build_landlock(
        abi,
        &[
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "rmdir '{}' 2>/dev/null && echo RMDIR_OK; \
                 truncate -s 0 '{}' 2>/dev/null && echo TRUNC_OK; \
                 echo DONE",
                target_dir.display(),
                target_file.display()
            ),
        ],
        &work,
        &[],
        false,
        true,
    )
    .unwrap();
    let plan = prep.landlock.unwrap();
    let mut cmd = std::process::Command::new(&prep.argv[0]);
    cmd.args(&prep.argv[1..]);
    if let Some(ref s) = prep.scratch_dir {
        cmd.env("TMPDIR", s);
    }
    unsafe {
        cmd.pre_exec(move || apply_landlock_in_child(&plan));
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("RMDIR_OK"), "{stdout}");
    assert!(!stdout.contains("TRUNC_OK"), "{stdout}");
    assert!(target_dir.exists());
    assert_eq!(std::fs::read(&target_file).unwrap(), b"untouched");
    if let Some(s) = prep.scratch_dir {
        let _ = std::fs::remove_dir_all(s);
    }
    let _ = std::fs::remove_dir_all(&work);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn seatbelt_wraps_workspace_write_or_refuses_without_a_backend() {
    let argv = vec!["sh".into(), "-c".into(), "echo ok".into()];
    let dir = tmpdir("seatbelt");
    let result = prepare_command(
        &argv,
        dir.to_str().unwrap(),
        &SandboxPolicy::WorkspaceWrite {
            cwd: dir.display().to_string(),
            extra_rw: vec![],
            allow_net: false,
        },
    );
    match probe_backend() {
        SandboxBackend::Seatbelt(bin) => {
            let prep = result.expect("seatbelt prepare");
            assert_eq!(prep.argv[0], bin.display().to_string());
            assert!(prep.argv.iter().any(|arg| arg == "-p"));
            assert!(prep.argv.iter().any(|arg| arg.contains("(deny network*)")));
            assert!(prep
                .argv
                .iter()
                .any(|arg| arg.contains("(deny file-write* (subpath")));
            assert!(prep.argv.iter().any(|arg| arg.starts_with("TMPDIR=")));
            assert!(prep.scratch_dir.is_some());
            assert!(prep.landlock.is_none());
            if let Some(scratch) = prep.scratch_dir {
                let _ = std::fs::remove_dir_all(scratch);
            }
        }
        SandboxBackend::None => {
            let err = result
                .expect_err("missing backend must fail closed")
                .to_string();
            assert!(err.contains("no sandbox backend"), "{err}");
        }
        other => panic!("unexpected non-linux backend {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prepare_command_with_env_uses_bwrap_setenv_and_filters_dangerous_vars() {
    assert!(is_dangerous_env_var("LD_PRELOAD"));
    assert!(is_dangerous_env_var("ld_library_path"));
    assert!(is_dangerous_env_var("DYLD_INSERT_LIBRARIES"));
    assert!(is_dangerous_env_var("BASH_ENV"));
    assert!(is_dangerous_env_var("PATH"));
    assert!(!is_dangerous_env_var("MY_SAFE_APP_VAR"));

    let Some(bwrap) = find_bwrap() else {
        return;
    };
    let dir = tmpdir("bwrap-env");
    let prep = build_bwrap_with_secrets(
        &bwrap,
        &["sh".into(), "-c".into(), "echo ok".into()],
        &dir,
        &[],
        true,
        false,
        &[],
        &[("MY_SAFE_APP_VAR".into(), "123".into())],
    )
    .unwrap();
    assert!(!prep.apply_env_in_child);
    let setenv_idx = prep
        .argv
        .windows(3)
        .position(|w| w[0] == "--setenv" && w[1] == "MY_SAFE_APP_VAR" && w[2] == "123")
        .expect("missing --setenv");
    let sep_idx = prep.argv.iter().position(|a| a == "--").unwrap();
    assert!(setenv_idx < sep_idx);
    let _ = std::fs::remove_dir_all(&dir);
}
