//! Inspector Tabs 4 & 5: Full streaming tool/job Output and structured Inspect details.
use crate::tui::*;

pub(super) fn draw_output_view(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let w = area.width.max(16) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    let tool_item = app
        .ui
        .inspector
        .output_item_id
        .and_then(|id| app.session.current.items.iter().find(|i| i.id() == id))
        .or_else(|| {
            app.ui.inspector.output_job_id.and_then(|jid| {
                app.session.current.items.iter().find(
                    |i| matches!(i, TranscriptItem::Tool { job_id: Some(j), .. } if *j == jid),
                )
            })
        })
        .or_else(|| {
            app.session
                .current
                .items
                .iter()
                .rev()
                .find(|i| matches!(i, TranscriptItem::Tool { .. }))
        });

    if let Some(TranscriptItem::Tool {
        title,
        subtitle,
        target,
        output,
        status,
        duration_ms,
        exit_code,
        started_ms,
        ..
    }) = tool_item
    {
        let (glyph, color) = match status {
            CardStatus::Queued => ("o", theme.warn),
            CardStatus::Running => ("*", theme.ok),
            CardStatus::Done => ("✓", theme.ok),
            CardStatus::Error => ("✗", theme.bad),
        };
        let elapsed = duration_ms.unwrap_or_else(|| now_ms().saturating_sub(*started_ms));
        let mut meta = vec![fmt_duration(elapsed)];
        if let Some(c) = exit_code {
            meta.push(format!("exit {c}"));
        }
        if target != "local" {
            meta.push(target.clone());
        }
        lines.push(Line::from(vec![
            Span::styled(format!(" {glyph} "), Style::default().fg(color)),
            Span::styled(
                truncate_width(title, w.saturating_sub(6)),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
        ]));
        let sub = if subtitle.is_empty() {
            meta.join(" - ")
        } else {
            format!("{subtitle}  -  {}", meta.join(" - "))
        };
        lines.push(Line::styled(
            format!("   {}", truncate_width(&sub, w.saturating_sub(4))),
            Style::default().fg(theme.dim),
        ));
        lines.push(Line::styled(
            "-".repeat(w),
            Style::default().fg(theme.border),
        ));

        if output.is_empty() {
            lines.push(Line::styled(
                if *status == CardStatus::Running {
                    "   (streaming / waiting for output.)"
                } else {
                    "   (no output)"
                },
                Style::default().fg(theme.dim),
            ));
        } else {
            for out_line in output {
                let st = Style::default().fg(if out_line.err { theme.bad } else { theme.text });
                for wrapped in wrap_text(&out_line.text, w.saturating_sub(3)) {
                    lines.push(Line::styled(format!(" {wrapped}"), st));
                }
            }
        }
    } else {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "  No tool/job output selected.",
            Style::default().fg(theme.dim),
        ));
        lines.push(Line::styled(
            "  Click any command card or job to inspect logs.",
            Style::default().fg(theme.dim),
        ));
    }

    super::render_scrolled_lines(
        frame,
        lines,
        &mut app.ui.inspector.scroll,
        &mut app.ui.inspector.max_scroll,
        area,
    );
}

pub(super) fn draw_inspect_view(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let w = area.width.max(16) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    let item = app
        .ui
        .inspector
        .inspect_item_id
        .and_then(|id| app.session.current.items.iter().find(|i| i.id() == id))
        .or_else(|| {
            app.ui
                .selected_card
                .and_then(|idx| app.session.current.items.get(idx))
        });

    if let Some(TranscriptItem::Tool {
        call_id,
        name,
        title,
        subtitle,
        target,
        status,
        duration_ms,
        exit_code,
        output,
        ..
    }) = item
    {
        lines.push(Line::styled(
            format!(" TOOL: {name}"),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            format!("   call_id : {call_id}"),
            Style::default().fg(theme.dim),
        ));
        lines.push(Line::styled(
            format!(
                "   title   : {}",
                truncate_width(title, w.saturating_sub(13))
            ),
            Style::default().fg(theme.text),
        ));
        if !subtitle.is_empty() {
            lines.push(Line::styled(
                format!(
                    "   detail  : {}",
                    truncate_width(subtitle, w.saturating_sub(13))
                ),
                Style::default().fg(theme.text),
            ));
        }
        lines.push(Line::styled(
            format!(
                "   status  : {:?} - target={target} - exit={:?} - dur={}ms",
                status,
                exit_code,
                duration_ms.unwrap_or(0)
            ),
            Style::default().fg(theme.dim),
        ));
        lines.push(Line::styled(
            "-".repeat(w),
            Style::default().fg(theme.border),
        ));
        for o in output {
            for wrapped in wrap_text(&o.text, w.saturating_sub(3)) {
                lines.push(Line::styled(
                    format!(" {wrapped}"),
                    Style::default().fg(if o.err { theme.bad } else { theme.text }),
                ));
            }
        }
    } else {
        // Show workspace skills & MCP servers details as general inspect view
        let trusted = app.inputs.catalog.trusted;
        let mcp: Vec<(String, String, bool)> = app
            .inputs
            .catalog
            .mcp
            .iter()
            .map(|srv| (srv.name.clone(), srv.scope.clone(), srv.disabled))
            .collect();
        let skills: Vec<(String, String, String)> = app
            .inputs
            .catalog
            .skills
            .iter()
            .map(|skill| {
                (
                    skill.name.clone(),
                    skill.scope.clone(),
                    skill.description.clone(),
                )
            })
            .collect();
        lines.push(Line::styled(
            " WORKSPACE INSPECTOR",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            format!("   cwd     : {}", app.cwd_label()),
            Style::default().fg(theme.text),
        ));
        lines.push(Line::styled(
            format!("   trusted : {trusted}"),
            Style::default().fg(theme.dim),
        ));
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!(" MCP SERVERS ({})", mcp.len()),
            Style::default().fg(theme.dim).add_modifier(Modifier::BOLD),
        ));
        for (name, scope, disabled) in &mcp {
            lines.push(Line::styled(
                format!(
                    "   * {name} [{scope}]{}",
                    if *disabled { " (disabled)" } else { "" }
                ),
                Style::default().fg(theme.text),
            ));
        }
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!(" SKILLS ({})", skills.len()),
            Style::default().fg(theme.dim).add_modifier(Modifier::BOLD),
        ));
        for (name, scope, description) in &skills {
            for wrapped in wrap_text(
                &format!("{name} [{scope}]: {description}"),
                w.saturating_sub(4),
            ) {
                lines.push(Line::styled(
                    format!("   {wrapped}"),
                    Style::default().fg(theme.text),
                ));
            }
        }
    }

    super::render_scrolled_lines(
        frame,
        lines,
        &mut app.ui.inspector.scroll,
        &mut app.ui.inspector.max_scroll,
        area,
    );
}
