//! Editor input, key routing, and cursor helpers.
use super::*;

impl App {
    pub(in crate::tui) fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.welcome.is_some() {
            return self.handle_welcome_key(key);
        }
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }
        if self.pending_ask.is_some() {
            return self.handle_ask_key(key);
        }
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Up {
            if let Some(last) = self.queued.pop_back() {
                // A queued item can be in two places: the local outbox while
                // offline, or the daemon's follow-up queue. Remove it from
                // both before putting it back in the editor.
                self.remove_outbox(&last);
                self.recall_queued();
                self.input = last;
                self.input_cursor = self.input.chars().count();
                self.focus = Focus::Input;
            } else {
                // The console may have restarted while the daemon still had
                // follow-ups queued; ask it for the last one.
                self.recall_queued();
            }
            return Action::None;
        }

        // Leader prefix (`ctrl+x`)
        if let Some(at) = self.leader_at.take() {
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
                    if self.agent_running {
                        return Action::Cancel;
                    }
                    return Action::Quit;
                }
                KeyCode::Char('d') if self.input.is_empty() => return Action::Quit,
                KeyCode::Char('b') => return self.run_command(Command::ToggleSidebar),
                KeyCode::Char('p') => {
                    let items = self.command_items();
                    self.open_picker("commands", items);
                    return Action::None;
                }
                KeyCode::Char('x') => {
                    self.leader_at = Some(Instant::now());
                    return Action::None;
                }
                KeyCode::Char('o') => {
                    self.toggle_expand_cards();
                    return Action::None;
                }
                KeyCode::Char('u') => {
                    self.follow = false;
                    self.transcript_scroll = self
                        .transcript_scroll
                        .saturating_sub((self.transcript_height / 2).max(1));
                    return Action::None;
                }
                KeyCode::Char('f') => {
                    self.follow = false;
                    self.transcript_scroll = self
                        .transcript_scroll
                        .saturating_add((self.transcript_height / 2).max(1));
                    return Action::None;
                }
                _ => {}
            }
        }

        if self.focus == Focus::Input {
            return self.handle_input_key(key);
        }

        if self.focus == Focus::Sidebar {
            match key.code {
                KeyCode::Char('1') => return self.focus_inspector_tab(InspectorTab::Overview),
                KeyCode::Char('2') => return self.focus_inspector_tab(InspectorTab::Diff),
                KeyCode::Char('3') => return self.focus_inspector_tab(InspectorTab::File),
                KeyCode::Char('4') => return self.focus_inspector_tab(InspectorTab::Output),
                KeyCode::Char('5') => return self.focus_inspector_tab(InspectorTab::Inspect),
                KeyCode::Char('v') => {
                    self.inspector.diff_layout = match self.inspector.diff_layout {
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
                KeyCode::Char('f') if self.inspector.tab == InspectorTab::Diff => {
                    self.inspector.tab = InspectorTab::File;
                    self.inspector.scroll = 0;
                    return Action::None;
                }
                KeyCode::Char('d') if self.inspector.tab == InspectorTab::File => {
                    self.inspector.tab = InspectorTab::Diff;
                    self.inspector.scroll = 0;
                    return Action::None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('/') => {
                self.focus = Focus::Input;
                self.input = "/".into();
                self.input_cursor = 1;
                self.menu_selected = 0;
                Action::None
            }
            KeyCode::Char('i') | KeyCode::Char('n') => {
                self.focus = Focus::Input;
                Action::None
            }
            KeyCode::BackTab => {
                self.set_permission_mode(self.perm_mode.cycle());
                Action::None
            }
            KeyCode::Esc => {
                if self.agent_running {
                    return Action::Cancel;
                }
                self.focus = Focus::Input;
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                match self.focus {
                    Focus::Sidebar => {
                        if self.inspector.tab == InspectorTab::Overview && !self.jobs.is_empty() {
                            self.job_selected = self.job_selected.saturating_sub(1);
                        } else {
                            self.inspector.scroll = self.inspector.scroll.saturating_sub(2);
                        }
                    }
                    Focus::Transcript => {
                        if let Some(idx) = self.selected_card {
                            self.selected_card = Some(idx.saturating_sub(1));
                        } else if !self.session.items.is_empty() {
                            self.selected_card = Some(self.session.items.len() - 1);
                        }
                        self.follow = false;
                        self.transcript_scroll = self.transcript_scroll.saturating_sub(2);
                    }
                    Focus::Input => {}
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.focus {
                    Focus::Sidebar => {
                        if self.inspector.tab == InspectorTab::Overview && !self.jobs.is_empty() {
                            if self.job_selected + 1 < self.jobs.len() {
                                self.job_selected += 1;
                            }
                        } else {
                            self.inspector.scroll = self
                                .inspector
                                .scroll
                                .saturating_add(2)
                                .min(self.inspector.max_scroll);
                        }
                    }
                    Focus::Transcript => {
                        if !self.session.items.is_empty() {
                            let last = self.session.items.len() - 1;
                            self.selected_card =
                                Some(self.selected_card.map(|i| (i + 1).min(last)).unwrap_or(0));
                        }
                        self.transcript_scroll = self.transcript_scroll.saturating_add(2);
                    }
                    Focus::Input => {}
                }
                Action::None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if self.focus == Focus::Transcript {
                    self.toggle_expand_cards();
                    return Action::None;
                }
                if self.focus == Focus::Sidebar {
                    if let Some(job) = self.jobs.get(self.job_selected).cloned() {
                        return Action::AttachJob(job);
                    }
                }
                Action::None
            }
            KeyCode::Char('c') if self.focus == Focus::Sidebar => {
                if let Some(job) = self.jobs.get(self.job_selected) {
                    return Action::CancelJob(job.id);
                }
                Action::None
            }
            KeyCode::Char('g') => {
                if self.focus == Focus::Sidebar {
                    self.inspector.scroll = 0;
                    return Action::None;
                }
                self.follow = false;
                self.transcript_scroll = 0;
                if !self.session.items.is_empty() {
                    self.selected_card = Some(0);
                }
                Action::None
            }
            KeyCode::Char('G') => {
                if self.focus == Focus::Sidebar {
                    self.inspector.scroll = self.inspector.max_scroll;
                    return Action::None;
                }
                self.follow = true;
                if !self.session.items.is_empty() {
                    self.selected_card = Some(self.session.items.len() - 1);
                }
                Action::None
            }
            KeyCode::PageUp => {
                if self.focus == Focus::Sidebar {
                    self.inspector.scroll = self
                        .inspector
                        .scroll
                        .saturating_sub(self.inspector.height.max(1));
                } else {
                    self.follow = false;
                    self.transcript_scroll = self
                        .transcript_scroll
                        .saturating_sub(self.transcript_height.max(1));
                }
                Action::None
            }
            KeyCode::PageDown => {
                if self.focus == Focus::Sidebar {
                    self.inspector.scroll = self
                        .inspector
                        .scroll
                        .saturating_add(self.inspector.height.max(1))
                        .min(self.inspector.max_scroll);
                } else {
                    self.transcript_scroll = self
                        .transcript_scroll
                        .saturating_add(self.transcript_height.max(1));
                    if self.transcript_scroll >= self.transcript_max_scroll {
                        self.follow = true;
                    }
                }
                Action::None
            }
            KeyCode::Home => {
                self.follow = false;
                self.transcript_scroll = 0;
                Action::None
            }
            KeyCode::End => {
                self.follow = true;
                Action::None
            }
            KeyCode::Char('b') => self.run_command(Command::ToggleSidebar),
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Char('q') => Action::Quit,
            _ => Action::None,
        }
    }

    pub(in crate::tui) fn focus_inspector_tab(&mut self, tab: InspectorTab) -> Action {
        self.show_sidebar = true;
        self.inspector.force_open = true;
        self.inspector.tab = tab;
        self.inspector.scroll = 0;
        if tab == InspectorTab::Diff && self.inspector.file_path.is_none() {
            if let Some(last) = self.session.changed_files.last() {
                self.inspector.selected_diff = self.session.changed_files.len() - 1;
                self.inspector.file_path = Some(last.path.clone());
            }
        }
        Action::None
    }

    pub(in crate::tui) fn jump_diff_hunk(&mut self, dir: isize) {
        let cur = self.inspector.scroll as usize;
        if self.inspector.diff_hunk_lines.is_empty() {
            return;
        }
        let next = if dir > 0 {
            self.inspector
                .diff_hunk_lines
                .iter()
                .copied()
                .find(|&l| l > cur)
                .unwrap_or(*self.inspector.diff_hunk_lines.first().unwrap())
        } else {
            self.inspector
                .diff_hunk_lines
                .iter()
                .rev()
                .copied()
                .find(|&l| l < cur)
                .unwrap_or(*self.inspector.diff_hunk_lines.last().unwrap())
        };
        self.inspector.scroll = (next as u16).min(self.inspector.max_scroll);
    }
}
