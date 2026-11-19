//! Discovery and YAML-frontmatter parsing for `SKILL.md` Agent Skills.
//!
//! Search roots (higher precedence later, so project overrides user):
//! - User scope (always loaded): `<data_dir>/skills`, `~/.envoy/skills`,
//!   `~/.agents/skills`, `~/.claude/skills`
//! - Project scope (only when the workspace is trusted):
//!   `<cwd>/.envoy/skills`, `<cwd>/.agents/skills`, `<cwd>/.claude/skills`
//!
//! The system prompt lists Envoy roots (`<data_dir>/skills`,
//! `~/.envoy/skills`, and a trusted `<cwd>/.envoy/skills`) unless
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
        roots.push((SkillScope::User, home.join(".envoy").join("skills")));
        roots.push((SkillScope::User, home.join(".agents").join("skills")));
        roots.push((SkillScope::User, home.join(".claude").join("skills")));
    }

    if is_workspace_trusted(data_dir, cwd) {
        roots.push((SkillScope::Project, cwd.join(".envoy").join("skills")));
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
    super::prefs::injected(
        data_dir,
        &skill.name,
        envoy_skill_root(data_dir, cwd, skill),
    )
}

fn envoy_skill_root(data_dir: &Path, cwd: &Path, skill: &SkillSummary) -> bool {
    let mut roots = vec![data_dir.join("skills")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".envoy").join("skills"));
    }
    roots.push(cwd.join(".envoy").join("skills"));
    roots.into_iter().any(|root| path_under(&root, &skill.dir))
}

/// `envoy`, `claude`, `agents`, or `user`, from the directory that won discovery.
pub fn skill_root_kind(data_dir: &Path, skill: &SkillSummary) -> &'static str {
    if path_under(&data_dir.join("skills"), &skill.dir) {
        return "envoy";
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if path_under(&home.join(".envoy").join("skills"), &skill.dir) {
            return "envoy";
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
                    "unknown skill `{target}` (no skills installed in ~/.envoy/skills, ~/.agents/skills, ~/.claude/skills, or project skills)"
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
    let (_, body) = split_frontmatter(&raw);
    let resources = collect_skill_resources(&summary.dir);

    Ok(LoadedSkill {
        summary,
        body: body.trim().to_string(),
        resources,
    })
}

fn parse_skill_summary(scope: SkillScope, dir: &Path, skill_file: &Path) -> Option<SkillSummary> {
    let raw = std::fs::read_to_string(skill_file).ok()?;
    let (fm_opt, body) = split_frontmatter(&raw);
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

fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let normalized = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(rest) = normalized
        .strip_prefix("---\n")
        .or_else(|| normalized.strip_prefix("---\r\n"))
    else {
        return (None, normalized);
    };

    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']).trim();
        if trimmed == "---" {
            let fm = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(fm), body);
        }
        offset += line.len();
    }
    (None, normalized)
}

fn parse_frontmatter(
    fm: &str,
    name: &mut String,
    description: &mut String,
    compatibility: &mut Option<String>,
    allowed_tools: &mut Vec<String>,
    metadata: &mut BTreeMap<String, String>,
) {
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
                        metadata.insert(k.trim().to_string(), unquote(v.trim()));
                    }
                }
                Section::AllowedTools => {
                    if let Some(item) = trimmed.strip_prefix('-').map(str::trim) {
                        if !item.is_empty() {
                            allowed_tools.push(unquote(item));
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

        let Some((key, val)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();

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
                } else if val.starts_with('[') && val.ends_with(']') {
                    for part in val[1..val.len() - 1].split(',') {
                        let item = unquote(part.trim());
                        if !item.is_empty() {
                            allowed_tools.push(item);
                        }
                    }
                } else {
                    for part in val.split([',', ' ']) {
                        let item = unquote(part.trim());
                        if !item.is_empty() {
                            allowed_tools.push(item);
                        }
                    }
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

fn unquote(raw: &str) -> String {
    let s = raw.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
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

fn walk_rel(current: &Path, base: &Path, out: &mut Vec<String>, cap: usize) {
    if out.len() >= cap {
        return;
    }
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|x| x.path())).collect();
    paths.sort();
    for p in paths {
        if out.len() >= cap {
            break;
        }
        if p.is_dir() {
            walk_rel(&p, base, out, cap);
        } else if let Ok(rel) = p.strip_prefix(base) {
            out.push(rel.display().to_string());
        }
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
