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

        // 3. `@file` workspace completion popup
        let at_files = self.file_completions();
        if !at_files.is_empty() {
            let last = at_files.len() - 1;
            match key.code {
                KeyCode::Up => {
                    self.menu_selected = self.menu_selected.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.menu_selected = (self.menu_selected + 1).min(last);
                    return Action::None;
                }
                KeyCode::Tab | KeyCode::Enter if key.modifiers == KeyModifiers::NONE => {
                    let chosen = at_files[self.menu_selected.min(last)].clone();
                    self.accept_at_completion(&chosen);
                    return Action::None;
                }
                KeyCode::Esc => {
                    if let Some((start_ch, _, _)) = self.at_mention_span() {
                        self.at_dismissed_at = Some(start_ch);
                    }
                    self.menu_selected = 0;
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
                self.at_dismissed_at = None;

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
                        sandbox: envoy_proto::SandboxPolicy::Off,
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
                self.mode = match self.mode {
                    InputMode::Agent => InputMode::Shell,
                    InputMode::Shell => InputMode::Agent,
                };
                self.set_toast(format!("mode: {}", self.mode.label()), false);
                Action::None
            }
            KeyCode::BackTab => {
                self.set_permission_mode(self.perm_mode.cycle());
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
                        self.at_dismissed_at = None;
                    }
                    'a' => self.input_cursor = self.line_start_char(self.input_cursor),
                    'e' => self.input_cursor = self.line_end_char(self.input_cursor),
                    'k' => {
                        let byte = self.byte_index(self.input_cursor);
                        self.input.truncate(byte);
                        self.at_dismissed_at = None;
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
        if let Some(modal) = self.modal.take() {
            self.handle_modal_paste(modal, text);
            return;
        }
        if self.welcome.is_some() {
            self.handle_welcome_paste(text);
            return;
        }
        if self.pending_ask.is_some() {
            return;
        }
        self.focus = Focus::Input;
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return;
        }
        let at = self.byte_index(self.input_cursor);
        self.input.insert_str(at, &normalized);
        self.input_cursor += normalized.chars().count();
        self.menu_selected = 0;
        self.at_dismissed_at = None;
        if self.at_mention_span().is_some() {
            self.workspace_index.ensure_fresh(&self.session.cwd);
        }
    }

    pub(in crate::tui) fn at_mention_span(&self) -> Option<(usize, usize, String)> {
        if self.slash_query().is_some() || self.cd_query().is_some() {
            return None;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let cursor = self.input_cursor.min(chars.len());
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
        if self.at_dismissed_at == Some(start_ch) {
            return Vec::new();
        }
        self.workspace_index.search_files(&query, 10)
    }

    pub(in crate::tui) fn accept_at_completion(&mut self, chosen: &str) {
        let Some((start_ch, end_ch, _)) = self.at_mention_span() else {
            return;
        };
        let b0 = self.byte_index(start_ch);
        let b1 = self.byte_index(end_ch);
        let replacement = format!("@{chosen} ");
        self.input.replace_range(b0..b1, &replacement);
        self.input_cursor = start_ch + replacement.chars().count();
        self.menu_selected = 0;
        self.at_dismissed_at = None;
    }

    pub(in crate::tui) fn insert_char(&mut self, c: char) {
        let at = self.byte_index(self.input_cursor);
        self.input.insert(at, c);
        self.input_cursor += 1;
        self.menu_selected = 0;
        self.at_dismissed_at = None;
        if c == '@' || self.at_mention_span().is_some() {
            self.workspace_index.ensure_fresh(&self.session.cwd);
        }
    }

    pub(in crate::tui) fn delete_before_cursor(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let at = self.byte_index(self.input_cursor - 1);
        self.input.remove(at);
        self.input_cursor -= 1;
        self.menu_selected = 0;
        self.at_dismissed_at = None;
    }

    pub(in crate::tui) fn delete_at_cursor(&mut self) {
        if self.input_cursor >= self.input.chars().count() {
            return;
        }
        let at = self.byte_index(self.input_cursor);
        self.input.remove(at);
        self.menu_selected = 0;
        self.at_dismissed_at = None;
    }

    pub(in crate::tui) fn delete_word_before_cursor(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let end = self.input_cursor.min(chars.len());
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
        self.input.replace_range(b0..b1, "");
        self.input_cursor = start;
        self.menu_selected = 0;
        self.at_dismissed_at = None;
    }

    pub(in crate::tui) fn move_cursor_word(&mut self, dir: isize) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.input_cursor.min(chars.len());
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
        self.input_cursor = i;
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
