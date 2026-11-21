//! Compound shell parser, wrapper unwrapper, redirection & path inspector.
//!
//! Shell parsing is an *advisory classification* layer for permission prompts;
//! the OS sandbox (`bwrap` / Landlock) enforces the hard boundary. Whenever
//! syntax is ambiguous or unparseable, `parse_shell` returns `Err` so the
//! rule engine falls back to `Ask`.

use super::shell_classify::{
    base_bin, classify_argv, is_env_assign, parse_redir_inline, redir_op_kind,
    segment_is_catastrophic, segment_is_destructive, segment_needs_net, tokenize_words, RedirKind,
};
use anyhow::{bail, Result};

#[derive(Debug, Clone)]
pub struct CommandSegment {
    pub raw: String,
    pub argv: Vec<String>,
    pub redirections: Vec<String>,
    pub read_redirections: Vec<String>,
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
    let (raw_segments, has_write_proc_sub) = split_compound_and_subshells(command)?;
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
    let mut read_only_all = !has_write_proc_sub;
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
        for r in &seg.read_redirections {
            referenced_paths.push(r.clone());
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

fn split_compound_and_subshells(input: &str) -> Result<(Vec<String>, bool)> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut has_write_proc_sub = false;
    split_range(&chars, &mut out, &mut has_write_proc_sub)?;
    Ok((out, has_write_proc_sub))
}

fn split_range(chars: &[char], out: &mut Vec<String>, has_write_proc_sub: &mut bool) -> Result<()> {
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
                split_range(&inner, out, has_write_proc_sub)?;
                cur.push_str(&chars[i..=end].iter().collect::<String>());
                i = end + 1;
                continue;
            }
            if c == '`' {
                let (inner, end) = extract_backtick(chars, i)?;
                split_range(&inner, out, has_write_proc_sub)?;
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
                split_range(&inner, out, has_write_proc_sub)?;
                cur.push_str(&chars[i..=end].iter().collect::<String>());
                i = end + 1;
            }
            '<' | '>' if i + 1 < chars.len() && chars[i + 1] == '(' => {
                let (inner, end) = extract_paren_group(chars, i + 1)?;
                let inner_s: String = inner.iter().collect();
                if inner_s.trim().is_empty() {
                    bail!("empty process substitution");
                }
                if c == '>' {
                    *has_write_proc_sub = true;
                }
                split_range(&inner, out, has_write_proc_sub)?;
                if !cur.ends_with(' ') && !cur.is_empty() {
                    cur.push(' ');
                }
                i = end + 1;
            }
            '`' => {
                let (inner, end) = extract_backtick(chars, i)?;
                split_range(&inner, out, has_write_proc_sub)?;
                cur.push_str(&chars[i..=end].iter().collect::<String>());
                i = end + 1;
            }
            '(' => {
                paren_depth += 1;
                if paren_depth == 1 && cur.trim().is_empty() {
                    let (inner, end) = extract_paren_group(chars, i)?;
                    split_range(&inner, out, has_write_proc_sub)?;
                    paren_depth = 0;
                    i = end + 1;
                } else if paren_depth == 1 {
                    bail!("unexpected `(` mid-command");
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
    let mut read_redirections = Vec::new();
    let mut has_heredoc = false;
    let mut i = 0usize;

    while i < tokens.len() {
        let tok = &tokens[i];
        if let Some((kind, target)) = parse_redir_inline(tok) {
            match kind {
                RedirKind::Write => {
                    if target != "/dev/null" && !target.starts_with('&') {
                        redirections.push(target);
                    }
                }
                RedirKind::Read => {
                    if target != "/dev/null" && !target.starts_with('&') {
                        read_redirections.push(target);
                    }
                }
                RedirKind::Heredoc => {
                    has_heredoc = true;
                }
            }
            i += 1;
            continue;
        }
        if let Some(kind) = redir_op_kind(tok) {
            let Some(next) = tokens.get(i + 1) else {
                bail!("missing redirection target");
            };
            match kind {
                RedirKind::Write => {
                    if next != "/dev/null" && !next.starts_with('&') {
                        redirections.push(next.clone());
                    }
                }
                RedirKind::Read => {
                    if next != "/dev/null" && !next.starts_with('&') {
                        read_redirections.push(next.clone());
                    }
                }
                RedirKind::Heredoc => {
                    has_heredoc = true;
                }
            }
            i += 2;
            continue;
        }
        argv.push(tok.clone());
        i += 1;
    }

    let mut segs = unwrap_wrappers(raw, argv, redirections, read_redirections)?;
    if has_heredoc {
        for s in &mut segs {
            s.uses_xargs = true;
        }
    }
    Ok(segs)
}

fn unwrap_wrappers(
    raw: &str,
    mut argv: Vec<String>,
    redirections: Vec<String>,
    mut read_redirections: Vec<String>,
) -> Result<Vec<CommandSegment>> {
    let mut uses_sudo = false;
    let mut uses_xargs = false;
    let mut stripped_anything = false;

    loop {
        while argv.first().is_some_and(|w| is_env_assign(w)) {
            stripped_anything = true;
            let assign = argv.remove(0);
            if let Some((k, _)) = assign.split_once('=') {
                if jade_core::sandbox::is_dangerous_env_var(k) {
                    bail!("command sets security-sensitive env var `{k}`");
                }
            }
        }
        let Some(first) = argv.first().map(String::as_str) else {
            if stripped_anything {
                bail!("wrapper or env assignment left no executable command");
            }
            return Ok(Vec::new());
        };
        if matches!(
            first,
            "{" | "}"
                | "!"
                | "if"
                | "then"
                | "elif"
                | "else"
                | "fi"
                | "for"
                | "while"
                | "until"
                | "do"
                | "done"
                | "case"
                | "esac"
                | "select"
                | "function"
                | "coproc"
                | "eval"
                | "exec"
                | "source"
                | "."
        ) {
            bail!("compound syntax or dynamic builtin `{first}` requires approval");
        }
        let bin = base_bin(first);
        match bin {
            "sudo" | "doas" => {
                stripped_anything = true;
                uses_sudo = true;
                argv.remove(0);
                strip_wrapper_flags(&mut argv, &["-u", "-g", "-C", "-h"]);
            }
            "env" | "nohup" | "time" | "command" => {
                stripped_anything = true;
                let is_env = bin == "env";
                argv.remove(0);
                while argv.first().is_some_and(|a| a.starts_with('-')) {
                    if argv[0] == "--" {
                        argv.remove(0);
                        break;
                    }
                    if is_env
                        && (argv[0] == "-S"
                            || argv[0].starts_with("-S")
                            || argv[0] == "--split-string"
                            || argv[0].starts_with("--split-string="))
                    {
                        bail!("`env -S` split-string requires approval");
                    }
                    let take_arg = matches!(argv[0].as_str(), "-u" | "-C" | "--unset" | "--chdir");
                    argv.remove(0);
                    if take_arg && !argv.is_empty() {
                        argv.remove(0);
                    }
                }
            }
            "nice" | "ionice" | "stdbuf" => {
                stripped_anything = true;
                argv.remove(0);
                strip_wrapper_flags(&mut argv, &["-n", "-c", "-i", "-o", "-e"]);
            }
            "timeout" => {
                stripped_anything = true;
                argv.remove(0);
                strip_wrapper_flags(&mut argv, &["-k", "-s", "--kill-after", "--signal"]);
                if argv.is_empty() {
                    bail!("timeout missing duration or command");
                }
                argv.remove(0);
            }
            "xargs" => {
                stripped_anything = true;
                uses_xargs = true;
                argv.remove(0);
                strip_wrapper_flags(&mut argv, &["-I", "-n", "-P", "-d", "-E", "-L"]);
            }
            "sh" | "bash" | "zsh" | "dash" => {
                if let Some(c_pos) = argv.iter().position(|a| is_shell_c_flag(a)) {
                    let Some(inner) = argv.get(c_pos + 1).cloned() else {
                        bail!("shell `-c` missing inline command");
                    };
                    let extra_positional: Vec<String> =
                        argv.get(c_pos + 2..).unwrap_or(&[]).to_vec();
                    let (inner_segs, has_write_proc) = split_compound_and_subshells(&inner)?;
                    let mut out = Vec::new();
                    for seg in inner_segs {
                        let mut parsed = parse_segment(&seg)?;
                        for s in &mut parsed {
                            s.uses_sudo |= uses_sudo;
                            s.uses_xargs |= uses_xargs || has_write_proc;
                            s.redirections.extend(redirections.iter().cloned());
                            s.read_redirections
                                .extend(read_redirections.iter().cloned());
                            s.read_redirections.extend(extra_positional.iter().cloned());
                        }
                        out.extend(parsed);
                    }
                    if out.is_empty() {
                        bail!("empty inline shell command");
                    }
                    return Ok(out);
                }
                for tok in argv.iter().skip(1) {
                    if !tok.starts_with('-') {
                        read_redirections.push(tok.clone());
                    }
                }
                break;
            }
            _ => break,
        }
    }

    if argv.is_empty() {
        if stripped_anything {
            bail!("wrapper left no executable command");
        }
        return Ok(Vec::new());
    }
    Ok(vec![CommandSegment {
        raw: raw.to_string(),
        argv,
        redirections,
        read_redirections,
        uses_sudo,
        uses_xargs,
    }])
}

fn strip_wrapper_flags(argv: &mut Vec<String>, take_arg_flags: &[&str]) {
    while argv.first().is_some_and(|a| a.starts_with('-')) {
        if argv[0] == "--" {
            argv.remove(0);
            break;
        }
        let take_arg = take_arg_flags.contains(&argv[0].as_str());
        argv.remove(0);
        if take_arg && !argv.is_empty() {
            argv.remove(0);
        }
    }
}

fn is_shell_c_flag(flag: &str) -> bool {
    flag.starts_with('-')
        && !flag.starts_with("--")
        && flag.len() >= 2
        && flag[1..].bytes().all(|b| b.is_ascii_alphabetic())
        && flag[1..].contains('c')
}
