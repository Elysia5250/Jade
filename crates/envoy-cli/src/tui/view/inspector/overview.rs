//! Inspector Tab 1: Overview (Changed Files, Todos, Context, MCP/Skills, Jobs).
use super::push_linked_rows;
use crate::tui::links::{normalize_display_path, LinkedSpan};
use crate::tui::*;

pub(super) fn draw_overview(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let w = area.width.max(16) as usize;
    let mut rows: Vec<LinkedLine> = Vec::new();

    // 1. Changed Files
    push_section_header(
        &mut rows,
        "CHANGED FILES",
        &format!("{}", app.session.changed_files.len()),
        theme,
    );
    if app.session.changed_files.is_empty() {
        rows.push(LinkedLine::from_line(Line::styled(
            "  none yet (click any path to preview)",
            Style::default().fg(theme.dim),
        )));
    } else {
        for (idx, fc) in app.session.changed_files.iter().enumerate().rev().take(12) {
            let rel = normalize_display_path(&app.session.cwd, std::path::Path::new(&fc.path));
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
    let todos = crate::agent::tools::todo::load_todos(&app.data_dir, &app.session.id);
    if !todos.is_empty() {
        let done = todos
            .iter()
            .filter(|t| matches!(t.status, crate::agent::tools::todo::TodoStatus::Completed))
            .count();
        push_section_header(
            &mut rows,
            "TODOS",
            &format!("{done}/{}", todos.len()),
            theme,
        );
        for t in todos.iter().take(10) {
            let (glyph, color) = match t.status {
                crate::agent::tools::todo::TodoStatus::Completed => ("✓", theme.ok),
                crate::agent::tools::todo::TodoStatus::InProgress => ("*", theme.accent),
                crate::agent::tools::todo::TodoStatus::Pending => ("o", theme.dim),
            };
            let text = truncate_width(&t.content, w.saturating_sub(5));
            rows.push(LinkedLine::from_line(Line::from(vec![
                Span::styled(format!("  {glyph} "), Style::default().fg(color)),
                Span::styled(text, Style::default().fg(theme.text)),
            ])));
        }
        rows.push(LinkedLine::from_line(Line::from("")));
    }

    // 3. Context & Session
    push_section_header(&mut rows, "SESSION & CONTEXT", "", theme);
    if let Some((prov, model)) = resolve_active_provider(&app.data_dir) {
        let (used, est) = context_used(&app.session);
        let budget =
            crate::agent::context::resolve_budget(&app.data_dir, &prov.id, &model.model_id);
        let (filled, empty) = context_bar(used, budget.compact_at);
        let mark = if est { "~" } else { "" };
        rows.push(LinkedLine::from_line(Line::from(vec![
            Span::styled("  model   ", Style::default().fg(theme.dim)),
            Span::styled(
                truncate_width(&model.model_id, w.saturating_sub(10)),
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
                    fmt_tokens(budget.compact_at),
                    budget.tier
                ),
                Style::default().fg(theme.text),
            ),
        ])));
        rows.push(LinkedLine::from_line(Line::from(vec![
            Span::styled("  tokens  ", Style::default().fg(theme.dim)),
            Span::styled(
                format!(
                    "in {} - out {}",
                    fmt_tokens(app.session.input_tokens),
                    fmt_tokens(app.session.output_tokens)
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
    let mcp = crate::mcp::config::load_mcp_configs(&app.data_dir, &app.session.cwd);
    let skills = crate::skills::discover_skills(&app.data_dir, &app.session.cwd);
    push_section_header(
        &mut rows,
        "MCP & SKILLS",
        &format!("{} mcp - {} skills", mcp.servers.len(), skills.len()),
        theme,
    );
    if mcp.servers.is_empty() && skills.is_empty() {
        rows.push(LinkedLine::from_line(Line::styled(
            "  none configured (/mcp, /skill)",
            Style::default().fg(theme.dim),
        )));
    } else {
        for srv in mcp.servers.iter().take(6) {
            let color = if srv.disabled { theme.dim } else { theme.ok };
            rows.push(LinkedLine::from_line(Line::from(vec![
                Span::styled("  * ", Style::default().fg(color)),
                Span::styled(
                    truncate_width(&srv.name, w.saturating_sub(14)),
                    Style::default().fg(theme.text),
                ),
                Span::styled(
                    format!("  [{}]", srv.scope.label()),
                    Style::default().fg(theme.dim),
                ),
            ])));
        }
        if !skills.is_empty() {
            let names: Vec<&str> = skills.iter().take(4).map(|s| s.name.as_str()).collect();
            let suffix = if skills.len() > 4 { "." } else { "" };
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

    // 5. Detached Jobs
    push_section_header(&mut rows, "jobs", &format!("{}", app.jobs.len()), theme);
    if app.jobs.is_empty() {
        rows.push(LinkedLine::from_line(Line::styled(
            "  none",
            Style::default().fg(theme.dim),
        )));
    } else {
        for (idx, job) in app.jobs.iter().enumerate() {
            let (glyph, color) = match &job.state {
                JobState::Queued { .. } => ("o", theme.warn),
                JobState::Running { .. } => ("*", theme.ok),
                JobState::Done { result, .. } if result.ok() => ("✓", theme.dim),
                JobState::Done { .. } => ("✗", theme.bad),
            };
            let selected = app.focus == Focus::Sidebar && idx == app.job_selected;
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

    let max_scroll = (rows.len() as u16).saturating_sub(area.height);
    app.inspector.max_scroll = max_scroll;
    app.inspector.scroll = app.inspector.scroll.min(max_scroll);
    push_linked_rows(frame, app, &rows, app.inspector.scroll, area);
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
