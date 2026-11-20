//! Editor input, key routing, and cursor helpers.
use super::*;

impl App {
    pub(in crate::tui) fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.ui.welcome.is_some()
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return Action::Quit;
        }
        if self.ui.modal.is_some() {
            return self.handle_modal_key(key);
        }
        if self.ui.welcome.is_some() {
            return self.handle_welcome_key(key);
        }
        if self.agent.pending_ask.is_some() {
            return self.handle_ask_key(key);
        }
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Up {
            if let Some(last) = self.agent.queued.pop_back() {
                // A queued item can be in two places: the local outbox while
                // offline, or the daemon's follow-up queue. Remove it from
                // both before putting it back in the editor.
                self.remove_outbox(&last);
                self.recall_queued();
                self.ui.input = last;
                self.ui.input_cursor = self.ui.input.chars().count();
                self.ui.focus = Focus::Input;
            } else {
                // The console may have restarted while the daemon still had
                // follow-ups queued; ask it for the last one.
                self.recall_queued();
            }
            return Action::None;
        }

        // Leader prefix (`ctrl+x`)
        if let Some(at) = self.ui.leader_at.take() {
            if at.elapsed() <= LEADER_TIMEOUT {
                if let KeyCode::Char(c) = key.code {
                    let cmd = match c {
                        'q' => Some(Command::Quit),
                        'n' => Some(Command::NewSession),
                        'c' => Some(Command::Cancel),
                        'b' => Some(Command::ToggleSidebar),
                        't' => Some(Command::NextTheme),
                        's' => Some(Command::Refresh),
                        'l' => Some(Command::OpenLogin),
                        'm' => Some(Command::OpenModels),
                        _ => None,
                    };
                    if let Some(cmd) = cmd {
                        return self.run_command(cmd);
                    }
                }
                return Action::None;
            }
        }

        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char('1') => return self.focus_inspector_tab(InspectorTab::Overview),
                KeyCode::Char('2') => return self.focus_inspector_tab(InspectorTab::Diff),
                KeyCode::Char('3') => return self.focus_inspector_tab(InspectorTab::File),
                KeyCode::Char('4') => return self.focus_inspector_tab(InspectorTab::Output),
                KeyCode::Char('5') => return self.focus_inspector_tab(InspectorTab::Inspect),
                KeyCode::Char('m') => return Action::ToggleMouseCapture,
                _ => {}
            }
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.agent.running {
                        return Action::Cancel;
                    }
                    return Action::Quit;
                }
                KeyCode::Char('d') if self.ui.input.is_empty() => return Action::Quit,
                KeyCode::Char('b') => return self.run_command(Command::ToggleSidebar),
                KeyCode::Char('p') => {
                    let items = self.command_items();
                    self.open_picker("commands", items);
                    return Action::None;
                }
                KeyCode::Char('x') => {
                    self.ui.leader_at = Some(Instant::now());
                    return Action::None;
                }
                KeyCode::Char('o') => {
                    self.toggle_expand_cards();
                    return Action::None;
                }
                KeyCode::Char('u') if self.ui.focus != Focus::Input => {
                    self.ui.follow = false;
                    self.ui.transcript_scroll = self
                        .ui.transcript_scroll
                        .saturating_sub((self.ui.transcript_height as usize / 2).max(1));
                    return Action::None;
                }
                KeyCode::Char('f') if self.ui.focus != Focus::Input => {
                    self.ui.follow = false;
                    self.ui.transcript_scroll = self
                        .ui.transcript_scroll
                        .saturating_add((self.ui.transcript_height as usize / 2).max(1));
                    return Action::None;
                }
                _ => {}
            }
        }

        if self.ui.focus == Focus::Input {
            return self.handle_input_key(key);
        }

        if self.ui.focus == Focus::Sidebar {
            match key.code {
                KeyCode::Char('1') => return self.focus_inspector_tab(InspectorTab::Overview),
                KeyCode::Char('2') => return self.focus_inspector_tab(InspectorTab::Diff),
                KeyCode::Char('3') => return self.focus_inspector_tab(InspectorTab::File),
                KeyCode::Char('4') => return self.focus_inspector_tab(InspectorTab::Output),
                KeyCode::Char('5') => return self.focus_inspector_tab(InspectorTab::Inspect),
                KeyCode::Char('v') => {
                    self.ui.inspector.diff_layout = match self.ui.inspector.diff_layout {
                        diff::DiffLayout::Unified => diff::DiffLayout::Split,
                        diff::DiffLayout::Split => diff::DiffLayout::Unified,
                    };
                    return Action::None;
                }
                KeyCode::Char('n') => {
                    self.jump_diff_hunk(1);
                    return Action::None;
                }
                KeyCode::Char('p') => {
                    self.jump_diff_hunk(-1);
                    return Action::None;
                }
                KeyCode::Char('f') if self.ui.inspector.tab == InspectorTab::Diff => {
                    self.ui.inspector.tab = InspectorTab::File;
                    self.ui.inspector.scroll = 0;
                    return Action::None;
                }
                KeyCode::Char('d') if self.ui.inspector.tab == InspectorTab::File => {
                    self.ui.inspector.tab = InspectorTab::Diff;
                    self.ui.inspector.scroll = 0;
                    return Action::None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('/') => {
                self.ui.focus = Focus::Input;
                self.ui.input = "/".into();
                self.ui.input_cursor = 1;
                self.ui.menu_selected = 0;
                Action::None
            }
            KeyCode::Char('i') | KeyCode::Char('n') => {
                self.ui.focus = Focus::Input;
                Action::None
            }
            KeyCode::BackTab => {
                self.set_permission_mode(self.session.perm_mode.cycle());
                Action::None
            }
            KeyCode::Esc => {
                if self.agent.running {
                    return Action::Cancel;
                }
                self.ui.focus = Focus::Input;
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                match self.ui.focus {
                    Focus::Sidebar => {
                        if self.ui.inspector.tab == InspectorTab::Overview && !self.jobs.list.is_empty() {
                            self.jobs.selected = self.jobs.selected.saturating_sub(1);
                        } else {
                            self.ui.inspector.scroll = self.ui.inspector.scroll.saturating_sub(2);
                        }
                    }
                    Focus::Transcript => {
                        if let Some(idx) = self.ui.selected_card {
                            self.ui.selected_card = Some(idx.saturating_sub(1));
                        } else if !self.session.current.items.is_empty() {
                            self.ui.selected_card = Some(self.session.current.items.len() - 1);
                        }
                        self.ui.follow = false;
                        self.ui.transcript_scroll = self.ui.transcript_scroll.saturating_sub(2);
                    }
                    Focus::Input => {}
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.ui.focus {
                    Focus::Sidebar => {
                        if self.ui.inspector.tab == InspectorTab::Overview && !self.jobs.list.is_empty() {
                            if self.jobs.selected + 1 < self.jobs.list.len() {
                                self.jobs.selected += 1;
                            }
                        } else {
                            self.ui.inspector.scroll = self
                                .ui.inspector
                                .scroll
                                .saturating_add(2)
                                .min(self.ui.inspector.max_scroll);
                        }
                    }
                    Focus::Transcript => {
                        if !self.session.current.items.is_empty() {
                            let last = self.session.current.items.len() - 1;
                            self.ui.selected_card =
                                Some(self.ui.selected_card.map(|i| (i + 1).min(last)).unwrap_or(0));
                        }
                        self.ui.transcript_scroll = self.ui.transcript_scroll.saturating_add(2);
                    }
                    Focus::Input => {}
                }
                Action::None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if self.ui.focus == Focus::Transcript {
                    self.toggle_expand_cards();
                    return Action::None;
                }
                if self.ui.focus == Focus::Sidebar {
                    if let Some(job) = self.jobs.list.get(self.jobs.selected).cloned() {
                        return Action::AttachJob(job);
                    }
                }
                Action::None
            }
            KeyCode::Char('c') if self.ui.focus == Focus::Sidebar => {
                if let Some(job) = self.jobs.list.get(self.jobs.selected) {
                    return Action::CancelJob(job.id);
                }
                Action::None
            }
            KeyCode::Char('g') => {
                if self.ui.focus == Focus::Sidebar {
                    self.ui.inspector.scroll = 0;
                    return Action::None;
                }
                self.ui.follow = false;
                self.ui.transcript_scroll = 0;
                if !self.session.current.items.is_empty() {
                    self.ui.selected_card = Some(0);
                }
                Action::None
            }
            KeyCode::Char('G') => {
                if self.ui.focus == Focus::Sidebar {
                    self.ui.inspector.scroll = self.ui.inspector.max_scroll;
                    return Action::None;
                }
                self.ui.follow = true;
                if !self.session.current.items.is_empty() {
                    self.ui.selected_card = Some(self.session.current.items.len() - 1);
                }
                Action::None
            }
            KeyCode::PageUp => {
                if self.ui.focus == Focus::Sidebar {
                    self.ui.inspector.scroll = self
                        .ui.inspector
                        .scroll
                        .saturating_sub(self.ui.inspector.height.max(1) as usize);
                } else {
                    self.ui.follow = false;
                    self.ui.transcript_scroll = self
                        .ui.transcript_scroll
                        .saturating_sub(self.ui.transcript_height.max(1) as usize);
                }
                Action::None
            }
            KeyCode::PageDown => {
                if self.ui.focus == Focus::Sidebar {
                    self.ui.inspector.scroll = self
                        .ui.inspector
                        .scroll
                        .saturating_add(self.ui.inspector.height.max(1) as usize)
                        .min(self.ui.inspector.max_scroll);
                } else {
                    self.ui.transcript_scroll = self
                        .ui.transcript_scroll
                        .saturating_add(self.ui.transcript_height.max(1) as usize);
                    if self.ui.transcript_scroll >= self.ui.transcript_max_scroll {
                        self.ui.follow = true;
                    }
                }
                Action::None
            }
            KeyCode::Home => {
                self.ui.follow = false;
                self.ui.transcript_scroll = 0;
                Action::None
            }
            KeyCode::End => {
                self.ui.follow = true;
                Action::None
            }
            KeyCode::Char('b') => self.run_command(Command::ToggleSidebar),
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Char('q') => Action::Quit,
            _ => Action::None,
        }
    }

    pub(in crate::tui) fn focus_inspector_tab(&mut self, tab: InspectorTab) -> Action {
        self.ui.show_sidebar = true;
        self.ui.inspector.force_open = true;
        self.ui.inspector.tab = tab;
        self.ui.inspector.scroll = 0;
        if tab == InspectorTab::Diff && self.ui.inspector.file_path.is_none() {
            if let Some(last) = self.session.current.changed_files.last() {
                self.ui.inspector.selected_diff = self.session.current.changed_files.len() - 1;
                self.ui.inspector.file_path = Some(last.path.clone());
            }
        }
        Action::None
    }

    pub(in crate::tui) fn jump_diff_hunk(&mut self, dir: isize) {
        let cur = self.ui.inspector.scroll;
        if self.ui.inspector.diff_hunk_lines.is_empty() {
            return;
        }
        let next = if dir > 0 {
            self.ui.inspector
                .diff_hunk_lines
                .iter()
                .copied()
                .find(|&l| l > cur)
                .unwrap_or(*self.ui.inspector.diff_hunk_lines.first().unwrap())
        } else {
            self.ui.inspector
                .diff_hunk_lines
                .iter()
                .rev()
                .copied()
                .find(|&l| l < cur)
                .unwrap_or(*self.ui.inspector.diff_hunk_lines.last().unwrap())
        };
        self.ui.inspector.scroll = next.min(self.ui.inspector.max_scroll);
    }
}
