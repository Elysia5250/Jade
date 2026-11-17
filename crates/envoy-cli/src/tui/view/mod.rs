//! Frame layout. Dual-pane workbench with transcript, rounded prompt, status, and universal right Inspector.
use super::*;

mod ask;
mod editor;
mod inspector;
mod menus;
mod modals;
mod queued;
mod sidebar;
mod status;
mod transcript;
mod welcome;

use ask::{ask_inline_height, draw_ask};
use editor::draw_editor;
use menus::{draw_cd_menu, draw_slash_menu};
use modals::draw_modal;
use queued::draw_queued;
use sidebar::draw_sidebar;
use status::draw_status;
use transcript::draw_transcript;
use welcome::draw_workspace_picker;

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let area = frame.area();
    app.hit_regions.clear();
    app.workspace_index.ensure_fresh(&app.session.cwd);

    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);

    if app.welcome.is_some() {
        draw_workspace_picker(frame, app, &theme, area);
        return;
    }

    let show_inspector =
        app.show_sidebar && (area.width >= 104 || (app.inspector.force_open && area.width >= 76));

    let (main_area, sidebar_area, divider_x) = if show_inspector {
        let desired = if let Some(ratio) = app.inspector.dragged_ratio {
            (area.width as f32 * ratio).round() as u16
        } else if app.inspector.tab.prefers_wide() {
            (area.width as f32 * 0.46).round() as u16
        } else {
            (area.width as f32 * 0.34).round() as u16
        };
        let max_right = area.width.saturating_sub(42).max(24);
        let right_w = desired.clamp(24, max_right);
        let left_w = area.width.saturating_sub(right_w + 1);
        let div_x = area.x + left_w;
        (
            Rect {
                x: area.x,
                y: area.y,
                width: left_w,
                height: area.height,
            },
            Some(Rect {
                x: div_x + 1,
                y: area.y,
                width: right_w,
                height: area.height,
            }),
            Some(div_x),
        )
    } else {
        app.inspector.last_area = Rect::default();
        (area, None, None)
    };
    app.inspector.divider_x = divider_x;

    if let Some(dx) = divider_x {
        let active = app.inspector.dragging_divider || app.inspector.hover_divider;
        let glyph = "|";
        let fg = if active { theme.accent } else { theme.border };
        let div_lines: Vec<Line> = (0..area.height)
            .map(|_| Line::styled(glyph, Style::default().fg(fg)))
            .collect();
        frame.render_widget(
            Paragraph::new(div_lines),
            Rect {
                x: dx,
                y: area.y,
                width: 1,
                height: area.height,
            },
        );
    }

    let column = centered_column(main_area, 108);
    let editor_inner_width = column.width.saturating_sub(6).max(10) as usize;
    let (visual_lines, cursor_row, cursor_col) =
        layout_editor_text(&app.input, app.input_cursor, editor_inner_width);
    let inner_rows = visual_lines.len().clamp(1, 8) as u16;
    let editor_height = inner_rows + 2;
    let max_rows = inner_rows as usize;
    let editor_skip = if cursor_row >= max_rows {
        (cursor_row + 1 - max_rows).min(visual_lines.len().saturating_sub(max_rows))
    } else {
        0
    };

    let ask_h = ask_inline_height(app, column.width);
    let (body_area, ask_area, editor_area, status_area) = if ask_h > 0 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(ask_h),
                Constraint::Length(editor_height),
                Constraint::Length(1),
            ])
            .split(column);
        (chunks[0], Some(chunks[1]), chunks[2], chunks[3])
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(editor_height),
                Constraint::Length(1),
            ])
            .split(column);
        (chunks[0], None, chunks[2], chunks[3])
    };

    draw_transcript(frame, app, &theme, body_area);
    if let Some(sidebar) = sidebar_area {
        draw_sidebar(frame, app, &theme, sidebar);
    }
    draw_editor(frame, app, &theme, editor_area, &visual_lines, editor_skip);
    draw_status(frame, app, &theme, status_area);
    draw_queued(frame, app, &theme, editor_area);

    if let Some(ask_rect) = ask_area {
        draw_ask(frame, app, &theme, ask_rect);
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
        let y = editor_area.y + 1 + visible_row as u16;
        if x < editor_area.x + editor_area.width.saturating_sub(2)
            && y < editor_area.y + editor_area.height.saturating_sub(1)
        {
            frame.set_cursor_position((x, y));
        }
    }

    let buf = frame.buffer_mut();
    apply_selection_highlight(buf, app.selection.as_ref(), theme.selection);
    snapshot_screen_lines(buf, &mut app.screen_lines);
}

fn apply_selection_highlight(
    buf: &mut ratatui::buffer::Buffer,
    selection: Option<&MouseSelection>,
    bg: ratatui::style::Color,
) {
    let Some(sel) = selection else {
        return;
    };
    if sel.start == sel.end {
        return;
    }
    let (start, end) = if (sel.start.row, sel.start.col) <= (sel.end.row, sel.end.col) {
        (sel.start, sel.end)
    } else {
        (sel.end, sel.start)
    };
    let area = buf.area;
    for row in start.row..=end.row {
        if row < area.y || row >= area.y + area.height {
            continue;
        }
        let lo = if row == start.row {
            start.col.max(area.x)
        } else {
            area.x
        };
        let hi = if row == end.row {
            end.col.min(area.x + area.width.saturating_sub(1))
        } else {
            area.x + area.width.saturating_sub(1)
        };
        for col in lo..=hi {
            buf[(col, row)].set_bg(bg);
        }
    }
}

fn snapshot_screen_lines(buf: &ratatui::buffer::Buffer, out: &mut Vec<Vec<String>>) {
    let area = buf.area;
    let total_rows = (area.y + area.height) as usize;
    let total_cols = (area.x + area.width) as usize;
    out.clear();
    out.resize_with(total_rows, || vec![String::new(); total_cols]);
    for row in area.y..area.y + area.height {
        for col in area.x..area.x + area.width {
            out[row as usize][col as usize] = buf[(col, row)].symbol().to_string();
        }
    }
}

/// Add a separator only when the previous line is not already blank (and not
/// at the very top of the transcript).
pub(super) fn push_blank_linked(lines: &mut Vec<LinkedLine>) {
    let already = lines
        .last()
        .map(|line| line.spans.iter().all(|s| s.span.content.trim().is_empty()))
        .unwrap_or(true);
    if !already {
        lines.push(LinkedLine::from_line(Line::from("")));
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
