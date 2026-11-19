//! Command classification heuristics for parsed shell segments:
//! network usage, destructive/catastrophic flags, read-only detection,
//! and path argument extraction.

pub(super) fn base_bin(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

pub(super) fn segment_needs_net(argv: &[String]) -> bool {
    let Some(bin) = argv.first().map(|s| base_bin(s)) else {
        return false;
    };
    match bin {
        "curl" | "wget" | "ssh" | "scp" | "sftp" | "nc" | "ncat" | "telnet" | "ping" | "dig"
        | "nslookup" | "host" | "apt" | "apt-get" | "dnf" | "yum" | "pacman" | "brew"
        | "rustup" => true,
        "git" => argv.get(1).is_some_and(|s| {
            matches!(
                s.as_str(),
                "clone" | "fetch" | "pull" | "push" | "ls-remote"
            )
        }),
        "cargo" => argv.get(1).is_some_and(|s| {
            matches!(
                s.as_str(),
                "fetch" | "update" | "install" | "publish" | "search"
            )
        }),
        "npm" | "pnpm" | "yarn" | "pip" | "pip3" | "uv" => argv.get(1).is_some_and(|s| {
            matches!(
                s.as_str(),
                "install" | "i" | "add" | "update" | "upgrade" | "publish"
            )
        }),
        "gh" | "glab" => true,
        _ => false,
    }
}

pub(super) fn segment_is_destructive(argv: &[String]) -> bool {
    let Some(bin) = argv.first().map(|s| base_bin(s)) else {
        return false;
    };
    let joined = argv.join(" ");
    match bin {
        "mkfs" | "fdisk" | "parted" | "shutdown" | "reboot" | "poweroff" => true,
        "dd" => argv.iter().any(|a| a.starts_with("of=/dev/")),
        "rm" => {
            argv.iter().any(|a| a.contains('r') && a.starts_with('-'))
                || argv.iter().any(|a| a == "/" || a == "~" || a == "*")
        }
        "chmod" | "chown" => argv.iter().any(|a| a == "-R" || a == "--recursive"),
        "git" => {
            joined.contains("reset --hard")
                || joined.contains("push --force")
                || joined.contains("push -f")
                || joined.contains("clean -fd")
        }
        "kill" | "pkill" | "killall" => true,
        _ => false,
    }
}

pub(super) fn segment_is_catastrophic(argv: &[String]) -> bool {
    let Some(bin) = argv.first().map(|s| base_bin(s)) else {
        return false;
    };
    match bin {
        "mkfs" | "fdisk" | "parted" | "shutdown" | "reboot" | "poweroff" => true,
        "dd" => argv.iter().any(|a| a.starts_with("of=/dev/")),
        "rm" => argv.iter().any(|a| {
            let trimmed = a.trim_end_matches('/');
            const ROOT_DIRS: &[&str] = &[
                "", "~", "/*", "$HOME", "/bin", "/boot", "/dev", "/etc", "/home", "/lib", "/lib64",
                "/opt", "/proc", "/root", "/sbin", "/sys", "/usr", "/var",
            ];
            a == "/" || ROOT_DIRS.contains(&trimmed)
        }),
        _ => false,
    }
}

pub(super) fn classify_argv(argv: &[String]) -> (bool, Vec<String>, Vec<String>) {
    let Some(bin) = argv.first().map(|s| base_bin(s)) else {
        return (false, Vec::new(), Vec::new());
    };
    let mut refs = Vec::new();
    let mut writes = Vec::new();
    for arg in &argv[1..] {
        if arg.starts_with('-') || arg == "--" {
            continue;
        }
        if looks_like_path(arg) {
            refs.push(arg.clone());
        }
    }

    let is_ro = match bin {
        "ls" | "pwd" | "echo" | "printf" | "cat" | "head" | "tail" | "wc" | "grep" | "egrep"
        | "fgrep" | "rg" | "which" | "type" | "uname" | "id" | "whoami" | "date" | "true"
        | "false" | "stat" | "file" | "readlink" | "realpath" | "tree" | "diff" => true,
        "find" => !argv
            .iter()
            .any(|a| matches!(a.as_str(), "-exec" | "-execdir" | "-delete" | "-ok")),
        "git" => argv.get(1).is_some_and(|s| {
            matches!(
                s.as_str(),
                "status" | "log" | "diff" | "show" | "branch" | "rev-parse" | "ls-files"
            )
        }),
        "tee" => {
            writes.extend(refs.iter().cloned());
            false
        }
        "rm" | "mv" | "cp" | "touch" | "mkdir" | "rmdir" | "chmod" | "chown" | "ln"
        | "truncate" => {
            writes.extend(refs.iter().cloned());
            false
        }
        "sed" => {
            if argv.iter().any(|a| a == "-i" || a.starts_with("-i")) {
                writes.extend(refs.iter().cloned());
                false
            } else {
                true
            }
        }
        _ => false,
    };

    (is_ro, refs, writes)
}

fn looks_like_path(arg: &str) -> bool {
    if arg.is_empty()
        || arg.contains("://")
        || arg.contains('*')
        || arg.contains('?')
        || arg.starts_with("/dev/")
        || arg.starts_with("/proc/")
        || arg.starts_with("/sys/")
        || matches!(arg, "." | ".." | "/dev" | "/proc" | "/sys")
    {
        return false;
    }
    if arg.starts_with('/')
        || arg.starts_with("./")
        || arg.starts_with("../")
        || arg.starts_with("~/")
        || arg.starts_with("$HOME/")
        || (arg.starts_with('.') && arg.len() > 1)
    {
        return true;
    }
    if let Some((head, _)) = arg.split_once('/') {
        return !head.is_empty()
            && head
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    }
    false
}
