//! Agent Skills (`SKILL.md`) catalog discovery and on-demand loader.

pub mod loader;

pub mod prefs;

pub use loader::{
    discover_skills, format_skills_catalog, load_skill, prompt_skills, skill_is_injected,
    skill_root_kind, LoadedSkill, SkillScope, SkillSummary,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::set_workspace_trusted;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 6];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "envoy-skills-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn parses_frontmatter_and_blocks_project_skills_until_trusted() {
        let root = tmpdir("trust");
        let data = root.join("data");
        let cwd = root.join("repo");

        let user_skill = data.join("skills").join("rust-review");
        std::fs::create_dir_all(&user_skill).unwrap();
        std::fs::write(
            user_skill.join("SKILL.md"),
            "---\nname: rust-review\ndescription: \"Review Rust code carefully\"\ncompatibility: \"Rust 1.80+\"\nallowed-tools:\n  - read\n  - grep\nmetadata:\n  author: envoy\n---\n\n# Steps\n1. Run clippy.\n",
        )
        .unwrap();

        let proj_skill = cwd.join(".envoy").join("skills").join("deploy-helper");
        std::fs::create_dir_all(proj_skill.join("scripts")).unwrap();
        std::fs::write(
            proj_skill.join("scripts").join("check.sh"),
            "#!/bin/sh\necho ok\n",
        )
        .unwrap();
        std::fs::write(
            proj_skill.join("SKILL.md"),
            "---\nname: deploy-helper\ndescription: Project deployment checklist\n---\n\nAlways verify dry-run first.\n",
        )
        .unwrap();

        let untrusted = discover_skills(&data, &cwd);
        assert_eq!(untrusted.len(), 1);
        assert_eq!(untrusted[0].name, "rust-review");
        assert_eq!(untrusted[0].allowed_tools, vec!["read", "grep"]);
        assert_eq!(
            untrusted[0].metadata.get("author").map(String::as_str),
            Some("envoy")
        );

        let missing = load_skill(&data, &cwd, "deploy-helper").unwrap_err();
        assert!(missing.to_string().contains("available: rust-review"));

        set_workspace_trusted(&data, &cwd, true).unwrap();
        let trusted = discover_skills(&data, &cwd);
        assert_eq!(trusted.len(), 2);

        let loaded = load_skill(&data, &cwd, "deploy-helper").unwrap();
        assert_eq!(loaded.summary.scope, SkillScope::Project);
        assert!(loaded.body.contains("Always verify dry-run first."));
        assert_eq!(loaded.resources, vec!["scripts/check.sh"]);

        let empty_root = tmpdir("empty");
        let empty_err = load_skill(&empty_root, &empty_root, "pdf").unwrap_err();
        assert!(empty_err.to_string().contains("no skills installed"));
        let _ = std::fs::remove_dir_all(&empty_root);

        let catalog = format_skills_catalog(&trusted).unwrap();
        assert!(catalog.contains("<available_skills>"));
        assert!(catalog.contains("name=\"deploy-helper\""));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prompt_lists_only_envoy_skill_roots() {
        let root = tmpdir("inject");
        let data = root.join("data");
        let cwd = root.join("repo");
        for (rel, name) in [
            ("skills/from-data", "from-data"),
            (".envoy/skills/from-envoy", "from-envoy"),
            (".claude/skills/from-claude", "from-claude"),
            (".agents/skills/from-agents", "from-agents"),
        ] {
            let dir = if rel.starts_with("skills/") {
                data.join(rel)
            } else {
                cwd.join(rel)
            };
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} steps\n---\n\nbody\n"),
            )
            .unwrap();
        }
        set_workspace_trusted(&data, &cwd, true).unwrap();

        let discovered = discover_skills(&data, &cwd);
        let names: Vec<_> = discovered.iter().map(|skill| skill.name.as_str()).collect();
        assert!(names.contains(&"from-data"), "{names:?}");
        assert!(names.contains(&"from-envoy"), "{names:?}");
        assert!(names.contains(&"from-claude"), "{names:?}");
        assert!(names.contains(&"from-agents"), "{names:?}");

        let prompted = prompt_skills(&data, &cwd);
        let prompted_names: Vec<_> = prompted.iter().map(|skill| skill.name.as_str()).collect();
        assert!(prompted_names.contains(&"from-data"), "{prompted_names:?}");
        assert!(prompted_names.contains(&"from-envoy"), "{prompted_names:?}");
        assert!(
            !prompted_names.contains(&"from-claude"),
            "{prompted_names:?}"
        );
        assert!(
            !prompted_names.contains(&"from-agents"),
            "{prompted_names:?}"
        );

        let loaded = load_skill(&data, &cwd, "from-claude").unwrap();
        assert!(loaded.body.contains("body"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skill_pref_overrides_the_default_roots() {
        let root = tmpdir("pref");
        let data = root.join("data");
        let cwd = root.join("repo");
        for (rel, name) in [
            ("skills/from-data", "from-data"),
            (".envoy/skills/from-envoy", "from-envoy"),
            (".claude/skills/from-claude", "from-claude"),
        ] {
            let dir = if rel.starts_with("skills/") {
                data.join(rel)
            } else {
                cwd.join(rel)
            };
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} steps\n---\n\nbody\n"),
            )
            .unwrap();
        }
        set_workspace_trusted(&data, &cwd, true).unwrap();
        prefs::set_inject(&data, "from-claude", true).unwrap();
        prefs::set_inject(&data, "from-data", false).unwrap();
        let prompted = prompt_skills(&data, &cwd);
        let names: Vec<_> = prompted.iter().map(|skill| skill.name.as_str()).collect();
        assert!(names.contains(&"from-claude"), "{names:?}");
        assert!(names.contains(&"from-envoy"), "{names:?}");
        assert!(!names.contains(&"from-data"), "{names:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
