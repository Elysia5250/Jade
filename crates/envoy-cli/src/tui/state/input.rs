//! Editor input, key routing, and cursor helpers.
use super::*;

impl App {

    pub(in crate::tui) fn handle_key(&mut self, key: KeyEvent) -> Action {
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

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.agent_running {
                        return Action::Cancel;
                    }
                    return Action::Quit;
                }
                KeyCode::Char('d') if self.input.is_empty() => return Action::Quit,
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
                self.focus = match self.focus {
                    Focus::Input => Focus::Transcript,
                    Focus::Transcript => {
                        if self.show_sidebar {
                            Focus::Sidebar
                        } else {
                            Focus::Input
                        }
                    }
                    Focus::Sidebar => Focus::Input,
                };
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
                        self.job_selected = self.job_selected.saturating_sub(1);
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
                        if self.job_selected + 1 < self.jobs.len() {
                            self.job_selected += 1;
                        }
                    }
                    Focus::Transcript => {
                        if !self.session.items.is_empty() {
                            let last = self.session.items.len() - 1;
                            self.selected_card = Some(
                                self.selected_card.map(|i| (i + 1).min(last)).unwrap_or(0),
                            );
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
                self.follow = false;
                self.transcript_scroll = 0;
                if !self.session.items.is_empty() {
                    self.selected_card = Some(0);
                }
                Action::None
            }
            KeyCode::Char('G') => {
                self.follow = true;
                if !self.session.items.is_empty() {
                    self.selected_card = Some(self.session.items.len() - 1);
                }
                Action::None
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.transcript_scroll = self
                    .transcript_scroll
                    .saturating_sub(self.transcript_height.max(1));
                Action::None
            }
            KeyCode::PageDown => {
                self.transcript_scroll = self
                    .transcript_scroll
                    .saturating_add(self.transcript_height.max(1));
                if self.transcript_scroll >= self.transcript_max_scroll {
                    self.follow = true;
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


    pub(in crate::tui) fn handle_input_key(&mut self, key: KeyEvent) -> Action {
        // 1. `/cd <dir>` directory completion popup
        let cd_dirs = self.cd_completions();
        if !cd_dirs.is_empty() {
            let last = cd_dirs.len() - 1;
            match key.code {
                KeyCode::Up => {
                    self.menu_selected = self.menu_selected.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.menu_selected = (self.menu_selected + 1).min(last);
                    return Action::None;
                }
                KeyCode::Tab => {
                    let chosen = &cd_dirs[self.menu_selected.min(last)].1;
                    self.input = format!("/cd {chosen}");
                    self.input_cursor = self.input.chars().count();
                    self.menu_selected = 0;
                    return Action::None;
                }
                KeyCode::Enter if key.modifiers == KeyModifiers::NONE => {
                    let chosen = cd_dirs[self.menu_selected.min(last)].1.clone();
                    self.input.clear();
                    self.input_cursor = 0;
                    self.menu_selected = 0;
                    return self.run_slash(&format!("/cd {chosen}"));
                }
                KeyCode::Esc => {
                    self.input.clear();
                    self.input_cursor = 0;
                    return Action::None;
                }
                _ => {}
            }
        }

        // 2. `/` slash command menu
        let menu = self.filtered_slash();
        if !menu.is_empty() {
            let last = menu.len() - 1;
            match key.code {
                KeyCode::Up => {
                    self.menu_selected = self.menu_selected.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.menu_selected = (self.menu_selected + 1).min(last);
                    return Action::None;
                }
                KeyCode::Tab => {
                    let cmd = menu[self.menu_selected.min(last)];
                    self.input = if cmd.args.is_empty() {
                        format!("/{}", cmd.name)
                    } else {
                        format!("/{} ", cmd.name)
                    };
                    self.input_cursor = self.input.chars().count();
                    self.menu_selected = 0;
                    return Action::None;
                }
                KeyCode::Enter if key.modifiers == KeyModifiers::NONE => {
                    let cmd = menu[self.menu_selected.min(last)];
                    if cmd.args.is_empty() || cmd.args.starts_with('[') {
                        self.input.clear();
                        self.input_cursor = 0;
                        self.menu_selected = 0;
                        return self.run_slash(&format!("/{}", cmd.name));
                    }
                    self.input = format!("/{} ", cmd.name);
                    self.input_cursor = self.input.chars().count();
                    self.menu_selected = 0;
                    return Action::None;
                }
                KeyCode::Esc => {
                    self.input.clear();
                    self.input_cursor = 0;
                    return Action::None;
                }
                _ => {}
            }
        }

        // 3. Multiline newline bindings (Shift+Enter, Alt+Enter, Ctrl+J)
        if (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT))
            || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.insert_char('\n');
            return Action::None;
        }

        match key.code {
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return Action::None;
                }
                if self.history.last().map(|h| h != &text).unwrap_or(true) {
                    self.history.push(text.clone());
                }
                self.history_pos = None;
                self.input.clear();
                self.input_cursor = 0;
                self.menu_selected = 0;

                if text.starts_with('/') {
                    return self.run_slash(&text);
                }
                if let Some(shell_cmd) = text.strip_prefix('!') {
                    let cmd = shell_cmd.trim().to_string();
                    if cmd.is_empty() {
                        return Action::None;
                    }
                    return Action::SubmitShell(SubmitRequest {
                        spec: TaskSpec::Shell { command: cmd },
                        cwd: Some(self.session.cwd.display().to_string()),
                        env: Vec::new(),
                        label: None,
                        group: DEFAULT_GROUP.to_string(),
                        timeout_s: 0,
                    });
                }
                match self.mode {
                    InputMode::Shell => Action::SubmitShell(SubmitRequest {
                        spec: TaskSpec::Shell { command: text },
                        cwd: Some(self.session.cwd.display().to_string()),
                        env: Vec::new(),
                        label: None,
                        group: DEFAULT_GROUP.to_string(),
                        timeout_s: 0,
                    }),
                    InputMode::Agent if key.modifiers.contains(KeyModifiers::ALT) => {
                        Action::SubmitAgentFollowup(text)
                    }
                    InputMode::Agent => Action::SubmitAgent(text),
                }
            }
            KeyCode::Tab => {
                // Opencode-style Tab: switch Agent <-> Shell mode
                self.mode = match self.mode {
                    InputMode::Agent => InputMode::Shell,
                    InputMode::Shell => InputMode::Agent,
                };
                self.set_toast(format!("mode: {}", self.mode.label()), false);
                Action::None
            }
            KeyCode::BackTab => {
                self.focus = Focus::Transcript;
                Action::None
            }
            KeyCode::Esc => {
                if !self.input.is_empty() {
                    self.input.clear();
                    self.input_cursor = 0;
                } else if self.agent_running {
                    return Action::Cancel;
                } else {
                    self.focus = Focus::Transcript;
                }
                Action::None
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.transcript_scroll = self
                    .transcript_scroll
                    .saturating_sub(self.transcript_height.max(1));
                Action::None
            }
            KeyCode::PageDown => {
                self.transcript_scroll = self
                    .transcript_scroll
                    .saturating_add(self.transcript_height.max(1));
                if self.transcript_scroll >= self.transcript_max_scroll {
                    self.follow = true;
                }
                Action::None
            }
            KeyCode::Backspace => {
                self.delete_before_cursor();
                Action::None
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
                Action::None
            }
            KeyCode::Left => {
                self.input_cursor = self.input_cursor.saturating_sub(1);
                Action::None
            }
            KeyCode::Right => {
                let len = self.input.chars().count();
                if self.input_cursor < len {
                    self.input_cursor += 1;
                }
                Action::None
            }
            KeyCode::Home => {
                self.input_cursor = self.line_start_char(self.input_cursor);
                Action::None
            }
            KeyCode::End => {
                self.input_cursor = self.line_end_char(self.input_cursor);
                Action::None
            }
            KeyCode::Up => {
                if self.move_cursor_vertical(-1) {
                    return Action::None;
                }
                if !self.history.is_empty() {
                    let pos = match self.history_pos {
                        Some(p) => p.saturating_sub(1),
                        None => self.history.len() - 1,
                    };
                    self.history_pos = Some(pos);
                    self.input = self.history[pos].clone();
                    self.input_cursor = self.input.chars().count();
                }
                Action::None
            }
            KeyCode::Down => {
                if self.move_cursor_vertical(1) {
                    return Action::None;
                }
                if let Some(pos) = self.history_pos {
                    if pos + 1 < self.history.len() {
                        self.history_pos = Some(pos + 1);
                        self.input = self.history[pos + 1].clone();
                    } else {
                        self.history_pos = None;
                        self.input.clear();
                    }
                    self.input_cursor = self.input.chars().count();
                }
                Action::None
            }
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match c {
                    'u' => {
                        self.input.clear();
                        self.input_cursor = 0;
                    }
                    'a' => self.input_cursor = self.line_start_char(self.input_cursor),
                    'e' => self.input_cursor = self.line_end_char(self.input_cursor),
                    'k' => {
                        let byte = self.byte_index(self.input_cursor);
                        self.input.truncate(byte);
                    }
                    _ => {}
                }
                Action::None
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                Action::None
            }
            _ => Action::None,
        }
    }


    pub(in crate::tui) fn insert_char(&mut self, c: char) {
        let at = self.byte_index(self.input_cursor);
        self.input.insert(at, c);
        self.input_cursor += 1;
        self.menu_selected = 0;
    }


    pub(in crate::tui) fn delete_before_cursor(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let at = self.byte_index(self.input_cursor - 1);
        self.input.remove(at);
        self.input_cursor -= 1;
        self.menu_selected = 0;
    }


    pub(in crate::tui) fn delete_at_cursor(&mut self) {
        if self.input_cursor >= self.input.chars().count() {
            return;
        }
        let at = self.byte_index(self.input_cursor);
        self.input.remove(at);
        self.menu_selected = 0;
    }


    pub(in crate::tui) fn byte_index(&self, chars: usize) -> usize {
        self.input
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }


    pub(in crate::tui) fn line_start_char(&self, cursor: usize) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = cursor.min(chars.len());
        while i > 0 && chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }


    pub(in crate::tui) fn line_end_char(&self, cursor: usize) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = cursor.min(chars.len());
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        i
    }


    pub(in crate::tui) fn move_cursor_vertical(&mut self, delta: isize) -> bool {
        let chars: Vec<char> = self.input.chars().collect();
        let cur_start = self.line_start_char(self.input_cursor);
        let col = self.input_cursor - cur_start;
        if delta < 0 {
            if cur_start == 0 {
                return false;
            }
            let prev_end = cur_start - 1;
            let prev_start = self.line_start_char(prev_end);
            let prev_len = prev_end - prev_start;
            self.input_cursor = prev_start + col.min(prev_len);
            true
        } else {
            let cur_end = self.line_end_char(self.input_cursor);
            if cur_end >= chars.len() {
                return false;
            }
            let next_start = cur_end + 1;
            let next_end = self.line_end_char(next_start);
            let next_len = next_end - next_start;
            self.input_cursor = next_start + col.min(next_len);
            true
        }
    }

}
