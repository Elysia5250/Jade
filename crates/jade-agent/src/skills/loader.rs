//! Discovery and YAML-frontmatter parsing for `SKILL.md` Agent Skills.
//!
//! Search roots (higher precedence later, so project overrides user):
//! - User scope (always loaded): `<data_dir>/skills`, `~/.jade/skills`,
//!   `~/.agents/skills`, `~/.claude/skills`
//! - Project scope (only when the workspace is trusted):
//!   `<cwd>/.jade/skills`, `<cwd>/.agents/skills`, `<cwd>/.claude/skills`
//!
//! The system prompt lists Jade roots (`<data_dir>/skills`,
//! `~/.jade/skills`, and a trusted `<cwd>/.jade/skills`) unless
//! `<data_dir>/skills.json` turns a skill on or off by name. The other roots
//! stay discoverable and loadable by name.

use crate::permissions::is_workspace_trusted;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub compatibility: Option<String>,
    pub allowed_tools: Vec<String>,
    pub metadata: BTreeMap<String, String>,
    pub scope: SkillScope,
    pub dir: PathBuf,
    pub skill_file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    User,
    Project,
}

impl SkillScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub summary: SkillSummary,
    pub body: String,
    pub resources: Vec<String>,
}

pub fn discover_skills(data_dir: &Path, cwd: &Path) -> Vec<SkillSummary> {
    let mut roots: Vec<(SkillScope, PathBuf)> = vec![(SkillScope::User, data_dir.join("skills"))];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push((SkillScope::User, home.join(".jade").join("skills")));
        roots.push((SkillScope::User, home.join(".agents").join("skills")));
        roots.push((SkillScope::User, home.join(".claude").join("skills")));
    }

    if is_workspace_trusted(data_dir, cwd) {
        roots.push((SkillScope::Project, cwd.join(".jade").join("skills")));
        roots.push((SkillScope::Project, cwd.join(".agents").join("skills")));
        roots.push((SkillScope::Project, cwd.join(".claude").join("skills")));
    }

    let mut by_name: BTreeMap<String, SkillSummary> = BTreeMap::new();
    let mut seen_dirs: BTreeSet<PathBuf> = BTreeSet::new();

    for (scope, root) in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|ent| ent.path()))
            .filter(|p| p.is_dir())
            .collect();
        children.sort();

        for dir in children {
            let canon = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            if !seen_dirs.insert(canon) {
                continue;
            }
            let skill_file = dir.join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            if let Some(summary) = parse_skill_summary(scope, &dir, &skill_file) {
                // Project skills override same-named user skills because project roots come later.
                by_name.insert(summary.name.clone(), summary);
            }
        }
    }

    by_name.into_values().collect()
}

/// Skills that belong in the system prompt. Claude and Agents roots stay
/// available through [`load_skill`] and `/skill` until `skills.json` enables them.
pub fn prompt_skills(data_dir: &Path, cwd: &Path) -> Vec<SkillSummary> {
    discover_skills(data_dir, cwd)
        .into_iter()
        .filter(|skill| skill_is_injected(data_dir, cwd, skill))
        .collect()
}

pub fn skill_is_injected(data_dir: &Path, cwd: &Path, skill: &SkillSummary) -> bool {
    super::prefs::injected(data_dir, &skill.name, jade_skill_root(data_dir, cwd, skill))
}

fn jade_skill_root(data_dir: &Path, cwd: &Path, skill: &SkillSummary) -> bool {
    let mut roots = vec![data_dir.join("skills")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".jade").join("skills"));
    }
    roots.push(cwd.join(".jade").join("skills"));
    roots.into_iter().any(|root| path_under(&root, &skill.dir))
}

/// `project`, `jade`, `claude`, `agents`, or `user`, from scope and directory.
pub fn skill_root_kind(data_dir: &Path, skill: &SkillSummary) -> &'static str {
    if skill.scope == SkillScope::Project {
        return "project";
    }
    if path_under(&data_dir.join("skills"), &skill.dir) {
        return "jade";
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if path_under(&home.join(".jade").join("skills"), &skill.dir) {
            return "jade";
        }
        if path_under(&home.join(".claude").join("skills"), &skill.dir) {
            return "claude";
        }
        if path_under(&home.join(".agents").join("skills"), &skill.dir) {
            return "agents";
        }
    }
    "user"
}

fn path_under(root: &Path, dir: &Path) -> bool {
    let Some(root) = std::fs::canonicalize(root).ok() else {
        return false;
    };
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    dir.starts_with(root)
}

pub fn format_skills_catalog(skills: &[SkillSummary]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from("<available_skills>\n");
    for s in skills {
        out.push_str(&format!(
            "  <skill name=\"{}\" scope=\"{}\" path=\"{}\">{}</skill>\n",
            escape_xml(&s.name),
            s.scope.label(),
            escape_xml(&s.skill_file.display().to_string()),
            escape_xml(&s.description),
        ));
    }
    out.push_str("</available_skills>");
    Some(out)
}

pub fn load_skill(data_dir: &Path, cwd: &Path, name: &str) -> Result<LoadedSkill> {
    let target = name.trim();
    if target.is_empty() {
        bail!("missing skill name");
    }
    let skills = discover_skills(data_dir, cwd);
    let available: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    let summary = skills
        .into_iter()
        .find(|s| s.name == target || s.name.eq_ignore_ascii_case(target))
        .with_context(|| {
            if available.is_empty() {
                format!(
                    "unknown skill `{target}` (no skills installed in ~/.jade/skills, ~/.agents/skills, ~/.claude/skills, or project skills)"
                )
            } else {
                format!(
                    "unknown skill `{target}` (available: {})",
                    available.join(", ")
                )
            }
        })?;

    let raw = std::fs::read_to_string(&summary.skill_file)
        .with_context(|| format!("reading {}", summary.skill_file.display()))?;
    let (_, body) = crate::frontmatter::split_frontmatter(&raw);
    let resources = collect_skill_resources(&summary.dir);

    Ok(LoadedSkill {
        summary,
        body: body.trim().to_string(),
        resources,
    })
}

fn parse_skill_summary(scope: SkillScope, dir: &Path, skill_file: &Path) -> Option<SkillSummary> {
    let raw = std::fs::read_to_string(skill_file).ok()?;
    let (fm_opt, body) = crate::frontmatter::split_frontmatter(&raw);
    let fallback_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("skill")
        .to_string();

    let mut name = fallback_name;
    let mut description = String::new();
    let mut compatibility = None;
    let mut allowed_tools = Vec::new();
    let mut metadata = BTreeMap::new();

    if let Some(fm) = fm_opt {
        parse_frontmatter(
            fm,
            &mut name,
            &mut description,
            &mut compatibility,
            &mut allowed_tools,
            &mut metadata,
        );
    }

    if description.is_empty() {
        description = body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("Agent skill instructions")
            .chars()
            .take(160)
            .collect();
    }

    Some(SkillSummary {
        name,
        description,
        compatibility,
        allowed_tools,
        metadata,
        scope,
        dir: dir.to_path_buf(),
        skill_file: skill_file.to_path_buf(),
    })
}

fn parse_frontmatter(
    fm: &str,
    name: &mut String,
    description: &mut String,
    compatibility: &mut Option<String>,
    allowed_tools: &mut Vec<String>,
    metadata: &mut BTreeMap<String, String>,
) {
    use crate::frontmatter::{parse_inline_list, strip_inline_comment, unquote};

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        None,
        Metadata,
        AllowedTools,
        DescriptionBlock,
    }

    let mut section = Section::None;
    let mut desc_lines: Vec<String> = Vec::new();

    for raw_line in fm.lines() {
        let line = raw_line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            if section == Section::DescriptionBlock {
                desc_lines.push(String::new());
            }
            continue;
        }

        let indented = line.starts_with(' ') || line.starts_with('\t');
        if indented {
            match section {
                Section::Metadata => {
                    if let Some((k, v)) = trimmed.split_once(':') {
                        metadata.insert(k.trim().to_string(), unquote(v));
                    }
                }
                Section::AllowedTools => {
                    if let Some(item) = trimmed.strip_prefix('-').map(str::trim) {
                        let cleaned = unquote(item);
                        if !cleaned.is_empty() {
                            allowed_tools.push(cleaned);
                        }
                    }
                }
                Section::DescriptionBlock => {
                    desc_lines.push(trimmed.to_string());
                }
                Section::None => {}
            }
            continue;
        }

        if section == Section::DescriptionBlock && !desc_lines.is_empty() {
            *description = desc_lines.join(" ").trim().to_string();
            desc_lines.clear();
        }
        section = Section::None;

        let Some((key, raw_val)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = strip_inline_comment(raw_val).trim();

        match key {
            "name" => {
                let v = unquote(val);
                if !v.is_empty() {
                    *name = v;
                }
            }
            "description" => {
                if val == "|" || val == ">" || val == "|-" || val == ">-" || val.is_empty() {
                    section = Section::DescriptionBlock;
                } else {
                    *description = unquote(val);
                }
            }
            "compatibility" => {
                let v = unquote(val);
                if !v.is_empty() {
                    *compatibility = Some(v);
                }
            }
            "allowed-tools" | "allowed_tools" => {
                if val.is_empty() {
                    section = Section::AllowedTools;
                } else {
                    allowed_tools.extend(parse_inline_list(val, true));
                }
            }
            "metadata" if val.is_empty() => {
                section = Section::Metadata;
            }
            _ => {}
        }
    }

    if section == Section::DescriptionBlock && !desc_lines.is_empty() {
        *description = desc_lines.join(" ").trim().to_string();
    }
}

fn collect_skill_resources(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for sub in ["scripts", "references", "assets"] {
        let root = dir.join(sub);
        if !root.is_dir() {
            continue;
        }
        walk_rel(&root, dir, &mut files, 64);
    }
    files.sort();
    files
}

const WALK_MAX_DEPTH: usize = 16;

fn walk_rel(current: &Path, base: &Path, out: &mut Vec<String>, cap: usize) {
    let mut seen = BTreeSet::new();
    walk_rel_at(current, base, out, cap, 0, &mut seen);
}

fn walk_rel_at(
    current: &Path,
    base: &Path,
    out: &mut Vec<String>,
    cap: usize,
    depth: usize,
    seen: &mut BTreeSet<(u64, u64)>,
) {
    if out.len() >= cap || depth > WALK_MAX_DEPTH {
        return;
    }
    if let Some(id) = followed_dir_id(current) {
        if !seen.insert(id) {
            return;
        }
    }
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.path());
    let mut dirs = Vec::new();
    for entry in entries {
        if out.len() >= cap {
            break;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            consider_skill_link(base, &path, out, seen, &mut dirs);
        } else if kind.is_dir() {
            dirs.push(path);
        } else if kind.is_file() {
            push_skill_rel(base, &path, out);
        }
    }
    for dir in dirs {
        if out.len() >= cap {
            break;
        }
        walk_rel_at(&dir, base, out, cap, depth + 1, seen);
    }
}

fn consider_skill_link(
    base: &Path,
    path: &Path,
    out: &mut Vec<String>,
    seen: &BTreeSet<(u64, u64)>,
    dirs: &mut Vec<PathBuf>,
) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.is_dir() {
        if followed_id(&meta).is_some_and(|id| seen.contains(&id)) {
            return;
        }
        dirs.push(path.to_path_buf());
        return;
    }
    if meta.is_file() {
        push_skill_rel(base, path, out);
    }
}

fn push_skill_rel(base: &Path, path: &Path, out: &mut Vec<String>) {
    if let Ok(rel) = path.strip_prefix(base) {
        out.push(rel.display().to_string());
    }
}

fn followed_dir_id(dir: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(dir).ok()?;
    if meta.is_dir() {
        followed_id(&meta)
    } else {
        None
    }
}

fn followed_id(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
