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
    if area.width < 2 || area.height < 2 {
        return;
    }
    let mode_color = match app.ui.mode {
        InputMode::Agent => theme.accent,
        InputMode::Shell => theme.warn,
    };
    let frame_color = if app.ui.focus == Focus::Input {
        mode_color
    } else {
        theme.border
    };
    let title = match app.ui.mode {
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
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(frame_color))
        .padding(Padding::new(1, 1, v_pad, v_pad))
        .title_bottom(title.into_right_aligned_line())
        .style(Style::default().bg(theme.panel));
    if !app.ui.pending_images.is_empty() {
        let names: Vec<String> = app
            .ui
            .pending_images
            .iter()
            .map(|img| super::super::clipboard::format_image_label(img, None))
            .collect();
        let badge = format!(
            " [{} image(s): {} · Backspace to remove] ",
            app.ui.pending_images.len(),
            truncate_width(&names.join(", "), 48)
        );
        block = block.title_top(Span::styled(
            badge,
            Style::default().fg(theme.ok).add_modifier(Modifier::BOLD),
        ));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height == 0 {
        return;
    }

    let max_rows = inner.height as usize;
    let skip = skip.min(visual_lines.len().saturating_sub(max_rows));
    let end = (skip + max_rows).min(visual_lines.len());
    let prompt_glyph = match app.ui.mode {
        InputMode::Agent => Span::styled(
            "❯ ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        InputMode::Shell => Span::styled(
            "$ ",
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ),
    };
    let text_budget = inner.width.saturating_sub(2) as usize;

    let lines: Vec<Line> = if app.ui.input.is_empty() {
        let placeholder = match app.ui.mode {
            InputMode::Agent if !app.inputs.catalog.signed_in => {
                "Sign in first (/login or press Enter to connect a provider)"
            }
            InputMode::Agent if !app.ui.pending_images.is_empty() => {
                "Add a message for attached image(s), or press Enter to send…"
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
