//! Composer status bar directly above the input box: live activity/toasts + model/context meter.
use super::bar::{render_split_bar, BarSegment};
use crate::tui::inputs::TodoMark;
use crate::tui::*;

pub(super) fn draw_composer_bar(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let left = build_left_segments(app, theme);
    let right = build_right_segments(app, theme);
    render_split_bar(frame, app, area, None, 1, "  ", left, right);
}

fn build_left_segments(app: &App, theme: &Theme) -> Vec<BarSegment> {
    let mut left = Vec::new();
    if app
        .ui
        .interrupt_at
        .is_some_and(|at| at.elapsed() < INTERRUPT_TIMEOUT)
    {
        left.push(BarSegment::plain(
            vec![Span::styled(
                "! Esc again to interrupt",
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            )],
            0,
        ));
        return left;
    }
    if let Some((msg, err)) = app.active_toast() {
        let color = if err { theme.bad } else { theme.ok };
        left.push(BarSegment::plain(
            vec![Span::styled(
                truncate_width(msg, 44),
                Style::default().fg(color),
            )],
            0,
        ));
        return left;
    }

    let sub_info = running_subagent_info(app);
    if app.agent.running || sub_info.0 > 0 {
        let (phase, tool_name) = active_phase(app);
        let spans = decrypt_line_spans(
            phase,
            0,
            now_ms(),
            0x5354_4154,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(theme.dim),
        );
        left.push(BarSegment::plain(spans, 0));
        if let Some(tool) = tool_name {
            left.push(BarSegment::plain(
                vec![Span::styled(
                    format!("· {}", truncate_width(&tool, 16)),
                    Style::default().fg(theme.dim),
                )],
                2,
            ));
        }
        if let Some(started) = app.agent.turn_started_at {
            left.push(BarSegment::plain(
                vec![Span::styled(
                    fmt_duration(started.elapsed().as_millis() as u64),
                    Style::default().fg(theme.dim),
                )],
                1,
            ));
        }
        if let (count, Some((call_id, index))) = sub_info {
            let label = if count == 1 {
                "1 agent".to_string()
            } else {
                format!("{count} agents")
            };
            left.push(BarSegment::clickable(
                vec![Span::styled(label, Style::default().fg(theme.secondary))],
                HitTarget::OpenSubagent { call_id, index },
                2,
            ));
        }
        if !app.agent.queued.is_empty() {
            left.push(BarSegment::plain(
                vec![Span::styled(
                    format!("+{} queued", app.agent.queued.len()),
                    Style::default().fg(theme.warn),
                )],
                1,
            ));
        }
        return left;
    }

    if !app.ui.follow && app.ui.transcript_scroll < app.ui.transcript_max_scroll {
        left.push(BarSegment::clickable(
            vec![Span::styled(
                "↓ scrolled",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )],
            HitTarget::JumpToBottom,
            0,
        ));
    }

    if let Some(seg) = todo_segment(app, theme) {
        left.push(seg);
    }

    let active_monitors = app
        .agent
        .active_monitors
        .iter()
        .filter(|m| m.active)
        .count();
    if active_monitors > 0 {
        left.push(BarSegment::clickable(
            vec![Span::styled(
                format!("mon:{active_monitors}"),
                Style::default().fg(theme.accent),
            )],
            HitTarget::SelectInspectorTab(InspectorTab::Overview),
            2,
        ));
    }

    let active_jobs = app
        .jobs
        .list
        .iter()
        .filter(|j| {
            matches!(
                j.state,
                ProtoJobState::Running { .. } | ProtoJobState::Queued { .. }
            )
        })
        .count();
    if active_jobs > 0 {
        left.push(BarSegment::clickable(
            vec![Span::styled(
                format!("job:{active_jobs}"),
                Style::default().fg(theme.ok),
            )],
            HitTarget::SelectInspectorTab(InspectorTab::Overview),
            2,
        ));
    }
    left
}

fn build_right_segments(app: &App, theme: &Theme) -> Vec<BarSegment> {
    let mut right = Vec::new();
    if !app.inputs.catalog.signed_in {
        right.push(BarSegment::clickable(
            vec![Span::styled(
                "/login to sign in",
                Style::default()
                    .fg(theme.warn)
                    .add_modifier(Modifier::UNDERLINED),
            )],
            HitTarget::OpenLogin,
            0,
        ));
        return right;
    }

    right.push(BarSegment::clickable(
        vec![Span::styled(
            app.inputs.catalog.model_id.clone(),
            Style::default().fg(theme.secondary),
        )],
        HitTarget::OpenModels,
        0,
    ));

    let effort = &app.inputs.catalog.effort_level;
    if !app.inputs.catalog.effort_options.is_empty()
        || (!effort.is_empty() && effort != "default")
    {
        right.push(BarSegment::clickable(
            vec![Span::styled(
                format!("effort:{effort}"),
                Style::default().fg(theme.dim),
            )],
            HitTarget::CycleEffort,
            3,
        ));
    }

    let (used, estimated) = context_used(&app.session.current);
    let window = app.inputs.catalog.compact_at;
    let hard_window = app.inputs.catalog.hard_window;
    let ratio = if window == 0 {
        0.0
    } else {
        (used as f64 / window as f64).clamp(0.0, 1.0)
    };
    let bar_color = if ratio >= 0.9 {
        theme.bad
    } else if ratio >= 0.7 {
        theme.warn
    } else {
        theme.accent
    };
    let (filled, empty) = context_bar(used, window);
    right.push(BarSegment::clickable(
        vec![
            Span::styled(filled, Style::default().fg(bar_color)),
            Span::styled(empty, Style::default().fg(theme.border)),
        ],
        HitTarget::OpenBudget,
        2,
    ));

    let mark = if estimated { "~" } else { "" };
    let cap_suffix = if window > 0 && window + 64_000 < hard_window {
        format!("~{}", fmt_tokens(hard_window))
    } else {
        String::new()
    };
    right.push(BarSegment::clickable(
        vec![Span::styled(
            format!("{mark}{}/{}{cap_suffix}", fmt_tokens(used), fmt_tokens(window)),
            Style::default().fg(theme.dim),
        )],
        HitTarget::OpenBudget,
        0,
    ));

    if window > 0 {
        let pct = ((used as f64 / window as f64) * 100.0)
            .round()
            .clamp(0.0, 999.0) as u64;
        right.push(BarSegment::clickable(
            vec![Span::styled(
                format!("{pct}%"),
                Style::default().fg(theme.dim),
            )],
            HitTarget::OpenBudget,
            3,
        ));
    }

    let cache_read = app.session.current.cache_read_tokens;
    let total_in = app.session.current.input_tokens;
    if cache_read > 0 && total_in > 0 {
        let cache_pct = (cache_read.saturating_mul(100) / total_in.max(1)).min(100);
        right.push(BarSegment::plain(
            vec![Span::styled(
                format!("cache {cache_pct}%"),
                Style::default().fg(theme.dim),
            )],
            4,
        ));
    }
    right
}

fn active_phase(app: &App) -> (&'static str, Option<String>) {
    let running_tool = app
        .session
        .current
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::Tool {
                status: CardStatus::Running,
                name,
                ..
            } if name != "say" => Some(name.clone()),
            _ => None,
        });
    if let Some(tool) = running_tool {
        return ("working", Some(tool));
    }
    if let Some(id) = app.agent.active_assistant_id {
        if let Some(TranscriptItem::Assistant {
            reasoning, text, ..
        }) = app.session.current.items.iter().find(|i| i.id() == id)
        {
            if !reasoning.is_empty() && text.trim().is_empty() {
                return ("thinking", None);
            }
        }
    }
    ("working", None)
}

fn running_subagent_info(app: &App) -> (usize, Option<(String, u32)>) {
    let mut count = 0usize;
    let mut first = None;
    for (call_id, lanes) in &app.session.current.subagents {
        for lane in lanes {
            if lane.status == CardStatus::Running {
                count += 1;
                if first.is_none() {
                    first = Some((call_id.clone(), lane.index));
                }
            }
        }
    }
    (count, first)
}

fn todo_segment(app: &App, theme: &Theme) -> Option<BarSegment> {
    let todos = &app.inputs.catalog.todos;
    if todos.is_empty() {
        return None;
    }
    let done = todos
        .iter()
        .filter(|t| t.mark == TodoMark::Completed)
        .count();
    if done >= todos.len() {
        return None;
    }
    let active = todos
        .iter()
        .find(|t| t.mark == TodoMark::InProgress)
        .or_else(|| todos.iter().find(|t| t.mark == TodoMark::Pending))?;
    let label = format!(
        "todo {done}/{} · {}",
        todos.len(),
        truncate_width(&active.content, 24)
    );
    Some(BarSegment::clickable(
        vec![Span::styled(label, Style::default().fg(theme.text))],
        HitTarget::SelectInspectorTab(InspectorTab::Overview),
        2,
    ))
}
