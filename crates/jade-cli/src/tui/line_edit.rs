//! Reusable single-line text editor helpers with cursor movement and kill shortcuts.
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditOutcome {
    Ignored,
    Moved,
    Changed,
}

pub(crate) fn clamp_cursor(text: &str, cursor: &mut usize) {
    let len = text.chars().count();
    if *cursor > len {
        *cursor = len;
    }
}

pub(crate) fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

pub(crate) fn cursor_col(text: &str, cursor: usize) -> usize {
    let end = byte_index(text, cursor);
    UnicodeWidthStr::width(&text[..end])
}

pub(crate) fn insert_char(text: &mut String, cursor: &mut usize, c: char) {
    clamp_cursor(text, cursor);
    let at = byte_index(text, *cursor);
    text.insert(at, c);
    *cursor += 1;
}

pub(crate) fn insert_str(text: &mut String, cursor: &mut usize, s: &str) {
    if s.is_empty() {
        return;
    }
    clamp_cursor(text, cursor);
    let at = byte_index(text, *cursor);
    text.insert_str(at, s);
    *cursor += s.chars().count();
}

pub(crate) fn backspace(text: &mut String, cursor: &mut usize) -> bool {
    clamp_cursor(text, cursor);
    if *cursor == 0 {
        return false;
    }
    let at = byte_index(text, *cursor - 1);
    text.remove(at);
    *cursor -= 1;
    true
}

pub(crate) fn delete(text: &mut String, cursor: &mut usize) -> bool {
    clamp_cursor(text, cursor);
    if *cursor >= text.chars().count() {
        return false;
    }
    let at = byte_index(text, *cursor);
    text.remove(at);
    true
}

pub(crate) fn move_word(text: &str, cursor: &mut usize, dir: isize) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = (*cursor).min(chars.len());
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
    *cursor = i;
}

pub(crate) fn delete_word_before(text: &mut String, cursor: &mut usize) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let end = (*cursor).min(chars.len());
    if end == 0 {
        return false;
    }
    let mut start = end;
    while start > 0 && chars[start - 1].is_whitespace() {
        start -= 1;
    }
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let b0 = byte_index(text, start);
    let b1 = byte_index(text, end);
    text.replace_range(b0..b1, "");
    *cursor = start;
    true
}

pub(crate) fn kill_before(text: &mut String, cursor: &mut usize) -> bool {
    clamp_cursor(text, cursor);
    if *cursor == 0 {
        return false;
    }
    let b = byte_index(text, *cursor);
    text.replace_range(..b, "");
    *cursor = 0;
    true
}

pub(crate) fn kill_after(text: &mut String, cursor: &mut usize) -> bool {
    clamp_cursor(text, cursor);
    let b = byte_index(text, *cursor);
    if b >= text.len() {
        return false;
    }
    text.truncate(b);
    true
}

/// Handle common single-line editing shortcuts (arrows, Home/End, Ctrl+A/E/U/K/W,
/// Alt+Left/Right/B/F/Backspace, Backspace, Delete, and printable characters).
pub(crate) fn handle_key(text: &mut String, cursor: &mut usize, key: KeyEvent) -> EditOutcome {
    clamp_cursor(text, cursor);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Left if ctrl || alt => {
            move_word(text, cursor, -1);
            EditOutcome::Moved
        }
        KeyCode::Right if ctrl || alt => {
            move_word(text, cursor, 1);
            EditOutcome::Moved
        }
        KeyCode::Left => {
            *cursor = cursor.saturating_sub(1);
            EditOutcome::Moved
        }
        KeyCode::Right => {
            let len = text.chars().count();
            if *cursor < len {
                *cursor += 1;
            }
            EditOutcome::Moved
        }
        KeyCode::Home => {
            *cursor = 0;
            EditOutcome::Moved
        }
        KeyCode::End => {
            *cursor = text.chars().count();
            EditOutcome::Moved
        }
        KeyCode::Backspace if alt => {
            if delete_word_before(text, cursor) {
                EditOutcome::Changed
            } else {
                EditOutcome::Moved
            }
        }
        KeyCode::Backspace => {
            if backspace(text, cursor) {
                EditOutcome::Changed
            } else {
                EditOutcome::Moved
            }
        }
        KeyCode::Delete => {
            if delete(text, cursor) {
                EditOutcome::Changed
            } else {
                EditOutcome::Moved
            }
        }
        KeyCode::Char('a') if ctrl => {
            *cursor = 0;
            EditOutcome::Moved
        }
        KeyCode::Char('e') if ctrl => {
            *cursor = text.chars().count();
            EditOutcome::Moved
        }
        KeyCode::Char('u') if ctrl => {
            if kill_before(text, cursor) {
                EditOutcome::Changed
            } else {
                EditOutcome::Moved
            }
        }
        KeyCode::Char('k') if ctrl => {
            if kill_after(text, cursor) {
                EditOutcome::Changed
            } else {
                EditOutcome::Moved
            }
        }
        KeyCode::Char('w') if ctrl => {
            if delete_word_before(text, cursor) {
                EditOutcome::Changed
            } else {
                EditOutcome::Moved
            }
        }
        KeyCode::Char('b') if alt => {
            move_word(text, cursor, -1);
            EditOutcome::Moved
        }
        KeyCode::Char('f') if alt => {
            move_word(text, cursor, 1);
            EditOutcome::Moved
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            insert_char(text, cursor, c);
            EditOutcome::Changed
        }
        _ => EditOutcome::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_movement_word_ops_and_unicode_work() {
        let mut text = "hello 世界 world".to_string();
        let mut cursor = text.chars().count();

        assert_eq!(
            handle_key(
                &mut text,
                &mut cursor,
                KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)
            ),
            EditOutcome::Changed
        );
        assert_eq!(text, "hello 世界 ");
        assert_eq!(cursor, 9);

        assert_eq!(
            handle_key(
                &mut text,
                &mut cursor,
                KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)
            ),
            EditOutcome::Moved
        );
        assert_eq!(cursor, 6);

        insert_str(&mut text, &mut cursor, "新");
        assert_eq!(text, "hello 新世界 ");
        assert_eq!(cursor, 7);

        assert_eq!(
            handle_key(
                &mut text,
                &mut cursor,
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)
            ),
            EditOutcome::Changed
        );
        assert_eq!(text, "hello 新");

        assert_eq!(
            handle_key(
                &mut text,
                &mut cursor,
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
            ),
            EditOutcome::Changed
        );
        assert!(text.is_empty());
        assert_eq!(cursor, 0);
    }
}
