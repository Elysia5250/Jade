//! System prompt and project instructions.
use crate::session::{ChatMessage, ChatRole};
use std::path::{Path, PathBuf};

pub(crate) const ENV_PREFIX: &str = "<environment>\n";
pub(crate) const ENV_ACK: &str = "Continuing with this environment.";

pub(crate) fn build_system_prompt(target: &str, cwd: &Path) -> String {
    let mut prompt = format!(
        "You are Envoy, a terminal coding and systems agent.\n\
         Active target machine: {target}\n\
         Working directory: {}\n\
         {}\n\
         ## Rules\n\
         - Reply in the same language as the user's most recent message. If the user writes Chinese, answer in Chinese; never switch to English just because the code or tool output is English.\n\
         - Be precise and concise. Do the work; never substitute a summary of what you would do for actually doing it.\n\
         - If a request is ambiguous, ask one short question before acting.\n\
         - If a command is destructive (delete, overwrite, force-push, chmod -R, kill), call `ask` before doing it. Do not ask in prose. Give 2-5 options, mark exactly one `recommended`, and keep the safe option recommended. If nobody answers within the timeout, the recommended option is taken.\n\
         - Prefer `read`/`edit`/`ls`/`glob`/`grep` over shell commands when inspecting or editing files. Relative paths resolve against the working directory. Tool arguments come from the tool schema.\n\
         - Use `exec` with a small wait for long commands, then `job` to follow or cancel. Report exact commands, job ids and exit codes; never invent tool output.\n\
         - Answer in Markdown. Use tables for comparisons, but keep them narrow enough for a terminal.\n\
         - Call `say` with one present-tense sentence when you change phase, start work that will take a while, or the plan changes. Use the user's language, for example `现在在对列表缩进，接着改代码块对齐`. Put `say` in the same response as the next real tool; do not spend a turn on it alone. Do not call it for every file read, and do not restate the request.\n\
         - Use `ask` when a decision is genuinely ambiguous. Give 2-5 options and mark exactly one `recommended`.",
        cwd.display(),
        host_line(target)
    );
    if let Some(extra) = project_instructions(cwd) {
        prompt.push_str("\n\n## Project instructions\n");
        prompt.push_str(&extra);
    }
    prompt
}

pub(crate) fn build_subagent_system_prompt(
    target: &str,
    cwd: &Path,
    role_prompt: &str,
) -> String {
    let mut prompt = format!(
        "You are an Envoy subagent executing a focused delegated task.\n\
         Active target machine: {target}\n\
         Working directory: {}\n\
         {}\n\
         ## Rules\n\
         - Stay strictly within the assigned task and available tools.\n\
         - Never invent file paths, symbols, or tool outputs.\n\
         - Reply with concise, factual results in the language of the task.\n\n\
         ## Role Instructions\n\
         {}",
        cwd.display(),
        host_line(target),
        role_prompt.trim(),
    );
    if let Some(extra) = project_instructions(cwd) {
        prompt.push_str("\n\n## Project instructions\n");
        prompt.push_str(&extra);
    }
    prompt
}

fn host_line(target: &str) -> String {
    if target.is_empty() || target == "local" {
        format!(
            "Operating system: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    } else {
        format!("Remote target `{target}`. Do not assume the local operating system.")
    }
}

/// Skills, agents, and todos. This changes during a session, so it stays out of
/// the stable system prompt.
#[allow(dead_code)]
pub(crate) fn environment_note(data_dir: &Path, cwd: &Path, session_id: &str) -> Option<String> {
    environment_note_for_depth(data_dir, cwd, session_id, 0)
}

pub(crate) fn environment_note_for_depth(
    data_dir: &Path,
    cwd: &Path,
    session_id: &str,
    depth: u8,
) -> Option<String> {
    let mut parts = Vec::new();
    let skills = crate::skills::discover_skills(data_dir, cwd);
    if let Some(catalog) = crate::skills::format_skills_catalog(&skills) {
        parts.push(format!(
            "## Available skills (load full steps on demand with `skill`)\n{catalog}"
        ));
    }
    if depth == 0 {
        let agents = super::subagent::discover_agents(data_dir, cwd);
        if let Some(catalog) = super::subagent::format_agents_catalog(&agents) {
            parts.push(format!(
                "## Available subagents (invoke with `delegate`)\n{catalog}"
            ));
        }
        if let Some(todos) = super::tools::todo::format_active_todos_xml(data_dir, session_id) {
            parts.push(format!("## Active session checklist\n{todos}"));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

pub(crate) fn sync_environment(messages: &mut Vec<ChatMessage>, note: Option<&str>) {
    let Some(note) = note.filter(|text| !text.trim().is_empty()) else {
        let pinned = take_environment(messages);
        let _ = pinned;
        return;
    };
    let content = format!("{ENV_PREFIX}{note}\n</environment>");
    if messages
        .first()
        .is_some_and(|msg| msg.content.starts_with(ENV_PREFIX))
    {
        messages[0].content = content;
        return;
    }
    messages.insert(0, ack_message());
    messages.insert(0, env_message(content));
}

pub(crate) fn take_environment(messages: &mut Vec<ChatMessage>) -> Vec<ChatMessage> {
    if !messages
        .first()
        .is_some_and(|msg| msg.role == ChatRole::User && msg.content.starts_with(ENV_PREFIX))
    {
        return Vec::new();
    }
    let mut pinned = vec![messages.remove(0)];
    if messages.first().is_some_and(|msg| {
        msg.role == ChatRole::Assistant && msg.content == ENV_ACK
    }) {
        pinned.push(messages.remove(0));
    }
    pinned
}

pub(crate) fn restore_environment(messages: &mut Vec<ChatMessage>, pinned: Vec<ChatMessage>) {
    for (idx, msg) in pinned.into_iter().enumerate() {
        messages.insert(idx, msg);
    }
}

fn env_message(content: String) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content,
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_name: None,
    }
}

fn ack_message() -> ChatMessage {
    ChatMessage {
        role: ChatRole::Assistant,
        content: ENV_ACK.into(),
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_name: None,
    }
}

/// `AGENTS.md` or `ENVOY.md` from the git root down to `cwd`. Whole files only.
fn project_instructions(cwd: &Path) -> Option<String> {
    const CAP: usize = 16_000;
    let mut files: Vec<String> = Vec::new();
    let mut skipped = Vec::new();
    for path in instruction_files(cwd) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let label = path.display().to_string();
        let block = format!("### {label}\n{trimmed}\n");
        let used: usize = files.iter().map(|text| text.chars().count()).sum();
        if used + block.chars().count() > CAP
        {
            skipped.push(label);
            continue;
        }
        files.push(block);
    }
    if files.is_empty() && skipped.is_empty() {
        return None;
    }
    let mut out = files.concat();
    if !skipped.is_empty() {
        out.push_str("\nSkipped project instructions that did not fit: ");
        out.push_str(&skipped.join(", "));
        out.push('\n');
    }
    Some(out)
}

fn instruction_files(cwd: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut dir = cwd.to_path_buf();
    loop {
        for name in ["AGENTS.md", "ENVOY.md"] {
            let path = dir.join(name);
            if path.is_file() {
                chain.push(path);
                break;
            }
        }
        let at_git_root = dir.join(".git").exists();
        if at_git_root || !dir.pop() {
            break;
        }
    }
    chain.reverse();
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tools::openai_tools_json;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-prompt-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn prompt_pins_the_language_and_environment() {
        let dir = tmpdir("lang");
        let prompt = build_system_prompt("laptop", &dir);
        assert!(prompt.contains("same language as the user's most recent message"));
        assert!(prompt.contains("Chinese"));
        assert!(prompt.contains("laptop"));
        assert!(prompt.contains(&dir.display().to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_toolset_includes_say_and_ask() {
        let names: Vec<String> = openai_tools_json()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
            .collect();
        assert!(names.contains(&"say".to_string()), "{names:?}");
        assert!(names.contains(&"ask".to_string()), "{names:?}");
        assert!(names.contains(&"exec".to_string()), "{names:?}");
        let prompt = build_system_prompt("local", std::path::Path::new("/tmp"));
        assert!(prompt.contains("`say`"));
        assert!(prompt.contains("same response as the next real tool"));
        let say = openai_tools_json()
            .into_iter()
            .find(|tool| tool["function"]["name"] == "say")
            .expect("say");
        let desc = say["function"]["description"].as_str().unwrap();
        assert!(desc.contains("same response as the next real tool"), "{desc}");
        assert!(prompt.contains("`ask`"));
        assert!(prompt.contains("recommended"));
    }

    #[test]
    fn agents_md_is_included_and_capped() {
        let dir = tmpdir("agents");
        std::fs::write(dir.join("AGENTS.md"), "# House rules\nUse tabs.").unwrap();
        let prompt = build_system_prompt("local", &dir);
        assert!(prompt.contains("Use tabs."));
        assert!(prompt.contains("Project instructions"));

        std::fs::write(dir.join("AGENTS.md"), "x".repeat(20_000)).unwrap();
        let prompt = build_system_prompt("local", &dir);
        assert!(prompt.contains("Skipped project instructions"));
        assert!(!prompt.contains(&"x".repeat(100)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_target_does_not_claim_the_local_os() {
        let local = build_system_prompt("local", std::path::Path::new("/tmp"));
        assert!(local.contains("Operating system:"));
        let remote = build_system_prompt("laptop", std::path::Path::new("/tmp"));
        assert!(remote.contains("Remote target `laptop`"));
        assert!(!remote.contains("Operating system:"));
    }

    #[test]
    fn system_prompt_stays_stable_when_todos_change() {
        let dir = tmpdir("env");
        let data = tmpdir("env-data");
        let system_before = build_system_prompt("local", &dir);
        std::fs::create_dir_all(data.join("todos")).unwrap();
        std::fs::write(
            data.join("todos").join("sess.json"),
            r#"[{"id":"1","content":"ship the flag","status":"pending"}]"#,
        )
        .unwrap();
        let system_after = build_system_prompt("local", &dir);
        assert_eq!(system_before, system_after);
        let note = environment_note(&data, &dir, "sess").unwrap();
        assert!(note.contains("ship the flag"));
        assert!(!system_after.contains("ship the flag"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn parent_agents_md_is_included_before_the_cwd_file() {
        let root = tmpdir("walk");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root rule").unwrap();
        let cwd = root.join("crates");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "crate rule").unwrap();
        let prompt = build_system_prompt("local", &cwd);
        let root_at = prompt.find("root rule").unwrap();
        let crate_at = prompt.find("crate rule").unwrap();
        assert!(root_at < crate_at);
        let _ = std::fs::remove_dir_all(&root);
    }

}
