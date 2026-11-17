//! Agent Skills (`SKILL.md`) catalog discovery and on-demand loader.

pub mod loader;

pub use loader::{
    discover_skills, format_skills_catalog, load_skill, LoadedSkill, SkillScope, SkillSummary,
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
        std::fs::write(proj_skill.join("scripts").join("check.sh"), "#!/bin/sh\necho ok\n").unwrap();
        std::fs::write(
            proj_skill.join("SKILL.md"),
            "---\nname: deploy-helper\ndescription: Project deployment checklist\n---\n\nAlways verify dry-run first.\n",
        )
        .unwrap();

        let untrusted = discover_skills(&data, &cwd);
        assert_eq!(untrusted.len(), 1);
        assert_eq!(untrusted[0].name, "rust-review");
        assert_eq!(untrusted[0].allowed_tools, vec!["read", "grep"]);
        assert_eq!(untrusted[0].metadata.get("author").map(String::as_str), Some("envoy"));

        assert!(load_skill(&data, &cwd, "deploy-helper").is_err());

        set_workspace_trusted(&data, &cwd, true).unwrap();
        let trusted = discover_skills(&data, &cwd);
        assert_eq!(trusted.len(), 2);

        let loaded = load_skill(&data, &cwd, "deploy-helper").unwrap();
        assert_eq!(loaded.summary.scope, SkillScope::Project);
        assert!(loaded.body.contains("Always verify dry-run first."));
        assert_eq!(loaded.resources, vec!["scripts/check.sh"]);

        let catalog = format_skills_catalog(&trusted).unwrap();
        assert!(catalog.contains("<available_skills>"));
        assert!(catalog.contains("name=\"deploy-helper\""));

        let _ = std::fs::remove_dir_all(&root);
    }
}
