//! Single-level workspace directory creation from the home screen.
use super::super::*;
use std::path::{Path, PathBuf};

impl App {
    pub(super) fn handle_path_keys(&mut self, key: KeyEvent) -> Action {
        let choose_parent = self
            .ui.welcome
            .as_ref()
            .is_some_and(|w| w.mode == WelcomeMode::NewFolderPickParent);
        let mut submit: Option<String> = None;
        let cwd = self.session.current.cwd.clone();
        if let Some(welcome) = self.ui.welcome.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    welcome.mode = WelcomeMode::Home;
                    welcome.path_selected = 0;
                    welcome.error = None;
                }
                KeyCode::Up => {
                    welcome.path_selected = welcome.path_selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    let comps = complete_directories(&cwd, &welcome.path);
                    if !comps.is_empty() {
                        welcome.path_selected = (welcome.path_selected + 1).min(comps.len() - 1);
                    }
                }
                KeyCode::Backspace => {
                    welcome.path.pop();
                    welcome.path_selected = 0;
                    welcome.error = None;
                }
                KeyCode::Tab => {
                    let comps = complete_directories(&cwd, &welcome.path);
                    let idx = welcome.path_selected.min(comps.len().saturating_sub(1));
                    if let Some((_, arg)) = comps.get(idx) {
                        welcome.path = arg.clone();
                        welcome.path_selected = 0;
                        welcome.error = None;
                    }
                }
                KeyCode::Enter => {
                    let comps = complete_directories(&cwd, &welcome.path);
                    let sel = welcome.path_selected.min(comps.len().saturating_sub(1));
                    let typed = welcome.path.trim();
                    let typed_is_dir = !typed.is_empty() && resolve_cd_path(&cwd, typed).is_dir();
                    if welcome.path_selected > 0 {
                        if let Some((_, arg)) = comps.get(sel) {
                            submit = Some(arg.clone());
                        }
                    } else if typed_is_dir || typed.is_empty() {
                        submit = Some(welcome.path.clone());
                    } else if let Some((_, arg)) = comps.get(sel) {
                        submit = Some(arg.clone());
                    } else {
                        submit = Some(welcome.path.clone());
                    }
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    welcome.path.push(c);
                    welcome.path_selected = 0;
                    welcome.error = None;
                }
                _ => {}
            }
        }
        if let Some(raw) = submit {
            if choose_parent {
                return self.begin_folder_name(&raw);
            }
            return self.enter_workspace_path(&raw);
        }
        Action::None
    }

    fn begin_folder_name(&mut self, raw: &str) -> Action {
        let candidate = resolve_cd_path(&self.session.current.cwd, raw.trim());
        let path = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if !path.is_dir() {
            if let Some(welcome) = self.ui.welcome.as_mut() {
                welcome.error = Some(format!("not a directory: {}", path.display()));
            }
            return Action::None;
        }
        if envoy_agent::permissions::paths::path_has_protected_component(&path) {
            if let Some(welcome) = self.ui.welcome.as_mut() {
                welcome.error = Some("refusing to create a folder inside a protected path".into());
            }
            return Action::None;
        }
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.folder_parent = Some(path);
            welcome.mode = WelcomeMode::NewFolderName;
            welcome.draft.clear();
            welcome.error = None;
        }
        Action::None
    }
}

pub(super) fn create_workspace_dir(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("folder name is empty".into());
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err("folder name must be a single directory".into());
    }
    if envoy_agent::permissions::paths::path_has_protected_component(Path::new(name)) {
        return Err("refusing a protected folder name".into());
    }
    if !parent.is_dir() {
        return Err("not a directory".into());
    }
    let parent = std::fs::canonicalize(parent).map_err(|_| "not a directory".to_string())?;
    if envoy_agent::permissions::paths::path_has_protected_component(&parent) {
        return Err("refusing to create a folder inside a protected path".into());
    }
    let dest = parent.join(name);
    if dest.exists() {
        return Err("already exists".into());
    }
    std::fs::create_dir(&dest).map_err(|_| "cannot create".to_string())?;
    std::fs::canonicalize(&dest).map_err(|_| "cannot create".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "envoy-folder-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn creates_one_level_and_refuses_bad_targets() {
        let root = tmpdir();
        let made = create_workspace_dir(&root, "fresh").unwrap();
        assert!(made.is_dir());
        assert_eq!(made.file_name().and_then(|n| n.to_str()), Some("fresh"));
        assert!(create_workspace_dir(&root, "fresh")
            .unwrap_err()
            .contains("already exists"));
        assert!(create_workspace_dir(&root, " ")
            .unwrap_err()
            .contains("empty"));
        assert!(create_workspace_dir(&root, "a/b")
            .unwrap_err()
            .contains("single"));
        assert!(create_workspace_dir(&root, "..")
            .unwrap_err()
            .contains("single"));
        assert!(create_workspace_dir(&root, ".git")
            .unwrap_err()
            .contains("protected"));
        let missing = root.join("missing");
        let err = create_workspace_dir(&missing, "child").unwrap_err();
        assert!(err.contains("not a directory"));
        assert!(!missing.exists());
        let protected = root.join(".ssh");
        std::fs::create_dir(&protected).unwrap();
        let err = create_workspace_dir(&protected, "child").unwrap_err();
        assert!(err.contains("protected"));
        assert!(!protected.join("child").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
