//! Cross-workspace memory list on the home screen.
use super::super::*;

impl App {
    pub(super) fn reload_welcome_notes(&mut self) {
        let notes = crate::memory::list(&self.data_dir);
        if let Some(welcome) = self.welcome.as_mut() {
            let len = notes.len();
            if len == 0 {
                welcome.list_selected = 0;
            } else if welcome.list_selected >= len {
                welcome.list_selected = len - 1;
            }
            welcome.memory_count = len;
            welcome.notes = notes;
        }
    }

    pub(super) fn handle_welcome_memory(&mut self, key: KeyEvent) -> Action {
        let len = self.welcome.as_ref().map(|w| w.notes.len()).unwrap_or(0);
        match key.code {
            KeyCode::Esc => {
                self.leave_welcome_panel();
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.bump_welcome_list(-1, len);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.bump_welcome_list(1, len);
                Action::None
            }
            KeyCode::Char('n') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.begin_welcome_name();
                Action::None
            }
            KeyCode::Char('e') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.edit_selected_note()
            }
            KeyCode::Char('d') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_selected_note();
                Action::None
            }
            KeyCode::Enter => {
                self.toggle_selected_note();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn edit_selected_note(&mut self) -> Action {
        let path = self
            .welcome
            .as_ref()
            .and_then(|w| w.notes.get(w.list_selected))
            .map(|note| note.path.clone());
        match path {
            Some(path) => Action::EditFile(path),
            None => Action::None,
        }
    }

    fn toggle_selected_note(&mut self) {
        let Some(note) = self
            .welcome
            .as_ref()
            .and_then(|w| w.notes.get(w.list_selected).cloned())
        else {
            return;
        };
        let selected = self.welcome.as_ref().map(|w| w.list_selected).unwrap_or(0);
        if let Err(err) = crate::memory::set_enabled(&note, !note.enabled) {
            if let Some(welcome) = self.welcome.as_mut() {
                welcome.error = Some(err);
            }
            return;
        }
        if let Some(welcome) = self.welcome.as_mut() {
            welcome.error = None;
        }
        self.reload_welcome_notes();
        if let Some(welcome) = self.welcome.as_mut() {
            welcome.list_selected = selected.min(welcome.notes.len().saturating_sub(1));
        }
    }

    fn delete_selected_note(&mut self) {
        let Some(note) = self
            .welcome
            .as_ref()
            .and_then(|w| w.notes.get(w.list_selected).cloned())
        else {
            return;
        };
        if let Err(err) = crate::memory::delete(&note) {
            if let Some(welcome) = self.welcome.as_mut() {
                welcome.error = Some(err);
            }
            return;
        }
        if let Some(welcome) = self.welcome.as_mut() {
            welcome.error = None;
        }
        self.reload_welcome_notes();
    }
}
