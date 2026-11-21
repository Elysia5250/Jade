//! Quick Controls panel actions on the Hub right pane.
use super::super::*;
use jade_agent::agent::delegate_pref::{self, DelegateLevel};
use jade_agent::agent::effort;
use jade_agent::agent::CompactionTier;

const BUDGET_TIERS: [(&str, CompactionTier); 4] = [
    ("native", CompactionTier::Native),
    ("focused", CompactionTier::Focused),
    ("balanced", CompactionTier::Balanced),
    ("max", CompactionTier::Max),
];

const DELEGATE_LEVELS: [DelegateLevel; 4] = [
    DelegateLevel::Off,
    DelegateLevel::Low,
    DelegateLevel::Normal,
    DelegateLevel::High,
];

const PERM_MODES: [PermissionMode; 5] = [
    PermissionMode::Default,
    PermissionMode::AcceptEdits,
    PermissionMode::Plan,
    PermissionMode::Auto,
    PermissionMode::Turbo,
];

impl App {
    pub(super) fn activate_hub_control(&mut self, ctrl: HubControl, dir: isize) -> Action {
        match ctrl {
            HubControl::Provider => {
                self.open_login_modal();
                Action::None
            }
            HubControl::Model => {
                self.open_models_modal();
                Action::None
            }
            HubControl::Effort => {
                self.cycle_effort(dir);
                Action::None
            }
            HubControl::Budget => {
                self.cycle_hub_budget(dir);
                Action::None
            }
            HubControl::Delegate => {
                self.cycle_hub_delegate(dir);
                Action::None
            }
            HubControl::Permission => {
                self.cycle_hub_permission(dir);
                Action::None
            }
            HubControl::Trust => {
                self.toggle_hub_trust();
                Action::None
            }
            HubControl::Target => {
                let items = self.machine_items();
                self.open_picker("machines", items);
                Action::None
            }
            HubControl::Skills => {
                if let Some(welcome) = self.ui.welcome.as_mut() {
                    welcome.mode = WelcomeMode::Skills;
                    welcome.list_selected = 0;
                    welcome.draft.clear();
                    welcome.error = None;
                }
                self.reload_welcome_skills();
                Action::None
            }
            HubControl::Memory => {
                if let Some(welcome) = self.ui.welcome.as_mut() {
                    welcome.mode = WelcomeMode::Memory;
                    welcome.list_selected = 0;
                    welcome.draft.clear();
                    welcome.error = None;
                }
                self.reload_welcome_notes();
                Action::None
            }
            HubControl::Mcp => {
                self.open_hub_mcp();
                Action::None
            }
            HubControl::Theme => {
                let len = THEMES.len() as isize;
                let next = (self.ui.theme as isize + dir).rem_euclid(len) as usize;
                self.ui.theme = next;
                self.save_ui_prefs();
                self.set_toast(format!("theme: {}", THEMES[next].0), false);
                Action::None
            }
            HubControl::Sidebar => {
                self.ui.show_sidebar = !self.ui.show_sidebar;
                self.ui.inspector.force_open = self.ui.show_sidebar;
                self.save_ui_prefs();
                Action::None
            }
        }
    }

    pub(in crate::tui) fn cycle_effort(&mut self, dir: isize) {
        let mut options = self.inputs.catalog.effort_options.clone();
        if options.is_empty() {
            options = vec![
                "off".into(),
                "low".into(),
                "medium".into(),
                "high".into(),
                "max".into(),
            ];
        }
        let current = effort::load(&self.data_dir);
        let idx = options.iter().position(|o| o == &current).unwrap_or(0);
        let next_idx = (idx as isize + dir).rem_euclid(options.len() as isize) as usize;
        let next = &options[next_idx];
        match effort::set(&self.data_dir, next) {
            Ok(()) => {
                self.inputs.invalidate_catalog();
                self.set_toast(format!("effort: {next}"), false);
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(err);
                } else {
                    self.set_toast(err, true);
                }
            }
        }
    }

    fn cycle_hub_budget(&mut self, dir: isize) {
        let current = self.inputs.catalog.tier.as_str();
        let idx = BUDGET_TIERS
            .iter()
            .position(|(name, _)| *name == current)
            .unwrap_or(2);
        let next_idx = (idx as isize + dir).rem_euclid(BUDGET_TIERS.len() as isize) as usize;
        let (name, tier) = BUDGET_TIERS[next_idx];
        let model = resolve_active_provider(&self.data_dir).map(|(_, m)| m);
        match jade_agent::agent::context::set_budget_tier(&self.data_dir, tier, model.as_ref()) {
            Ok(()) => {
                self.inputs.invalidate_catalog();
                self.set_toast(format!("budget: {name}"), false);
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(format!("{err:#}"));
                }
            }
        }
    }

    fn cycle_hub_delegate(&mut self, dir: isize) {
        let current = delegate_pref::load(&self.data_dir);
        let idx = DELEGATE_LEVELS
            .iter()
            .position(|lvl| *lvl == current)
            .unwrap_or(1);
        let next_idx = (idx as isize + dir).rem_euclid(DELEGATE_LEVELS.len() as isize) as usize;
        let next = DELEGATE_LEVELS[next_idx];
        match delegate_pref::set(&self.data_dir, next) {
            Ok(()) => {
                self.inputs.invalidate_catalog();
                self.set_toast(format!("delegate: {}", next.as_str()), false);
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(err);
                }
            }
        }
    }

    fn cycle_hub_permission(&mut self, dir: isize) {
        let idx = PERM_MODES
            .iter()
            .position(|m| *m == self.session.perm_mode)
            .unwrap_or(0);
        let next_idx = (idx as isize + dir).rem_euclid(PERM_MODES.len() as isize) as usize;
        self.set_permission_mode(PERM_MODES[next_idx]);
    }

    fn toggle_hub_trust(&mut self) {
        let cwd = self.selected_hub_cwd();
        let next = !is_workspace_trusted(&self.data_dir, &cwd);
        match set_workspace_trusted(&self.data_dir, &cwd, next) {
            Ok(()) => {
                self.inputs.invalidate_catalog();
                self.set_toast(
                    format!("workspace {}", if next { "trusted" } else { "untrusted" }),
                    false,
                );
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(format!("{err:#}"));
                }
            }
        }
    }
}
