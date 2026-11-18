//! Prompt editor with spacious breathing room, prompt prefix, and mode indicator.
use crate::tui::*;

pub(super) fn draw_editor(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    area: Rect,
    visual_lines: &[String],
    skip: usize,
) {
    let mode_color = match app.mode {
        InputMode::Agent => theme.accent,
        InputMode::Shell => theme.warn,
    };
    let frame_color = if app.focus == Focus::Input {
        mode_color
    } else {
        theme.border
    };
    let title = match app.mode {
        InputMode::Agent => Span::styled(
            " / commands · @ files · ! shell · Ctrl+J newline ",
            Style::default().fg(theme.dim),
        ),
        InputMode::Shell => Span::styled(
            " ! shell — Tab to return to agent ",
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ),
    };
    let v_pad = if area.height >= 6 { 1 } else { 0 };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(frame_color))
        .padding(Padding::new(1, 1, v_pad, v_pad))
        .title_bottom(title.into_right_aligned_line())
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height == 0 {
        return;
    }

    let max_rows = inner.height as usize;
    let skip = skip.min(visual_lines.len().saturating_sub(max_rows));
    let end = (skip + max_rows).min(visual_lines.len());
    let prompt_glyph = match app.mode {
        InputMode::Agent => Span::styled(
            "❯ ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        InputMode::Shell => Span::styled(
            "$ ",
            Style::default()
                .fg(theme.warn)
                .add_modifier(Modifier::BOLD),
        ),
    };
    let text_budget = inner.width.saturating_sub(2) as usize;

    let lines: Vec<Line> = if app.input.is_empty() {
        let placeholder = match app.mode {
            InputMode::Agent if resolve_active_provider(&app.data_dir).is_none() => {
                "Sign in first (/login or press Enter to connect a provider)"
            }
            InputMode::Agent => "Message Envoy…",
            InputMode::Shell => "Run a shell command…",
        };
        vec![Line::from(vec![
            prompt_glyph,
            Span::styled(
                truncate_width(placeholder, text_budget),
                Style::default().fg(theme.dim),
            ),
        ])]
    } else {
        visual_lines[skip..end]
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let prefix = if i == 0 && skip == 0 {
                    prompt_glyph.clone()
                } else {
                    Span::raw("  ")
                };
                Line::from(vec![prefix, Span::raw(text.clone())])
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), inner);
}
