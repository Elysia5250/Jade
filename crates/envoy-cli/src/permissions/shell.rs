//! Compound shell parser, wrapper unwrapper, redirection & path inspector.
//!
//! Shell parsing is an *advisory classification* layer for permission prompts;
//! the OS sandbox (`bwrap` / Landlock) enforces the hard boundary. Whenever
//! syntax is ambiguous or unparseable, `parse_shell` returns `Err` so the
//! rule engine falls back to `Ask`.

use anyhow::{bail, Result};

#[derive(Debug, Clone)]
pub struct CommandSegment {
    pub raw: String,
    pub argv: Vec<String>,
    pub redirections: Vec<String>,
    pub uses_sudo: bool,
    pub uses_xargs: bool,
}

#[derive(Debug, Clone)]
pub struct ShellAnalysis {
    pub segments: Vec<CommandSegment>,
    pub needs_net: bool,
    pub destructive: bool,
    pub catastrophic: bool,
    pub read_only_all: bool,
    pub referenced_paths: Vec<String>,
    pub write_paths: Vec<String>,
}

pub fn analyze_shell(command: &str) -> Result<ShellAnalysis> {
    let raw_segments = split_compound_and_subshells(command)?;
    if raw_segments.is_empty() {
        bail!("empty command");
    }
    let mut segments = Vec::new();
    for raw in raw_segments {
        let mut parsed = parse_segment(&raw)?;
        segments.append(&mut parsed);
    }
    if segments.is_empty() {
        bail!("no executable command segments");
    }

    let mut needs_net = false;
    let mut destructive = false;
    let mut catastrophic = false;
    let mut read_only_all = true;
    let mut referenced_paths = Vec::new();
    let mut write_paths = Vec::new();

    for seg in &segments {
        if seg.uses_sudo || seg.uses_xargs {
            read_only_all = false;
        }
        if segment_needs_net(&seg.argv) {
            needs_net = true;
        }
        if segment_is_destructive(&seg.argv) {
            destructive = true;
            read_only_all = false;
        }
        if segment_is_catastrophic(&seg.argv) {
            catastrophic = true;
            destructive = true;
            read_only_all = false;
        }
        if !seg.redirections.is_empty() {
            read_only_all = false;
            for r in &seg.redirections {
                write_paths.push(r.clone());
                referenced_paths.push(r.clone());
            }
        }
        let (is_ro, refs, writes) = classify_argv(&seg.argv);
        if !is_ro {
            read_only_all = false;
        }
        referenced_paths.extend(refs);
        write_paths.extend(writes);
    }

    Ok(ShellAnalysis {
        segments,
        needs_net,
        destructive,
        catastrophic,
        read_only_all,
        referenced_paths,
        write_paths,
    })
}

fn split_compound_and_subshells(input: &str) -> Result<Vec<String>> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    split_range(&chars, &mut out)?;
    Ok(out)
}

fn split_range(chars: &[char], out: &mut Vec<String>) -> Result<()> {
    let mut cur = String::new();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut paren_depth = 0usize;

    while i < chars.len() {
        let c = chars[i];
        if in_single {
            cur.push(c);
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if c == '\\' {
            if i + 1 >= chars.len() {
                bail!("dangling backslash escape");
            }
            cur.push(c);
            cur.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if in_double {
            if c == '$' && i + 1 < chars.len() && chars[i + 1] == '(' {
                let (inner, end) = extract_paren_group(chars, i + 1)?;
                split_range(&inner, out)?;
                cur.push_str(&chars[i..=end].iter().collect::<String>());
                i = end + 1;
                continue;
            }
            if c == '`' {
                let (inner, end) = extract_backtick(chars, i)?;
                split_range(&inner, out)?;
                cur.push_str(&chars[i..=end].iter().collect::<String>());
                i = end + 1;
                continue;
            }
            cur.push(c);
            if c == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        match c {
            '\'' => {
                in_single = true;
                cur.push(c);
                i += 1;
            }
            '"' => {
                in_double = true;
                cur.push(c);
                i += 1;
            }
            '$' if i + 1 < chars.len() && chars[i + 1] == '(' => {
                let (inner, end) = extract_paren_group(chars, i + 1)?;
                split_range(&inner, out)?;
                cur.push_str(&chars[i..=end].iter().collect::<String>());
                i = end + 1;
            }
            '`' => {
                let (inner, end) = extract_backtick(chars, i)?;
                split_range(&inner, out)?;
                cur.push_str(&chars[i..=end].iter().collect::<String>());
                i = end + 1;
            }
            '(' => {
                paren_depth += 1;
                if paren_depth == 1 && cur.trim().is_empty() {
                    let (inner, end) = extract_paren_group(chars, i)?;
                    split_range(&inner, out)?;
                    paren_depth = 0;
                    i = end + 1;
                } else {
                    cur.push(c);
                    i += 1;
                }
            }
            ')' => {
                if paren_depth == 0 {
                    bail!("unexpected `)`");
                }
                paren_depth -= 1;
                cur.push(c);
                i += 1;
            }
            ';' | '\n' => {
                push_segment(&mut cur, out);
                i += 1;
            }
            '|' | '&' => {
                // Handle `||`, `&&`, `|`, `&`, `2>&1`, `&>` carefully.
                if (c == '&' || c == '|') && i > 0 && chars[i - 1] == '>' {
                    cur.push(c);
                    i += 1;
                    continue;
                }
                if c == '&' && i + 1 < chars.len() && chars[i + 1] == '>' {
                    cur.push(c);
                    i += 1;
                    continue;
                }
                push_segment(&mut cur, out);
                if i + 1 < chars.len() && chars[i + 1] == c {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }

    if in_single || in_double || paren_depth != 0 {
        bail!("unclosed quote or subshell");
    }
    push_segment(&mut cur, out);
    Ok(())
}

fn extract_paren_group(chars: &[char], open_idx: usize) -> Result<(Vec<char>, usize)> {
    let mut depth = 0usize;
    let mut i = open_idx;
    let mut in_single = false;
    let mut in_double = false;
    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if c == '\\' {
            i += 2;
            continue;
        }
        if in_double {
            if c == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' => in_single = true,
            '"' => in_double = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((chars[open_idx + 1..i].to_vec(), i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    bail!("unclosed `(`")
}

fn extract_backtick(chars: &[char], open_idx: usize) -> Result<(Vec<char>, usize)> {
    let mut i = open_idx + 1;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '`' {
            return Ok((chars[open_idx + 1..i].to_vec(), i));
        }
        i += 1;
    }
    bail!("unclosed backtick")
}

fn push_segment(cur: &mut String, out: &mut Vec<String>) {
    let trimmed = cur.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    cur.clear();
}

fn parse_segment(raw: &str) -> Result<Vec<CommandSegment>> {
    let tokens = tokenize_words(raw)?;
    let mut argv = Vec::new();
    let mut redirections = Vec::new();
    let mut i = 0usize;

    while i < tokens.len() {
        let tok = &tokens[i];
        if let Some(target) = parse_redir_inline(tok) {
            if target != "/dev/null" && !target.starts_with('&') {
                redirections.push(target);
            }
            i += 1;
            continue;
        }
        if is_redir_op(tok) {
            let Some(next) = tokens.get(i + 1) else {
                bail!("missing redirection target");
            };
            if next != "/dev/null" && !next.starts_with('&') {
                redirections.push(next.clone());
            }
            i += 2;
            continue;
        }
        argv.push(tok.clone());
        i += 1;
    }

    unwrap_wrappers(raw, argv, redirections)
}

fn unwrap_wrappers(
    raw: &str,
    mut argv: Vec<String>,
    redirections: Vec<String>,
) -> Result<Vec<CommandSegment>> {
    let mut uses_sudo = false;
    let mut uses_xargs = false;

    loop {
        while argv.first().is_some_and(|w| is_env_assign(w)) {
            argv.remove(0);
        }
        let Some(first) = argv.first().map(String::as_str) else {
            return Ok(Vec::new());
        };
        let bin = base_bin(first);
        match bin {
            "sudo" | "doas" => {
                uses_sudo = true;
                argv.remove(0);
                while argv.first().is_some_and(|a| a.starts_with('-')) {
                    let take_arg = matches!(argv[0].as_str(), "-u" | "-g" | "-C" | "-h");
                    argv.remove(0);
                    if take_arg && !argv.is_empty() {
                        argv.remove(0);
                    }
                }
            }
            "env" | "nohup" | "time" | "command" => {
                argv.remove(0);
                while argv.first().is_some_and(|a| a.starts_with('-')) {
                    let take_arg = matches!(argv[0].as_str(), "-u" | "-C" | "-S");
                    argv.remove(0);
                    if take_arg && !argv.is_empty() {
                        argv.remove(0);
                    }
                }
            }
            "nice" | "ionice" | "stdbuf" => {
                argv.remove(0);
                while argv.first().is_some_and(|a| a.starts_with('-')) {
                    let take_arg = matches!(argv[0].as_str(), "-n" | "-c" | "-i" | "-o" | "-e");
                    argv.remove(0);
                    if take_arg && !argv.is_empty() {
                        argv.remove(0);
                    }
                }
            }
            "timeout" => {
                argv.remove(0);
                while argv.first().is_some_and(|a| a.starts_with('-')) {
                    argv.remove(0);
                }
                if !argv.is_empty() {
                    argv.remove(0);
                }
            }
            "xargs" => {
                uses_xargs = true;
                argv.remove(0);
                while argv.first().is_some_and(|a| a.starts_with('-')) {
                    let take_arg = matches!(
                        argv[0].as_str(),
                        "-I" | "-n" | "-P" | "-d" | "-E" | "-L"
                    );
                    argv.remove(0);
                    if take_arg && !argv.is_empty() {
                        argv.remove(0);
                    }
                }
            }
            "sh" | "bash" | "zsh" | "dash" => {
                if let Some(c_pos) = argv.iter().position(|a| a == "-c" || a == "-lc") {
                    if let Some(inner) = argv.get(c_pos + 1).cloned() {
                        let inner_segs = split_compound_and_subshells(&inner)?;
                        let mut out = Vec::new();
                        for seg in inner_segs {
                            let mut parsed = parse_segment(&seg)?;
                            for s in &mut parsed {
                                s.uses_sudo |= uses_sudo;
                                s.uses_xargs |= uses_xargs;
                                s.redirections.extend(redirections.iter().cloned());
                            }
                            out.extend(parsed);
                        }
                        return Ok(out);
                    }
                }
                break;
            }
            _ => break,
        }
    }

    if argv.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![CommandSegment {
        raw: raw.to_string(),
        argv,
        redirections,
        uses_sudo,
        uses_xargs,
    }])
}

fn tokenize_words(input: &str) -> Result<Vec<String>> {
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

fn is_redir_op(tok: &str) -> bool {
    matches!(tok, ">" | ">>" | "1>" | "1>>" | "2>" | "2>>" | "&>" | "&>>")
}

fn parse_redir_inline(tok: &str) -> Option<String> {
    for prefix in ["1>>", "2>>", "&>>", ">>", "1>", "2>", "&>", ">"] {
        if let Some(rest) = tok.strip_prefix(prefix) {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

fn is_env_assign(w: &str) -> bool {
    if let Some((k, _)) = w.split_once('=') {
        !k.is_empty()
            && !k.starts_with('-')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    } else {
        false
    }
}

fn base_bin(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn segment_needs_net(argv: &[String]) -> bool {
    let Some(bin) = argv.first().map(|s| base_bin(s)) else {
        return false;
    };
    match bin {
        "curl" | "wget" | "ssh" | "scp" | "sftp" | "nc" | "ncat" | "telnet" | "ping"
        | "dig" | "nslookup" | "host" | "apt" | "apt-get" | "dnf" | "yum" | "pacman"
        | "brew" | "rustup" => true,
        "git" => argv
            .get(1)
            .is_some_and(|s| matches!(s.as_str(), "clone" | "fetch" | "pull" | "push" | "ls-remote")),
        "cargo" => argv
            .get(1)
            .is_some_and(|s| matches!(s.as_str(), "fetch" | "update" | "install" | "publish" | "search")),
        "npm" | "pnpm" | "yarn" | "pip" | "pip3" | "uv" => argv
            .get(1)
            .is_some_and(|s| matches!(s.as_str(), "install" | "i" | "add" | "update" | "upgrade" | "publish")),
        "gh" | "glab" => true,
        _ => false,
    }
}

fn segment_is_destructive(argv: &[String]) -> bool {
    let Some(bin) = argv.first().map(|s| base_bin(s)) else {
        return false;
    };
    let joined = argv.join(" ");
    match bin {
        "mkfs" | "fdisk" | "parted" | "shutdown" | "reboot" | "poweroff" => true,
        "dd" => argv.iter().any(|a| a.starts_with("of=/dev/")),
        "rm" => argv.iter().any(|a| a.contains('r') && a.starts_with('-'))
            || argv.iter().any(|a| a == "/" || a == "~" || a == "*"),
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

fn segment_is_catastrophic(argv: &[String]) -> bool {
    let Some(bin) = argv.first().map(|s| base_bin(s)) else {
        return false;
    };
    match bin {
        "mkfs" | "fdisk" | "parted" | "shutdown" | "reboot" | "poweroff" => true,
        "dd" => argv.iter().any(|a| a.starts_with("of=/dev/")),
        "rm" => argv.iter().any(|a| {
            let trimmed = a.trim_end_matches('/');
            const ROOT_DIRS: &[&str] = &[
                "", "~", "/*", "$HOME", "/bin", "/boot", "/dev", "/etc", "/home", "/lib",
                "/lib64", "/opt", "/proc", "/root", "/sbin", "/sys", "/usr", "/var",
            ];
            a == "/" || ROOT_DIRS.contains(&trimmed)
        }),
        _ => false,
    }
}

fn classify_argv(argv: &[String]) -> (bool, Vec<String>, Vec<String>) {
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
        "ls" | "pwd" | "echo" | "printf" | "cat" | "head" | "tail" | "wc" | "grep"
        | "egrep" | "fgrep" | "rg" | "which" | "type" | "uname" | "id" | "whoami"
        | "date" | "true" | "false" | "stat" | "file" | "readlink" | "realpath" | "tree"
        | "diff" => true,
        "find" => !argv.iter().any(|a| matches!(a.as_str(), "-exec" | "-execdir" | "-delete" | "-ok")),
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
