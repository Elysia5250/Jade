//! Per-skill injection switches in `<data_dir>/skills.json`.
//!
//! A missing key keeps the loader default: Jade roots on, Claude and Agents
//! roots off. The file never moves or deletes a skill directory.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SkillPrefs {
    #[serde(default)]
    inject: BTreeMap<String, bool>,
}

fn prefs_path(data_dir: &Path) -> PathBuf {
    data_dir.join("skills.json")
}

fn load(data_dir: &Path) -> SkillPrefs {
    let Ok(raw) = std::fs::read_to_string(prefs_path(data_dir)) else {
        return SkillPrefs::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save(data_dir: &Path, prefs: &SkillPrefs) -> Result<(), String> {
    let text = serde_json::to_string_pretty(prefs).map_err(|err| err.to_string())?;
    jade_core::fs::write_atomic_sync(&prefs_path(data_dir), text.as_bytes(), 0o600)
        .map_err(|err| format!("cannot create: {err:#}"))
}

pub fn injected(data_dir: &Path, name: &str, default_on: bool) -> bool {
    load(data_dir)
        .inject
        .get(name)
        .copied()
        .unwrap_or(default_on)
}

pub fn set_inject(data_dir: &Path, name: &str, on: bool) -> Result<(), String> {
    let mut prefs = load(data_dir);
    prefs.inject.insert(name.to_string(), on);
    save(data_dir, &prefs)
}

/// One directory under `<data_dir>/skills/<name>/SKILL.md`.
pub fn create_user_skill(data_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("skill name is empty".into());
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err("skill name must be a single directory".into());
    }
    if crate::permissions::paths::path_has_protected_component(Path::new(name)) {
        return Err("refusing a protected skill name".into());
    }
    let root = data_dir.join("skills");
    std::fs::create_dir_all(&root).map_err(|err| format!("cannot create: {err}"))?;
    let dir = root.join(name);
    if dir.exists() {
        return Err("already exists".into());
    }
    std::fs::create_dir(&dir).map_err(|err| format!("cannot create: {err}"))?;
    let file = dir.join("SKILL.md");
    let body = format!("---\nname: {name}\ndescription: {name}\n---\n\n# Steps\n");
    std::fs::write(&file, body).map_err(|err| format!("cannot create: {err}"))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "jade-prefs-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_pref_keeps_the_default_and_override_wins() {
        let dir = tmpdir();
        assert!(injected(&dir, "from-data", true));
        assert!(!injected(&dir, "from-claude", false));
        set_inject(&dir, "from-data", false).unwrap();
        set_inject(&dir, "from-claude", true).unwrap();
        assert!(!injected(&dir, "from-data", true));
        assert!(injected(&dir, "from-claude", false));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(prefs_path(&dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_prefs_fall_back_to_the_default() {
        let dir = tmpdir();
        std::fs::write(prefs_path(&dir), "{").unwrap();
        assert!(injected(&dir, "from-data", true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_user_skill_writes_a_skeleton_and_rejects_bad_names() {
        let dir = tmpdir();
        let file = create_user_skill(&dir, "demo").unwrap();
        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(raw.contains("name: demo"));
        assert!(raw.contains("# Steps"));
        assert!(create_user_skill(&dir, "demo")
            .unwrap_err()
            .contains("already exists"));
        assert!(create_user_skill(&dir, "a/b")
            .unwrap_err()
            .contains("single"));
        assert!(create_user_skill(&dir, ".ssh")
            .unwrap_err()
            .contains("protected"));
        assert!(create_user_skill(&dir, " ").unwrap_err().contains("empty"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
