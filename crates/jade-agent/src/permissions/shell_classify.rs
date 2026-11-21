//! Command classification heuristics and lexical word/redirection tokenizer
//! for parsed shell segments.

use anyhow::{bail, Result};

#[derive(Clone, Copy)]
pub(super) enum RedirKind {
    Read,
    Write,
    Heredoc,
}

pub(super) fn base_bin(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

pub(super) fn segment_needs_net(argv: &[String]) -> bool {
    let Some(bin) = argv.first().map(|s| base_bin(s)) else {
        return false;
    };
    match bin {
        "curl" | "wget" | "ssh" | "scp" | "sftp" | "nc" | "ncat" | "socat" | "telnet" | "ping"
        | "dig" | "nslookup" | "host" | "apt" | "apt-get" | "dnf" | "yum" | "pacman" | "brew"
        | "rustup" => true,
        "openssl" => argv
            .get(1)
            .is_some_and(|s| matches!(s.as_str(), "s_client" | "ocsp")),
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
            (joined.contains("branch -D")
                || (joined.contains("branch")
                    && joined.contains("--delete")
                    && (joined.contains("--force") || argv.iter().any(|a| a == "-f"))))
                || joined.contains("reset --hard")
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
    let mut skip_next_filter = false;
    let mut take_next_write = false;
    for arg in &argv[1..] {
        if take_next_write {
            take_next_write = false;
            refs.push(arg.clone());
            writes.push(arg.clone());
            continue;
        }
        if skip_next_filter {
            skip_next_filter = false;
            continue;
        }
        if bin == "find"
            && matches!(
                arg.as_str(),
                "-name"
                    | "-iname"
                    | "-path"
                    | "-ipath"
                    | "-wholename"
                    | "-iwholename"
                    | "-regex"
                    | "-iregex"
            )
        {
            skip_next_filter = true;
            continue;
        }
        if bin == "find" && matches!(arg.as_str(), "-fprint" | "-fprint0" | "-fprintf" | "-fls") {
            take_next_write = true;
            continue;
        }
        if arg == "--output" {
            take_next_write = true;
            continue;
        }
        if let Some(target) = arg.strip_prefix("--output=") {
            if !target.is_empty() {
                refs.push(target.to_string());
                writes.push(target.to_string());
            }
            continue;
        }
        if arg.starts_with('-') || arg == "--" {
            continue;
        }
        if looks_like_path(arg) {
            refs.push(arg.clone());
        }
    }

    let is_ro = match bin {
        "ls" | "pwd" | "echo" | "printf" | "cat" | "head" | "tail" | "wc" | "grep" | "egrep"
        | "fgrep" | "which" | "type" | "uname" | "id" | "whoami" | "date" | "true" | "false"
        | "stat" | "file" | "readlink" | "realpath" | "tree" | "diff" => true,
        "rg" => !argv.iter().any(|a| {
            a == "--pre"
                || a.starts_with("--pre=")
                || a == "--pre-glob"
                || a.starts_with("--pre-glob=")
        }),
        "find" => !argv.iter().any(|a| {
            matches!(
                a.as_str(),
                "-exec"
                    | "-execdir"
                    | "-delete"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
            )
        }),
        "git" => git_is_readonly(argv),
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
            if argv.iter().any(|a| {
                a == "-i"
                    || a.starts_with("-i")
                    || a == "--in-place"
                    || a.starts_with("--in-place=")
            }) {
                writes.extend(refs.iter().cloned());
                false
            } else {
                sed_is_readonly(argv)
            }
        }
        _ => false,
    };

    (is_ro, refs, writes)
}

fn git_is_readonly(argv: &[String]) -> bool {
    let mut i = 1usize;
    while let Some(arg) = argv.get(i).map(String::as_str) {
        if arg == "-C" {
            i += 2;
            continue;
        }
        break;
    }
    let Some(sub) = argv.get(i).map(String::as_str) else {
        return false;
    };
    let rest = &argv[i + 1..];
    if rest.iter().any(|a| {
        a == "--output"
            || a.starts_with("--output=")
            || a == "--ext-diff"
            || a.starts_with("--config")
    }) {
        return false;
    }
    match sub {
        "status" | "log" | "diff" | "show" | "rev-parse" | "ls-files" => true,
        "branch" => rest.iter().all(|a| {
            matches!(
                a.as_str(),
                "-a" | "--all"
                    | "-r"
                    | "--remotes"
                    | "-v"
                    | "-vv"
                    | "--verbose"
                    | "-l"
                    | "--list"
                    | "--show-current"
                    | "--no-color"
                    | "--color"
                    | "--abbrev"
                    | "--no-abbrev"
                    | "--column"
                    | "--no-column"
                    | "--merged"
                    | "--no-merged"
                    | "--contains"
                    | "--no-contains"
                    | "--points-at"
            ) || a.starts_with("--sort=")
                || a.starts_with("--format=")
                || a.starts_with("--color=")
        }),
        _ => false,
    }
}

fn sed_is_readonly(argv: &[String]) -> bool {
    let mut has_n = false;
    let mut script: Option<&str> = None;
    let mut i = 1usize;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "-n" || a == "--quiet" || a == "--silent" {
            has_n = true;
            i += 1;
            continue;
        }
        if a == "-e" || a == "--expression" {
            let Some(expr) = argv.get(i + 1).map(String::as_str) else {
                return false;
            };
            if script.is_some() {
                return false;
            }
            script = Some(expr);
            i += 2;
            continue;
        }
        if a.starts_with('-') || a == "-f" || a == "--file" {
            return false;
        }
        if script.is_none() {
            script = Some(a);
        }
        i += 1;
    }
    has_n && script.is_some_and(is_simple_sed_print)
}

fn is_simple_sed_print(expr: &str) -> bool {
    let s = expr.trim();
    let Some(body) = s.strip_suffix('p') else {
        return false;
    };
    !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || c == ',' || c == '$')
}

fn looks_like_path(arg: &str) -> bool {
    if arg.is_empty()
        || arg.contains("://")
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

pub(super) fn tokenize_words(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            if has_token {
                words.push(std::mem::take(&mut cur));
                has_token = false;
            }
            i += 1;
            continue;
        }
        if c == '&' && i + 1 < chars.len() && chars[i + 1] == '>' {
            if has_token {
                words.push(std::mem::take(&mut cur));
                has_token = false;
            }
            if i + 2 < chars.len() && chars[i + 2] == '>' {
                words.push("&>>".into());
                i += 3;
            } else {
                words.push("&>".into());
                i += 2;
            }
            continue;
        }
        if c == '<' || c == '>' {
            if matches!(cur.as_str(), "0" | "1" | "2") && has_token {
                let fd = std::mem::take(&mut cur);
                has_token = false;
                let (op, advance) = scan_redir_op(&chars, i);
                words.push(format!("{fd}{op}"));
                i += advance;
                continue;
            }
            if has_token {
                words.push(std::mem::take(&mut cur));
                has_token = false;
            }
            let (op, advance) = scan_redir_op(&chars, i);
            words.push(op.to_string());
            i += advance;
            continue;
        }
        has_token = true;
        match c {
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    cur.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    bail!("unclosed single quote");
                }
                i += 1;
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        cur.push(chars[i + 1]);
                        i += 2;
                    } else {
                        cur.push(chars[i]);
                        i += 1;
                    }
                }
                if i >= chars.len() {
                    bail!("unclosed double quote");
                }
                i += 1;
            }
            '\\' => {
                if i + 1 >= chars.len() {
                    bail!("dangling escape");
                }
                cur.push(chars[i + 1]);
                i += 2;
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    if has_token {
        words.push(cur);
    }
    Ok(words)
}

fn scan_redir_op(chars: &[char], i: usize) -> (&'static str, usize) {
    let rem = chars.len() - i;
    if chars[i] == '<' {
        if rem >= 3 && chars[i + 1] == '<' && chars[i + 2] == '<' {
            return ("<<<", 3);
        }
        if rem >= 3 && chars[i + 1] == '<' && chars[i + 2] == '-' {
            return ("<<-", 3);
        }
        if rem >= 2 && chars[i + 1] == '<' {
            return ("<<", 2);
        }
        if rem >= 2 && chars[i + 1] == '>' {
            return ("<>", 2);
        }
        ("<", 1)
    } else if rem >= 2 && chars[i + 1] == '>' {
        (">>", 2)
    } else if rem >= 2 && chars[i + 1] == '|' {
        (">|", 2)
    } else {
        (">", 1)
    }
}

pub(super) fn redir_op_kind(tok: &str) -> Option<RedirKind> {
    match tok {
        "<<" | "<<-" | "<<<" => Some(RedirKind::Heredoc),
        "<" | "0<" => Some(RedirKind::Read),
        ">" | ">>" | ">|" | "1>" | "1>>" | "2>" | "2>>" | "&>" | "&>>" | "<>" | "0<>" => {
            Some(RedirKind::Write)
        }
        _ => None,
    }
}

pub(super) fn parse_redir_inline(tok: &str) -> Option<(RedirKind, String)> {
    for (prefix, kind) in [
        ("1>>", RedirKind::Write),
        ("2>>", RedirKind::Write),
        ("&>>", RedirKind::Write),
        ("0<>", RedirKind::Write),
        ("<<<", RedirKind::Heredoc),
        ("<<-", RedirKind::Heredoc),
        ("<<", RedirKind::Heredoc),
        ("<>", RedirKind::Write),
        (">>", RedirKind::Write),
        (">|", RedirKind::Write),
        ("1>", RedirKind::Write),
        ("2>", RedirKind::Write),
        ("&>", RedirKind::Write),
        ("0<", RedirKind::Read),
        (">", RedirKind::Write),
        ("<", RedirKind::Read),
    ] {
        if let Some(rest) = tok.strip_prefix(prefix) {
            if !rest.is_empty() {
                return Some((kind, rest.to_string()));
            }
        }
    }
    None
}

pub(super) fn is_env_assign(w: &str) -> bool {
    if let Some((k, _)) = w.split_once('=') {
        !k.is_empty()
            && !k.starts_with('-')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    } else {
        false
    }
}
