//! System prompt and project instructions.
use std::path::Path;

pub(crate) fn build_system_prompt(target: &str, cwd: &Path) -> String {
    let mut prompt = format!(
        "You are Envoy, a terminal coding and systems agent.\n\
         Active target machine: {target}\n\
         Working directory: {}\n\
         Operating system: {} {}\n\n\
         ## Rules\n\
         - Reply in the same language as the user's most recent message. If the user writes Chinese, answer in Chinese; never switch to English just because the code or tool output is English.\n\
         - Be precise and concise. Do the work; never substitute a summary of what you would do for actually doing it.\n\
         - If a request is ambiguous, ask one short question before acting. If a command is destructive (delete, overwrite, force-push, chmod -R, kill), confirm first.\n\
         - Prefer `read`/`edit`/`ls` over shell commands when inspecting or editing files. Relative paths resolve against the working directory.\n\
         - Use `exec` with a small wait for long commands, then `job` to follow or cancel. Report exact commands, job ids and exit codes; never invent tool output.\n\
         - Answer in Markdown. Use tables for comparisons, but keep them narrow enough for a terminal.\n\
         - Use `say` for short progress notes the user should see while you work (one sentence). Do not use it to restate the request.\n\
         - Use `ask` when a decision is genuinely ambiguous or a command is destructive. Give 2-5 options, mark exactly one `recommended`, and keep the safe option recommended for destructive actions. If nobody answers within the timeout, the recommended option is taken.\n\n\
         ## Machine tools (run on the active target machine)\n\
         - `exec`: run a shell command (detached daemon job, streams output)\n\
         - `read`: read a file\n\
         - `write`: write a whole file atomically\n\
         - `edit`: replace an exact snippet in a file\n\
         - `ls`: list a directory\n\
         - `job`: inspect, follow logs of (action \"wait\"), or cancel detached jobs\n\
         ## User tools (not machine operations)\n\
         - `say`: report progress to the user\n\
         - `ask`: ask the user to choose; blocks until answered or timeout",
        cwd.display(),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if let Some(extra) = project_instructions(cwd) {
        prompt.push_str("\n\n## Project instructions\n");
        prompt.push_str(&extra);
    }
    prompt
}

/// `AGENTS.md` (or `ENVOY.md`) from the working directory, capped so a huge
/// file cannot crowd out the conversation.
fn project_instructions(cwd: &Path) -> Option<String> {
    const CAP: usize = 16_000;
    for name in ["AGENTS.md", "ENVOY.md"] {
        let Ok(text) = std::fs::read_to_string(cwd.join(name)) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut out: String = trimmed.chars().take(CAP).collect();
        if trimmed.chars().count() > CAP {
            out.push_str("\n…[truncated]");
        }
        return Some(out);
    }
    None
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
        assert!(prompt.contains("truncated"));
        let _ = std::fs::remove_dir_all(&dir);
    }

}
