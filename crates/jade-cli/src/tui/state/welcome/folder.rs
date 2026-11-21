//! Interactive workspace directory browser and single-level folder creation from the Hub.
use super::super::*;
use crate::tui::line_edit::{self, EditOutcome};
use std::path::{Path, PathBuf};

/// Build the visible entries for the Hub directory browser (`o` / `N`).
pub(in crate::tui) fn build_path_browser_entries(
    browse_dir: &Path,
    raw: &str,
) -> Vec<PathBrowserEntry> {
    let trimmed = raw.trim();
    if is_explicit_path(trimmed) {
        return build_explicit_path_entries(browse_dir, trimmed);
    }
    let mut entries = Vec::new();
    if trimmed.is_empty() || trimmed == "." {
        entries.push(PathBrowserEntry::OpenCurrent(browse_dir.to_path_buf()));
        if trimmed.is_empty() {
            if let Some(parent) = browse_dir.parent() {
                entries.push(PathBrowserEntry::Parent(parent.to_path_buf()));
            }
        }
    }
    let filter = if trimmed == "." { "" } else { trimmed };
    for (display, path) in list_subdirectories(browse_dir, filter) {
        entries.push(PathBrowserEntry::Dir { display, path });
    }
    if entries.is_empty() && !trimmed.is_empty() {
        entries.push(PathBrowserEntry::TypedPath {
            display: trimmed.to_string(),
            raw: trimmed.to_string(),
        });
    }
    entries
}

fn is_explicit_path(trimmed: &str) -> bool {
    trimmed.starts_with('/')
        || trimmed == "~"
        || trimmed.starts_with("~/")
        || trimmed == ".."
        || trimmed.starts_with("../")
        || trimmed.starts_with("./")
        || trimmed.contains('/')
}

fn build_explicit_path_entries(browse_dir: &Path, trimmed: &str) -> Vec<PathBrowserEntry> {
    let mut entries = Vec::new();
    let resolved = resolve_cd_path(browse_dir, trimmed);
    let canon = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
    let resolved_is_dir = canon.is_dir();

    if trimmed.ends_with('/') || trimmed == "~" || trimmed == ".." {
        if resolved_is_dir {
            entries.push(PathBrowserEntry::OpenCurrent(canon.clone()));
            if let Some(parent) = canon.parent() {
                entries.push(PathBrowserEntry::Parent(parent.to_path_buf()));
            }
            for (display, path) in list_subdirectories(&canon, "") {
                entries.push(PathBrowserEntry::Dir { display, path });
            }
        } else {
            entries.push(PathBrowserEntry::TypedPath {
                display: trimmed.to_string(),
                raw: trimmed.to_string(),
            });
        }
        return entries;
    }

    if resolved_is_dir {
        entries.push(PathBrowserEntry::OpenCurrent(canon.clone()));
    }
    if let Some(idx) = trimmed.rfind('/') {
        let parent_raw = &trimmed[..=idx];
        let name_prefix = &trimmed[idx + 1..];
        let parent_dir = resolve_cd_path(browse_dir, parent_raw);
        for (display, path) in list_subdirectories(&parent_dir, name_prefix) {
            let item_canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if resolved_is_dir && item_canon == canon {
                continue;
            }
            entries.push(PathBrowserEntry::Dir { display, path });
        }
    }
    if entries.is_empty() {
        entries.push(PathBrowserEntry::TypedPath {
            display: trimmed.to_string(),
            raw: trimmed.to_string(),
        });
    }
    entries
}

impl App {
    pub(super) fn handle_path_keys(&mut self, key: KeyEvent) -> Action {
        let choosing_parent = self
            .ui
            .welcome
            .as_ref()
            .is_some_and(|w| w.mode == WelcomeMode::NewFolderPickParent);
        let (browse_dir, raw) = match self.ui.welcome.as_ref() {
            Some(w) => (w.browse_dir.clone(), w.path.clone()),
            None => return Action::None,
        };
        let entries = build_path_browser_entries(&browse_dir, &raw);

        match key.code {
            KeyCode::Esc => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.mode = WelcomeMode::Home;
                    w.path.clear();
                    w.path_cursor = 0;
                    w.path_selected = 0;
                    w.error = None;
                }
                return Action::None;
            }
            KeyCode::Up => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.path_selected = w.path_selected.saturating_sub(1);
                    w.error = None;
                }
                return Action::None;
            }
            KeyCode::Down => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    if !entries.is_empty() {
                        w.path_selected = (w.path_selected + 1).min(entries.len() - 1);
                    }
                    w.error = None;
                }
                return Action::None;
            }
            KeyCode::Left
                if key.modifiers.is_empty()
                    && self.ui.welcome.as_ref().is_some_and(|w| w.path.is_empty()) =>
            {
                self.path_browser_go_up();
                return Action::None;
            }
            KeyCode::Backspace
                if key.modifiers.is_empty()
                    && self.ui.welcome.as_ref().is_some_and(|w| w.path.is_empty()) =>
            {
                self.path_browser_go_up();
                return Action::None;
            }
            KeyCode::Right
                if key.modifiers.is_empty()
                    && self
                        .ui
                        .welcome
                        .as_ref()
                        .is_some_and(|w| w.path_cursor >= w.path.chars().count()) =>
            {
                self.path_browser_step_into(&entries);
                return Action::None;
            }
            KeyCode::Tab => {
                self.path_browser_step_into(&entries);
                return Action::None;
            }
            KeyCode::Enter => {
                return self.activate_path_entry(&entries, choosing_parent);
            }
            _ => {}
        }

        if let Some(w) = self.ui.welcome.as_mut() {
            match line_edit::handle_key(&mut w.path, &mut w.path_cursor, key) {
                EditOutcome::Changed => {
                    w.path_selected = 0;
                    w.error = None;
                }
                EditOutcome::Moved | EditOutcome::Ignored => {}
            }
        }
        Action::None
    }

    fn path_browser_go_up(&mut self) {
        let Some(w) = self.ui.welcome.as_mut() else {
            return;
        };
        let current = std::fs::canonicalize(&w.browse_dir).unwrap_or_else(|_| w.browse_dir.clone());
        let Some(parent) = current.parent().map(Path::to_path_buf) else {
            return;
        };
        w.browse_dir = parent.clone();
        w.path.clear();
        w.path_cursor = 0;
        w.error = None;
        let next_entries = build_path_browser_entries(&parent, "");
        w.path_selected = next_entries
            .iter()
            .position(|entry| match entry {
                PathBrowserEntry::Dir { path, .. } => {
                    std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()) == current
                }
                _ => false,
            })
            .unwrap_or(0);
    }

    fn path_browser_descend(&mut self, target: PathBuf) {
        let canon = std::fs::canonicalize(&target).unwrap_or(target);
        if !canon.is_dir() {
            if let Some(w) = self.ui.welcome.as_mut() {
                w.error = Some(format!("not a directory: {}", canon.display()));
            }
            return;
        }
        if let Some(w) = self.ui.welcome.as_mut() {
            w.browse_dir = canon;
            w.path.clear();
            w.path_cursor = 0;
            w.path_selected = 0;
            w.error = None;
        }
    }

    fn path_browser_step_into(&mut self, entries: &[PathBrowserEntry]) {
        let sel = self
            .ui
            .welcome
            .as_ref()
            .map(|w| w.path_selected.min(entries.len().saturating_sub(1)))
            .unwrap_or(0);
        match entries.get(sel) {
            Some(PathBrowserEntry::Dir { path, .. }) => {
                self.path_browser_descend(path.clone());
            }
            Some(PathBrowserEntry::Parent(_)) => {
                self.path_browser_go_up();
            }
            Some(PathBrowserEntry::OpenCurrent(path)) => {
                self.path_browser_descend(path.clone());
            }
            _ => {}
        }
    }

    fn activate_path_entry(
        &mut self,
        entries: &[PathBrowserEntry],
        choosing_parent: bool,
    ) -> Action {
        let sel = self
            .ui
            .welcome
            .as_ref()
            .map(|w| w.path_selected.min(entries.len().saturating_sub(1)))
            .unwrap_or(0);
        match entries.get(sel).cloned() {
            Some(PathBrowserEntry::OpenCurrent(path)) => {
                self.commit_browser_path(path, choosing_parent)
            }
            Some(PathBrowserEntry::Parent(_)) => {
                self.path_browser_go_up();
                Action::None
            }
            Some(PathBrowserEntry::Dir { path, .. }) => {
                self.path_browser_descend(path);
                Action::None
            }
            Some(PathBrowserEntry::TypedPath { raw, .. }) => {
                let base = self
                    .ui
                    .welcome
                    .as_ref()
                    .map(|w| w.browse_dir.clone())
                    .unwrap_or_else(|| self.session.current.cwd.clone());
                let candidate = resolve_cd_path(&base, raw.trim());
                self.commit_browser_path(candidate, choosing_parent)
            }
            None => {
                let fallback = self
                    .ui
                    .welcome
                    .as_ref()
                    .map(|w| resolve_cd_path(&w.browse_dir, w.path.trim()))
                    .unwrap_or_else(|| self.session.current.cwd.clone());
                self.commit_browser_path(fallback, choosing_parent)
            }
        }
    }

    fn commit_browser_path(&mut self, candidate: PathBuf, choosing_parent: bool) -> Action {
        if choosing_parent {
            return self.begin_folder_name_path(candidate);
        }
        let path = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if !path.is_dir() {
            if let Some(w) = self.ui.welcome.as_mut() {
                w.error = Some(format!("not a directory: {}", path.display()));
            }
            return Action::None;
        }
        self.activate_workspace(path);
        Action::None
    }

    pub(in crate::tui) fn hub_click_path_entry(&mut self, idx: usize) -> Action {
        let (choosing_parent, browse_dir, raw, already) = match self.ui.welcome.as_ref() {
            Some(w) if w.mode.is_path_input() => (
                w.mode == WelcomeMode::NewFolderPickParent,
                w.browse_dir.clone(),
                w.path.clone(),
                w.path_selected == idx,
            ),
            _ => return Action::None,
        };
        let entries = build_path_browser_entries(&browse_dir, &raw);
        if idx >= entries.len() {
            return Action::None;
        }
        if let Some(w) = self.ui.welcome.as_mut() {
            w.path_selected = idx;
            w.error = None;
        }
        let immediate = matches!(
            entries.get(idx),
            Some(PathBrowserEntry::OpenCurrent(_) | PathBrowserEntry::Parent(_))
        );
        if already || immediate {
            return self.activate_path_entry(&entries, choosing_parent);
        }
        Action::None
    }

    fn begin_folder_name_path(&mut self, candidate: PathBuf) -> Action {
        let path = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if !path.is_dir() {
            if let Some(welcome) = self.ui.welcome.as_mut() {
                welcome.error = Some(format!("not a directory: {}", path.display()));
            }
            return Action::None;
        }
        if jade_agent::permissions::paths::path_has_protected_component(&path) {
            if let Some(welcome) = self.ui.welcome.as_mut() {
                welcome.error = Some("refusing to create a folder inside a protected path".into());
            }
            return Action::None;
        }
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.folder_parent = Some(path);
            welcome.mode = WelcomeMode::NewFolderName;
            welcome.draft.clear();
            welcome.draft_cursor = 0;
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
    if jade_agent::permissions::paths::path_has_protected_component(Path::new(name)) {
        return Err("refusing a protected folder name".into());
    }
    if !parent.is_dir() {
        return Err("not a directory".into());
    }
    let parent = std::fs::canonicalize(parent).map_err(|_| "not a directory".to_string())?;
    if jade_agent::permissions::paths::path_has_protected_component(&parent) {
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
            "jade-folder-{:02x}{:02x}{:02x}{:02x}",
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
