//! Composer key handling and cursor movement.
use super::*;

impl App {
    pub(in crate::tui) fn handle_input_key(&mut self, key: KeyEvent) -> Action {
        // 1. `/cd <dir>` directory completion popup
        let cd_dirs = self.cd_completions();
        if !cd_dirs.is_empty() {
            let last = cd_dirs.len() - 1;
            match key.code {
                KeyCode::Up => {
                    self.ui.menu_selected = self.ui.menu_selected.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.ui.menu_selected = (self.ui.menu_selected + 1).min(last);
                    return Action::None;
                }
                KeyCode::Tab => {
                    let chosen = &cd_dirs[self.ui.menu_selected.min(last)].1;
                    self.ui.input = format!("/cd {chosen}");
                    self.ui.input_cursor = self.ui.input.chars().count();
                    self.ui.menu_selected = 0;
                    return Action::None;
                }
                KeyCode::Enter if key.modifiers == KeyModifiers::NONE => {
                    let chosen = cd_dirs[self.ui.menu_selected.min(last)].1.clone();
                    self.ui.input.clear();
                    self.ui.input_cursor = 0;
                    self.ui.menu_selected = 0;
                    return self.run_slash(&format!("/cd {chosen}"));
                }
                KeyCode::Esc => {
                    self.ui.input.clear();
                    self.ui.input_cursor = 0;
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
                    self.ui.menu_selected = self.ui.menu_selected.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.ui.menu_selected = (self.ui.menu_selected + 1).min(last);
                    return Action::None;
                }
                KeyCode::Tab => {
                    let cmd = menu[self.ui.menu_selected.min(last)];
                    self.ui.input = if cmd.args.is_empty() {
                        format!("/{}", cmd.name)
                    } else {
                        format!("/{} ", cmd.name)
                    };
                    self.ui.input_cursor = self.ui.input.chars().count();
                    self.ui.menu_selected = 0;
                    return Action::None;
                }
                KeyCode::Enter if key.modifiers == KeyModifiers::NONE => {
                    let cmd = menu[self.ui.menu_selected.min(last)];
                    if cmd.args.is_empty() || cmd.args.starts_with('[') {
                        self.ui.input.clear();
                        self.ui.input_cursor = 0;
                        self.ui.menu_selected = 0;
                        return self.run_slash(&format!("/{}", cmd.name));
                    }
                    self.ui.input = format!("/{} ", cmd.name);
                    self.ui.input_cursor = self.ui.input.chars().count();
                    self.ui.menu_selected = 0;
                    return Action::None;
                }
                KeyCode::Esc => {
                    self.ui.input.clear();
                    self.ui.input_cursor = 0;
                    return Action::None;
                }
                _ => {}
            }
        }

        // 3. `@file` workspace completion popup
        let at_files = self.file_completions();
        if !at_files.is_empty() {
            let last = at_files.len() - 1;
            match key.code {
                KeyCode::Up => {
                    self.ui.menu_selected = self.ui.menu_selected.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.ui.menu_selected = (self.ui.menu_selected + 1).min(last);
                    return Action::None;
                }
                KeyCode::Tab | KeyCode::Enter if key.modifiers == KeyModifiers::NONE => {
                    let chosen = at_files[self.ui.menu_selected.min(last)].clone();
                    self.accept_at_completion(&chosen);
                    return Action::None;
                }
                KeyCode::Esc => {
                    if let Some((start_ch, _, _)) = self.at_mention_span() {
                        self.ui.at_dismissed_at = Some(start_ch);
                    }
                    self.ui.menu_selected = 0;
                    return Action::None;
                }
                _ => {}
            }
        }

        // 4. Multiline newline bindings (Shift+Enter, Ctrl+J)
        if (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT))
            || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.insert_char('\n');
            return Action::None;
        }

        match key.code {
            KeyCode::Enter => {
                let text = self.ui.input.trim().to_string();
                if text.is_empty() {
                    return Action::None;
                }
                if self.ui.history.last().map(|h| h != &text).unwrap_or(true) {
                    self.ui.history.push(text.clone());
                }
                self.ui.history_pos = None;
                self.ui.input.clear();
                self.ui.input_cursor = 0;
                self.ui.menu_selected = 0;
                self.ui.at_dismissed_at = None;

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
                        cwd: Some(self.session.current.cwd.display().to_string()),
                        env: Vec::new(),
                        label: None,
                        group: DEFAULT_GROUP.to_string(),
                        timeout_s: 0,
                        sandbox: envoy_proto::SandboxPolicy::Off,
                    });
                }
                match self.ui.mode {
                    InputMode::Shell => Action::SubmitShell(SubmitRequest {
                        spec: TaskSpec::Shell { command: text },
                        cwd: Some(self.session.current.cwd.display().to_string()),
                        env: Vec::new(),
                        label: None,
                        group: DEFAULT_GROUP.to_string(),
                        timeout_s: 0,
                        sandbox: envoy_proto::SandboxPolicy::Off,
                    }),
                    InputMode::Agent if key.modifiers.contains(KeyModifiers::ALT) => {
                        Action::SubmitAgentFollowup(text)
                    }
                    InputMode::Agent => Action::SubmitAgent(text),
                }
            }
            KeyCode::Tab => {
                // Opencode-style Tab: switch Agent <-> Shell mode
                self.ui.mode = match self.ui.mode {
                    InputMode::Agent => InputMode::Shell,
                    InputMode::Shell => InputMode::Agent,
                };
                self.set_toast(format!("mode: {}", self.ui.mode.label()), false);
                Action::None
            }
            KeyCode::BackTab => {
                self.set_permission_mode(self.session.perm_mode.cycle());
                Action::None
            }
            KeyCode::Esc => {
                if !self.ui.input.is_empty() {
                    self.ui.input.clear();
                    self.ui.input_cursor = 0;
                } else if self.agent.running {
                    return Action::Cancel;
                } else {
                    self.ui.focus = Focus::Transcript;
                }
                Action::None
            }
            KeyCode::PageUp => {
                self.ui.follow = false;
                self.ui.transcript_scroll = self
                    .ui.transcript_scroll
                    .saturating_sub(self.ui.transcript_height.max(1) as usize);
                Action::None
            }
            KeyCode::PageDown => {
                self.ui.transcript_scroll = self
                    .ui.transcript_scroll
                    .saturating_add(self.ui.transcript_height.max(1) as usize);
                if self.ui.transcript_scroll >= self.ui.transcript_max_scroll {
                    self.ui.follow = true;
                }
                Action::None
            }
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                self.delete_word_before_cursor();
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
            KeyCode::Left
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.move_cursor_word(-1);
                Action::None
            }
            KeyCode::Right
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.move_cursor_word(1);
                Action::None
            }
            KeyCode::Left => {
                self.ui.input_cursor = self.ui.input_cursor.saturating_sub(1);
                Action::None
            }
            KeyCode::Right => {
                let len = self.ui.input.chars().count();
                if self.ui.input_cursor < len {
                    self.ui.input_cursor += 1;
                }
                Action::None
            }
            KeyCode::Home => {
                self.ui.input_cursor = self.line_start_char(self.ui.input_cursor);
                Action::None
            }
            KeyCode::End => {
                self.ui.input_cursor = self.line_end_char(self.ui.input_cursor);
                Action::None
            }
            KeyCode::Up => {
                if self.move_cursor_vertical(-1) {
                    return Action::None;
                }
                if !self.ui.history.is_empty() {
                    let pos = match self.ui.history_pos {
                        Some(p) => p.saturating_sub(1),
                        None => self.ui.history.len() - 1,
                    };
                    self.ui.history_pos = Some(pos);
                    self.ui.input = self.ui.history[pos].clone();
                    self.ui.input_cursor = self.ui.input.chars().count();
                }
                Action::None
            }
            KeyCode::Down => {
                if self.move_cursor_vertical(1) {
                    return Action::None;
                }
                if let Some(pos) = self.ui.history_pos {
                    if pos + 1 < self.ui.history.len() {
                        self.ui.history_pos = Some(pos + 1);
                        self.ui.input = self.ui.history[pos + 1].clone();
                    } else {
                        self.ui.history_pos = None;
                        self.ui.input.clear();
                    }
                    self.ui.input_cursor = self.ui.input.chars().count();
                }
                Action::None
            }
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match c {
                    'u' => {
                        self.ui.input.clear();
                        self.ui.input_cursor = 0;
                        self.ui.at_dismissed_at = None;
                    }
                    'a' => self.ui.input_cursor = self.line_start_char(self.ui.input_cursor),
                    'e' => self.ui.input_cursor = self.line_end_char(self.ui.input_cursor),
                    'k' => {
                        let byte = self.byte_index(self.ui.input_cursor);
                        self.ui.input.truncate(byte);
                        self.ui.at_dismissed_at = None;
                    }
                    'w' => self.delete_word_before_cursor(),
                    _ => {}
                }
                Action::None
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.move_cursor_word(-1);
                Action::None
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.move_cursor_word(1);
                Action::None
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert_char(c);
                Action::None
            }
            _ => Action::None,
        }
    }

    pub(in crate::tui) fn handle_paste(&mut self, text: &str) {
        if let Some(modal) = self.ui.modal.take() {
            self.handle_modal_paste(modal, text);
            return;
        }
        if self.ui.welcome.is_some() {
            self.handle_welcome_paste(text);
            return;
        }
        if self.agent.pending_ask.is_some() {
            return;
        }
        self.ui.focus = Focus::Input;
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return;
        }
        let at = self.byte_index(self.ui.input_cursor);
        self.ui.input.insert_str(at, &normalized);
        self.ui.input_cursor += normalized.chars().count();
        self.ui.menu_selected = 0;
        self.ui.at_dismissed_at = None;
        if self.at_mention_span().is_some() {
            self.session.workspace_index.ensure_fresh(&self.session.current.cwd);
        }
    }

    pub(in crate::tui) fn at_mention_span(&self) -> Option<(usize, usize, String)> {
        if self.slash_query().is_some() || self.cd_query().is_some() {
            return None;
        }
        let chars: Vec<char> = self.ui.input.chars().collect();
        let cursor = self.ui.input_cursor.min(chars.len());
        let mut i = cursor;
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        if i < cursor && chars[i] == '@' && (i == 0 || chars[i - 1].is_whitespace()) {
            let query: String = chars[i + 1..cursor].iter().collect();
            Some((i, cursor, query))
        } else {
            None
        }
    }

    pub(in crate::tui) fn file_completions(&self) -> Vec<String> {
        let Some((start_ch, _, query)) = self.at_mention_span() else {
            return Vec::new();
        };
        if self.ui.at_dismissed_at == Some(start_ch) {
            return Vec::new();
        }
        self.session.workspace_index.search_files(&query, 10)
    }

    pub(in crate::tui) fn accept_at_completion(&mut self, chosen: &str) {
        let Some((start_ch, end_ch, _)) = self.at_mention_span() else {
            return;
        };
        let b0 = self.byte_index(start_ch);
        let b1 = self.byte_index(end_ch);
        let replacement = format!("@{chosen} ");
        self.ui.input.replace_range(b0..b1, &replacement);
        self.ui.input_cursor = start_ch + replacement.chars().count();
        self.ui.menu_selected = 0;
        self.ui.at_dismissed_at = None;
    }

    pub(in crate::tui) fn insert_char(&mut self, c: char) {
        let at = self.byte_index(self.ui.input_cursor);
        self.ui.input.insert(at, c);
        self.ui.input_cursor += 1;
        self.ui.menu_selected = 0;
        self.ui.at_dismissed_at = None;
        if c == '@' || self.at_mention_span().is_some() {
            self.session.workspace_index.ensure_fresh(&self.session.current.cwd);
        }
    }

    pub(in crate::tui) fn delete_before_cursor(&mut self) {
        if self.ui.input_cursor == 0 {
            return;
        }
        let at = self.byte_index(self.ui.input_cursor - 1);
        self.ui.input.remove(at);
        self.ui.input_cursor -= 1;
        self.ui.menu_selected = 0;
        self.ui.at_dismissed_at = None;
    }

    pub(in crate::tui) fn delete_at_cursor(&mut self) {
        if self.ui.input_cursor >= self.ui.input.chars().count() {
            return;
        }
        let at = self.byte_index(self.ui.input_cursor);
        self.ui.input.remove(at);
        self.ui.menu_selected = 0;
        self.ui.at_dismissed_at = None;
    }

    pub(in crate::tui) fn delete_word_before_cursor(&mut self) {
        if self.ui.input_cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.ui.input.chars().collect();
        let end = self.ui.input_cursor.min(chars.len());
        let mut start = end;
        while start > 0 && chars[start - 1].is_whitespace() && chars[start - 1] != '\n' {
            start -= 1;
        }
        if start == end && start > 0 && chars[start - 1] == '\n' {
            start -= 1;
        } else {
            while start > 0 && !chars[start - 1].is_whitespace() {
                start -= 1;
            }
        }
        let b0 = self.byte_index(start);
        let b1 = self.byte_index(end);
        self.ui.input.replace_range(b0..b1, "");
        self.ui.input_cursor = start;
        self.ui.menu_selected = 0;
        self.ui.at_dismissed_at = None;
    }

    pub(in crate::tui) fn move_cursor_word(&mut self, dir: isize) {
        let chars: Vec<char> = self.ui.input.chars().collect();
        let mut i = self.ui.input_cursor.min(chars.len());
        if dir < 0 {
            while i > 0 && chars[i - 1].is_whitespace() {
                i -= 1;
            }
            while i > 0 && !chars[i - 1].is_whitespace() {
                i -= 1;
            }
        } else {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
        }
        self.ui.input_cursor = i;
    }

    pub(in crate::tui) fn byte_index(&self, chars: usize) -> usize {
        self.ui.input
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.ui.input.len())
    }

    pub(in crate::tui) fn line_start_char(&self, cursor: usize) -> usize {
        let chars: Vec<char> = self.ui.input.chars().collect();
        let mut i = cursor.min(chars.len());
        while i > 0 && chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    pub(in crate::tui) fn line_end_char(&self, cursor: usize) -> usize {
        let chars: Vec<char> = self.ui.input.chars().collect();
        let mut i = cursor.min(chars.len());
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        i
    }

    pub(in crate::tui) fn move_cursor_vertical(&mut self, delta: isize) -> bool {
        let chars: Vec<char> = self.ui.input.chars().collect();
        let cur_start = self.line_start_char(self.ui.input_cursor);
        let col = self.ui.input_cursor - cur_start;
        if delta < 0 {
            if cur_start == 0 {
                return false;
            }
            let prev_end = cur_start - 1;
            let prev_start = self.line_start_char(prev_end);
            let prev_len = prev_end - prev_start;
            self.ui.input_cursor = prev_start + col.min(prev_len);
            true
        } else {
            let cur_end = self.line_end_char(self.ui.input_cursor);
            if cur_end >= chars.len() {
                return false;
            }
            let next_start = cur_end + 1;
            let next_end = self.line_end_char(next_start);
            let next_len = next_end - next_start;
            self.ui.input_cursor = next_start + col.min(next_len);
            true
        }
    }
}
