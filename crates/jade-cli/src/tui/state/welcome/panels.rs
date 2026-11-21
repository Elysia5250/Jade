//! Hub naming modals (folder/session/skill/memory) and sub-panel mouse/scroll dispatch.
use super::super::*;
use super::folder;
use super::step_index;
use crate::tui::line_edit::{self, EditOutcome};

impl App {
    pub(super) fn handle_welcome_naming(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                if let Some(welcome) = self.ui.welcome.as_mut() {
                    welcome.mode = match welcome.mode {
                        WelcomeMode::NewFolderName => WelcomeMode::NewFolderPickParent,
                        WelcomeMode::SkillsNew => WelcomeMode::Skills,
                        WelcomeMode::MemoryNew => WelcomeMode::Memory,
                        _ => WelcomeMode::Home,
                    };
                    welcome.draft.clear();
                    welcome.draft_cursor = 0;
                    welcome.rename_target = None;
                    welcome.error = None;
                }
                Action::None
            }
            KeyCode::Enter => self.submit_welcome_name(),
            _ => {
                if let Some(welcome) = self.ui.welcome.as_mut() {
                    if line_edit::handle_key(&mut welcome.draft, &mut welcome.draft_cursor, key)
                        == EditOutcome::Changed
                    {
                        welcome.error = None;
                    }
                }
                Action::None
            }
        }
    }

    fn submit_welcome_name(&mut self) -> Action {
        let Some(welcome) = self.ui.welcome.as_ref() else {
            return Action::None;
        };
        let mode = welcome.mode;
        let draft = welcome.draft.clone();
        let parent = welcome.folder_parent.clone();
        let rename_id = welcome.rename_target.clone();
        match mode {
            WelcomeMode::NewFolderName => self.submit_new_folder(parent, &draft),
            WelcomeMode::RenameSession => self.submit_rename_session(rename_id, &draft),
            WelcomeMode::SkillsNew => self.submit_new_skill(&draft),
            WelcomeMode::MemoryNew => self.submit_new_memory(&draft),
            _ => Action::None,
        }
    }

    fn submit_new_folder(&mut self, parent: Option<PathBuf>, draft: &str) -> Action {
        let Some(parent) = parent else {
            if let Some(w) = self.ui.welcome.as_mut() {
                w.error = Some("not a directory".into());
            }
            return Action::None;
        };
        match folder::create_workspace_dir(&parent, draft) {
            Ok(path) => {
                self.activate_workspace_with_mode(path, false);
                Action::None
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(err);
                }
                Action::None
            }
        }
    }

    fn submit_rename_session(&mut self, id: Option<String>, draft: &str) -> Action {
        let Some(id) = id else {
            return Action::None;
        };
        match rename_session(&self.data_dir, &id, draft) {
            Ok(()) => {
                if self.session.current.id == id {
                    self.session.current.title = draft.trim().chars().take(64).collect();
                }
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.mode = WelcomeMode::Home;
                    w.draft.clear();
                    w.draft_cursor = 0;
                    w.rename_target = None;
                    w.error = None;
                }
                self.reload_hub_lists();
                self.set_toast("session renamed", false);
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(format!("{err:#}"));
                }
            }
        }
        Action::None
    }

    fn submit_new_skill(&mut self, draft: &str) -> Action {
        match jade_agent::skills::prefs::create_user_skill(&self.data_dir, draft) {
            Ok(path) => {
                let name = draft.trim().to_string();
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.mode = WelcomeMode::Skills;
                    w.draft.clear();
                    w.draft_cursor = 0;
                    w.error = None;
                }
                self.reload_welcome_skills();
                if let Some(w) = self.ui.welcome.as_mut() {
                    if let Some(idx) = w.skills.iter().position(|skill| skill.name == name) {
                        w.list_selected = idx;
                    }
                }
                Action::EditFile(path)
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(err);
                }
                Action::None
            }
        }
    }

    fn submit_new_memory(&mut self, draft: &str) -> Action {
        let cwd = self.selected_hub_cwd();
        match jade_agent::memory::create(&self.data_dir, &cwd, draft) {
            Ok(note) => {
                let id = note.id.clone();
                let path = note.path.clone();
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.mode = WelcomeMode::Memory;
                    w.draft.clear();
                    w.draft_cursor = 0;
                    w.error = None;
                }
                self.reload_welcome_notes();
                if let Some(w) = self.ui.welcome.as_mut() {
                    if let Some(idx) = w.notes.iter().position(|item| item.id == id) {
                        w.list_selected = idx;
                    }
                }
                Action::EditFile(path)
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(err);
                }
                Action::None
            }
        }
    }

    pub(in crate::tui) fn scroll_welcome_popup(&mut self, delta: isize) {
        let Some(mode) = self.ui.welcome.as_ref().map(|w| w.mode) else {
            return;
        };
        match mode {
            WelcomeMode::Home => self.scroll_hub(delta),
            WelcomeMode::OpenPath | WelcomeMode::NewFolderPickParent => {
                let len = self.inputs.welcome_path_entries.len();
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.path_selected = step_index(w.path_selected, delta, len);
                    w.error = None;
                }
            }
            WelcomeMode::Skills => {
                let len = self
                    .ui
                    .welcome
                    .as_ref()
                    .map(|w| w.skills.len())
                    .unwrap_or(0);
                self.bump_welcome_list(delta, len);
            }
            WelcomeMode::Memory => {
                let len = self.ui.welcome.as_ref().map(|w| w.notes.len()).unwrap_or(0);
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.confirm_delete = None;
                }
                self.bump_welcome_list(delta, len);
            }
            WelcomeMode::Mcp => {
                let len = self.inputs.catalog.mcp.len();
                self.bump_welcome_list(delta, len);
            }
            _ => {}
        }
    }

    pub(in crate::tui) fn hub_click_list_item(&mut self, idx: usize) {
        let Some(mode) = self.ui.welcome.as_ref().map(|w| w.mode) else {
            return;
        };
        match mode {
            WelcomeMode::Skills => {
                let already = self
                    .ui
                    .welcome
                    .as_ref()
                    .is_some_and(|w| w.list_selected == idx);
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.list_selected = idx.min(w.skills.len().saturating_sub(1));
                }
                if already {
                    self.toggle_selected_skill();
                }
            }
            WelcomeMode::Memory => {
                let already = self
                    .ui
                    .welcome
                    .as_ref()
                    .is_some_and(|w| w.list_selected == idx);
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.list_selected = idx.min(w.notes.len().saturating_sub(1));
                    w.confirm_delete = None;
                }
                if already {
                    self.toggle_selected_note();
                }
            }
            WelcomeMode::Mcp => {
                let max = self.inputs.catalog.mcp.len().saturating_sub(1);
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.list_selected = idx.min(max);
                }
            }
            _ => {}
        }
    }

    pub(in crate::tui) fn dismiss_welcome_subpanel(&mut self) {
        let Some(mode) = self.ui.welcome.as_ref().map(|w| w.mode) else {
            return;
        };
        if mode == WelcomeMode::Home {
            return;
        }
        let _ = self.handle_welcome_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }
}
