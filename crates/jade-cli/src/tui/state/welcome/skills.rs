//! User-skill list on the home screen. Toggles write skills.json only.
use super::super::*;

impl App {
    pub(super) fn reload_welcome_skills(&mut self) {
        let data = self.data_dir.clone();
        let cwd = self.session.current.cwd.clone();
        let mut skills: Vec<_> = jade_agent::skills::discover_skills(&data, &cwd)
            .into_iter()
            .filter(|skill| skill.scope == jade_agent::skills::SkillScope::User)
            .collect();
        skills.sort_by(|a, b| {
            source_ord(jade_agent::skills::skill_root_kind(&data, a))
                .cmp(&source_ord(jade_agent::skills::skill_root_kind(&data, b)))
                .then(a.name.cmp(&b.name))
        });
        if let Some(welcome) = self.ui.welcome.as_mut() {
            let len = skills.len();
            if len == 0 {
                welcome.list_selected = 0;
            } else if welcome.list_selected >= len {
                welcome.list_selected = len - 1;
            }
            welcome.skills = skills;
            welcome.skill_count = len;
        }
        self.inputs.invalidate_catalog();
    }

    pub(super) fn handle_welcome_skills(&mut self, key: KeyEvent) -> Action {
        let len = self
            .ui
            .welcome
            .as_ref()
            .map(|w| w.skills.len())
            .unwrap_or(0);
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
                self.edit_selected_skill()
            }
            KeyCode::Enter => {
                self.toggle_selected_skill();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn edit_selected_skill(&mut self) -> Action {
        let path = self
            .ui
            .welcome
            .as_ref()
            .and_then(|w| w.skills.get(w.list_selected))
            .map(|skill| skill.skill_file.clone());
        match path {
            Some(path) => Action::EditFile(path),
            None => Action::None,
        }
    }

    fn toggle_selected_skill(&mut self) {
        let Some(skill) = self
            .ui
            .welcome
            .as_ref()
            .and_then(|w| w.skills.get(w.list_selected).cloned())
        else {
            return;
        };
        let on = jade_agent::skills::skill_is_injected(
            &self.data_dir,
            &self.session.current.cwd,
            &skill,
        );
        let result = jade_agent::skills::prefs::set_inject(&self.data_dir, &skill.name, !on);
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.error = result.err();
        }
        self.inputs.invalidate_catalog();
    }
}

fn source_ord(kind: &str) -> u8 {
    match kind {
        "jade" => 0,
        "claude" => 1,
        "agents" => 2,
        _ => 3,
    }
}
