//! Inspector Tab 1: Overview (Changed Files, Todos, Context, MCP/Skills, Jobs).
use super::push_linked_rows;
use crate::tui::links::LinkedSpan;
use crate::tui::*;

pub(super) fn draw_overview(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let w = area.width.max(16) as usize;
    let mut rows: Vec<LinkedLine> = Vec::new();

    push_section_header(&mut rows, "WORKSPACE", "", theme);
    push_tree_lines(&mut rows, app.session.workspace_index.tree(), w, theme);
    rows.push(LinkedLine::from_line(Line::from("")));

    // 1. Changed Files
    push_section_header(
        &mut rows,
        "CHANGED FILES",
        &format!("{}", app.session.current.changed_files.len()),
        theme,
    );
    if app.session.current.changed_files.is_empty() {
        rows.push(LinkedLine::from_line(Line::styled(
            "  none yet (click any path to preview)",
            Style::default().fg(theme.dim),
        )));
    } else {
        for (idx, fc) in app
            .session
            .current
            .changed_files
            .iter()
            .enumerate()
            .rev()
            .take(12)
        {
            let rel = links::display_rel(&app.session.current.cwd, std::path::Path::new(&fc.path));
            let stats = format!(" +{} -{}", fc.added, fc.removed);
            let name_budget = w.saturating_sub(display_width(&stats) + 4).max(8);
            let shown = truncate_width(&rel, name_budget);
            let pad = w.saturating_sub(display_width(&shown) + display_width(&stats) + 3);
            rows.push(LinkedLine::from_linked(vec![
                LinkedSpan {
                    span: Span::styled("  M ", Style::default().fg(theme.warn)),
                    target: Some(HitTarget::SelectChangedFile(idx)),
                },
                LinkedSpan {
                    span: Span::styled(
                        shown,
                        Style::default()
                            .fg(theme.link)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                    target: Some(HitTarget::SelectChangedFile(idx)),
                },
                LinkedSpan {
                    span: Span::raw(" ".repeat(pad)),
                    target: Some(HitTarget::SelectChangedFile(idx)),
                },
                LinkedSpan {
                    span: Span::styled(format!("+{}", fc.added), Style::default().fg(theme.ok)),
                    target: Some(HitTarget::SelectChangedFile(idx)),
                },
                LinkedSpan {
                    span: Span::styled(format!(" -{}", fc.removed), Style::default().fg(theme.bad)),
                    target: Some(HitTarget::SelectChangedFile(idx)),
                },
            ]));
        }
    }
    rows.push(LinkedLine::from_line(Line::from("")));

    // 2. Active Todos
    let todo_len = app.inputs.catalog.todos.len();
    let todo_done = app
        .inputs
        .catalog
        .todos
        .iter()
        .filter(|todo| todo.mark == crate::tui::inputs::TodoMark::Completed)
        .count();
    let todos: Vec<(crate::tui::inputs::TodoMark, String)> = app
        .inputs
        .catalog
        .todos
        .iter()
        .take(10)
        .map(|todo| (todo.mark, todo.content.clone()))
        .collect();
    if todo_len > 0 {
        push_section_header(
            &mut rows,
            "TODOS",
            &format!("{todo_done}/{todo_len}"),
            theme,
        );
        for (mark, content) in &todos {
            let (glyph, color) = match mark {
                crate::tui::inputs::TodoMark::Completed => ("✓", theme.ok),
                crate::tui::inputs::TodoMark::InProgress => ("*", theme.accent),
                crate::tui::inputs::TodoMark::Pending => ("o", theme.dim),
            };
            let text = truncate_width(content, w.saturating_sub(5));
            rows.push(LinkedLine::from_line(Line::from(vec![
                Span::styled(format!("  {glyph} "), Style::default().fg(color)),
                Span::styled(text, Style::default().fg(theme.text)),
            ])));
        }
        rows.push(LinkedLine::from_line(Line::from("")));
    }

    // 3. Context & Session
    push_section_header(&mut rows, "SESSION & CONTEXT", "", theme);
    if app.inputs.catalog.signed_in {
        let (used, est) = context_used(&app.session.current);
        let window = app.inputs.catalog.compact_at;
        let tier = app.inputs.catalog.tier.clone();
        let model_id = app.inputs.catalog.model_id.clone();
        let (filled, empty) = context_bar(used, window);
        let mark = if est { "~" } else { "" };
        rows.push(LinkedLine::from_line(Line::from(vec![
            Span::styled("  model   ", Style::default().fg(theme.dim)),
            Span::styled(
                truncate_width(&model_id, w.saturating_sub(10)),
                Style::default().fg(theme.secondary),
            ),
        ])));
        rows.push(LinkedLine::from_line(Line::from(vec![
            Span::styled("  window  ", Style::default().fg(theme.dim)),
            Span::styled(filled, Style::default().fg(theme.accent)),
            Span::styled(empty, Style::default().fg(theme.border)),
            Span::styled(
                format!(
                    " {mark}{}/{} ({})",
                    fmt_tokens(used),
                    fmt_tokens(window),
                    tier
                ),
                Style::default().fg(theme.text),
            ),
        ])));
        rows.push(LinkedLine::from_line(Line::from(vec![
            Span::styled("  tokens  ", Style::default().fg(theme.dim)),
            Span::styled(
                format!(
                    "in {} - out {}",
                    fmt_tokens(app.session.current.input_tokens),
                    fmt_tokens(app.session.current.output_tokens)
                ),
                Style::default().fg(theme.text),
            ),
        ])));
    } else {
        rows.push(LinkedLine::from_line(Line::styled(
            "  not signed in (run /login)",
            Style::default().fg(theme.warn),
        )));
    }
    rows.push(LinkedLine::from_line(Line::from("")));

    // 4. MCP & Skills
    let mcp_len = app.inputs.catalog.mcp.len();
    let skill_len = app.inputs.catalog.skills.len();
    let mcp: Vec<(String, String, bool)> = app
        .inputs
        .catalog
        .mcp
        .iter()
        .take(6)
        .map(|srv| (srv.name.clone(), srv.scope.clone(), srv.disabled))
        .collect();
    let skill_names: Vec<String> = app
        .inputs
        .catalog
        .skills
        .iter()
        .take(4)
        .map(|skill| skill.name.clone())
        .collect();
    push_section_header(
        &mut rows,
        "MCP & SKILLS",
        &format!("{mcp_len} mcp - {skill_len} skills"),
        theme,
    );
    if mcp_len == 0 && skill_len == 0 {
        rows.push(LinkedLine::from_line(Line::styled(
            "  none configured (/mcp, /skill)",
            Style::default().fg(theme.dim),
        )));
    } else {
        for (name, scope, disabled) in &mcp {
            let color = if *disabled { theme.dim } else { theme.ok };
            rows.push(LinkedLine::from_line(Line::from(vec![
                Span::styled("  * ", Style::default().fg(color)),
                Span::styled(
                    truncate_width(name, w.saturating_sub(14)),
                    Style::default().fg(theme.text),
                ),
                Span::styled(format!("  [{scope}]"), Style::default().fg(theme.dim)),
            ])));
        }
        if skill_len > 0 {
            let names: Vec<&str> = skill_names.iter().map(String::as_str).collect();
            let suffix = if skill_len > 4 { "." } else { "" };
            rows.push(LinkedLine::from_line(Line::from(vec![
                Span::styled("  * skills: ", Style::default().fg(theme.secondary)),
                Span::styled(
                    truncate_width(
                        &format!("{}{suffix}", names.join(", ")),
                        w.saturating_sub(12),
                    ),
                    Style::default().fg(theme.dim),
                ),
            ])));
        }
    }
    rows.push(LinkedLine::from_line(Line::from("")));

    // 5. Active & Recent Monitors
    if !app.agent.active_monitors.is_empty() {
        let active_n = app
            .agent
            .active_monitors
            .iter()
            .filter(|m| m.active)
            .count();
        push_section_header(&mut rows, "MONITORS", &format!("{active_n} active"), theme);
        for m in app.agent.active_monitors.iter().take(8) {
            let (glyph, color) = if m.active {
                ("*", theme.ok)
            } else {
                ("✓", theme.dim)
            };
            let cap = m
                .max_triggers
                .map(|max| format!("{}/{}", m.trigger_count, max))
                .unwrap_or_else(|| format!("{}", m.trigger_count));
            let suffix = if m.active {
                format!(" ({cap}) [stop]")
            } else {
                format!(" ({cap})")
            };
            let label = truncate_width(
                &format!("#{} {}", m.id, m.label),
                w.saturating_sub(display_width(&suffix) + 6).max(8),
            );
            let mut spans = vec![
                LinkedSpan {
                    span: Span::styled(format!("  {glyph} "), Style::default().fg(color)),
                    target: None,
                },
                LinkedSpan {
                    span: Span::styled(label, Style::default().fg(theme.text)),
                    target: None,
                },
                LinkedSpan {
                    span: Span::styled(format!(" ({cap})"), Style::default().fg(theme.dim)),
                    target: None,
                },
            ];
            if m.active {
                spans.push(LinkedSpan {
                    span: Span::styled(" [stop]", Style::default().fg(theme.warn)),
                    target: Some(HitTarget::StopMonitor(m.id)),
                });
            }
            rows.push(LinkedLine::from_linked(spans));
        }
        rows.push(LinkedLine::from_line(Line::from("")));
    }

    // 6. Detached Jobs
    push_section_header(
        &mut rows,
        "jobs",
        &format!("{}", app.jobs.list.len()),
        theme,
    );
    if app.jobs.list.is_empty() {
        rows.push(LinkedLine::from_line(Line::styled(
            "  none",
            Style::default().fg(theme.dim),
        )));
    } else {
        for (idx, job) in app.jobs.list.iter().enumerate() {
            let (glyph, color) = match &job.state {
                ProtoJobState::Queued { .. } => ("o", theme.warn),
                ProtoJobState::Running { .. } => ("*", theme.ok),
                ProtoJobState::Done { result, .. } if result.ok() => ("✓", theme.dim),
                ProtoJobState::Done { .. } => ("✗", theme.bad),
            };
            let selected = app.ui.focus == Focus::Sidebar && idx == app.jobs.selected;
            let cmd = truncate_width(&job.command(), w.saturating_sub(8));
            let mut line = LinkedLine::from_linked(vec![
                LinkedSpan {
                    span: Span::styled(format!("  {glyph} "), Style::default().fg(color)),
                    target: Some(HitTarget::OpenJobOutput(job.id)),
                },
                LinkedSpan {
                    span: Span::styled(cmd, Style::default().fg(theme.text)),
                    target: Some(HitTarget::OpenJobOutput(job.id)),
                },
            ]);
            if selected {
                line.line_style = Some(Style::default().bg(theme.selection));
            }
            rows.push(line);
        }
    }

    let max_scroll = rows.len().saturating_sub(area.height as usize);
    app.ui.inspector.max_scroll = max_scroll;
    app.ui.inspector.scroll = app.ui.inspector.scroll.min(max_scroll);
    push_linked_rows(frame, app, &rows, app.ui.inspector.scroll, area);
}

const TREE_PREVIEW: usize = 18;

fn push_tree_lines(rows: &mut Vec<LinkedLine>, tree: &str, width: usize, theme: &Theme) {
    let lines: Vec<&str> = tree.lines().filter(|line| !line.is_empty()).collect();
    if lines.is_empty() {
        rows.push(LinkedLine::from_line(Line::styled(
            "  (empty)",
            Style::default().fg(theme.dim),
        )));
        return;
    }
    let shown = TREE_PREVIEW.min(lines.len());
    for line in lines.iter().take(shown) {
        let text = truncate_width(&format!("  {line}"), width);
        rows.push(LinkedLine::from_line(Line::styled(
            text,
            Style::default().fg(theme.text),
        )));
    }
    if lines.len() > shown {
        rows.push(LinkedLine::from_line(Line::styled(
            format!("  ({} more)", lines.len() - shown),
            Style::default().fg(theme.dim),
        )));
    }
}

fn push_section_header(rows: &mut Vec<LinkedLine>, title: &str, meta: &str, theme: &Theme) {
    let mut spans = vec![Span::styled(
        format!(" {title}"),
        Style::default().fg(theme.dim).add_modifier(Modifier::BOLD),
    )];
    if !meta.is_empty() {
        spans.push(Span::styled(
            format!("  {meta}"),
            Style::default().fg(theme.dim),
        ));
    }
    rows.push(LinkedLine::from_line(Line::from(spans)));
}
