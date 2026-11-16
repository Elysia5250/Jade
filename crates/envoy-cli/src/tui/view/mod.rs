//! Frame layout. The screen shows the transcript, the prompt, and one status line.
use super::*;

mod ask;
mod editor;
mod menus;
mod modals;
mod queued;
mod sidebar;
mod status;
mod transcript;

use ask::draw_ask;
use editor::draw_editor;
use menus::{draw_cd_menu, draw_slash_menu};
use modals::draw_modal;
use queued::draw_queued;
use sidebar::draw_sidebar;
use status::draw_status;
use transcript::draw_transcript;

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg)),
        area,
    );

    let (main_area, sidebar_area) = if app.show_sidebar && area.width >= 100 {
        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(48), Constraint::Length(32)])
            .split(area);
        let mut main = halves[0];
        main.width = main.width.saturating_sub(1);
        (main, Some(halves[1]))
    } else {
        (area, None)
    };
    let column = centered_column(main_area, 96);
    let editor_inner_width = column.width.saturating_sub(6).max(10) as usize;
    let (visual_lines, cursor_row, cursor_col) =
        layout_editor_text(&app.input, app.input_cursor, editor_inner_width);
    let editor_height = visual_lines.len().clamp(1, 8) as u16;
    let max_rows = editor_height as usize;
    let editor_skip = if cursor_row >= max_rows {
        (cursor_row + 1 - max_rows).min(visual_lines.len().saturating_sub(max_rows))
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(editor_height),
            Constraint::Length(1),
        ])
        .split(column);
    let body_area = chunks[0];
    let editor_area = chunks[2];
    let status_area = chunks[3];

    draw_transcript(frame, app, &theme, body_area);
    if let Some(sidebar) = sidebar_area {
        draw_sidebar(frame, app, &theme, sidebar);
    }
    draw_editor(frame, app, &theme, editor_area, &visual_lines, editor_skip);
    draw_status(frame, app, &theme, status_area);
    draw_queued(frame, app, &theme, editor_area);

    if app.pending_ask.is_some() {
        draw_ask(frame, app, &theme, area);
    } else {
        if app.focus == Focus::Input && app.modal.is_none() {
            if !app.cd_completions().is_empty() {
                draw_cd_menu(frame, app, &theme, editor_area);
            } else if !app.filtered_slash().is_empty() {
                draw_slash_menu(frame, app, &theme, editor_area);
            }
        }
        if app.modal.is_some() {
            draw_modal(frame, app, &theme, area);
        }
    }

    if app.modal.is_none() && app.pending_ask.is_none() && app.focus == Focus::Input {
        let x = editor_area.x + 2 + cursor_col as u16;
        let visible_row = cursor_row
            .saturating_sub(editor_skip)
            .min(max_rows.saturating_sub(1));
        let y = editor_area.y + visible_row as u16;
        if x < editor_area.x + editor_area.width.saturating_sub(1) && y < editor_area.y + editor_area.height
        {
            frame.set_cursor_position((x, y));
        }
    }
}

/// Add a separator only when the previous line is not already blank (and not
/// at the very top of the transcript).
fn push_blank(lines: &mut Vec<Line<'_>>) {
    let already = lines
        .last()
        .map(|line| line.spans.iter().all(|s| s.content.trim().is_empty()))
        .unwrap_or(true);
    if !already {
        lines.push(Line::from(""));
    }
}

fn centered_column(area: Rect, max: u16) -> Rect {
    let width = area.width.min(max);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y,
        width,
        height: area.height,
    }
}
