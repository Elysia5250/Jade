//! Right Hub pane: Quick Controls deck for model, safety, capabilities, and appearance.
use crate::tui::*;

pub(super) fn draw_controls_pane(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    wide: bool,
) {
    let Some(w) = app.ui.welcome.as_ref() else {
        return;
    };
    let focused = w.focus == HubFocus::Controls;
    let selected = w.selected_control;
    let border_fg = if focused { theme.accent } else { theme.border };
    let title = if wide {
        Line::from(Span::styled(
            " Quick Controls ",
            Style::default()
                .fg(if focused { theme.accent } else { theme.text })
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(vec![
            Span::styled(" [1] Sessions ", Style::default().fg(theme.dim)),
            Span::styled(" [2] Workspaces ", Style::default().fg(theme.dim)),
            Span::styled(
                " [3] Controls ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_fg))
        .title(title)
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut y = inner.y;
    let max_y = inner.y + inner.height;
    let width = inner.width as usize;
    let mut last_group = "";

    for (idx, ctrl) in HubControl::ALL.iter().copied().enumerate() {
        let group = control_group(ctrl);
        if group != last_group {
            if !last_group.is_empty() && y < max_y {
                y += 1;
            }
            if y >= max_y {
                break;
            }
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!(" {group}"),
                    Style::default().fg(theme.dim).add_modifier(Modifier::BOLD),
                )),
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
            );
            y += 1;
            last_group = group;
        }
        if y >= max_y {
            break;
        }
        let is_sel = focused && idx == selected;
        let rect = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: 1,
        };
        app.ui.hit_regions.push(HitRegion {
            rect,
            target: HitTarget::HubControl(ctrl),
        });
        render_control_row(frame, app, ctrl, theme, rect, width, is_sel);
        y += 1;
    }
}

fn control_group(ctrl: HubControl) -> &'static str {
    match ctrl {
        HubControl::Provider
        | HubControl::Model
        | HubControl::Effort
        | HubControl::Budget
        | HubControl::Delegate => "MODEL & REASONING",
        HubControl::Permission | HubControl::Trust | HubControl::Target => "SAFETY & RUNTIME",
        HubControl::Skills | HubControl::Memory | HubControl::Mcp => "CAPABILITIES",
        HubControl::Theme | HubControl::Sidebar => "APPEARANCE",
    }
}

fn render_control_row(
    frame: &mut Frame,
    app: &App,
    ctrl: HubControl,
    theme: &Theme,
    rect: Rect,
    width: usize,
    selected: bool,
) {
    let (label, value, val_color) = control_display(app, ctrl, theme);
    let bg = if selected {
        theme.selection
    } else {
        theme.panel
    };
    let marker = if selected { "▸" } else { " " };
    let label_col = format!(" {marker} {:<10}", label);
    let val_budget = width.saturating_sub(display_width(&label_col) + 2).max(6);
    let val_shown = truncate_width(&value, val_budget);
    let pad = width.saturating_sub(display_width(&label_col) + display_width(&val_shown) + 1);
    let line = Line::from(vec![
        Span::styled(
            label_col,
            Style::default()
                .fg(if selected { theme.accent } else { theme.dim })
                .bg(bg)
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        Span::styled(
            format!("{val_shown} "),
            Style::default()
                .fg(val_color)
                .bg(bg)
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), rect);
}

fn control_display(
    app: &App,
    ctrl: HubControl,
    theme: &Theme,
) -> (&'static str, String, ratatui::style::Color) {
    let cat = &app.inputs.catalog;
    let welcome = app.ui.welcome.as_ref();
    match ctrl {
        HubControl::Provider => {
            if cat.signed_in {
                ("Provider", format!("{} ▸", cat.provider_id), theme.ok)
            } else {
                ("Provider", "sign in ▸".into(), theme.warn)
            }
        }
        HubControl::Model => {
            if cat.signed_in {
                ("Model", format!("{} ▸", cat.model_id), theme.secondary)
            } else {
                ("Model", "none ▸".into(), theme.dim)
            }
        }
        HubControl::Effort => ("Effort", format!("◄ {} ►", cat.effort_level), theme.text),
        HubControl::Budget => {
            let cap = if cat.compact_at > 0 {
                format!(" ({})", fmt_tokens(cat.compact_at))
            } else {
                String::new()
            };
            let tier = if cat.tier.is_empty() {
                "balanced"
            } else {
                &cat.tier
            };
            ("Budget", format!("◄ {tier}{cap} ►"), theme.text)
        }
        HubControl::Delegate => (
            "Delegate",
            format!("◄ {} ►", cat.delegate_level.as_str()),
            theme.text,
        ),
        HubControl::Permission => {
            let color = match app.session.perm_mode {
                PermissionMode::Default => theme.dim,
                PermissionMode::AcceptEdits => theme.ok,
                PermissionMode::Plan => theme.accent,
                PermissionMode::Auto => theme.warn,
                PermissionMode::Turbo => theme.bad,
            };
            (
                "Perm Mode",
                format!("◄ {} ►", app.session.perm_mode.label()),
                color,
            )
        }
        HubControl::Trust => {
            if cat.trusted {
                ("Trust", "● trusted".into(), theme.ok)
            } else {
                ("Trust", "○ untrusted".into(), theme.warn)
            }
        }
        HubControl::Target => ("Target", format!("{} ▸", app.jobs.target), theme.secondary),
        HubControl::Skills => {
            let inj = welcome.map(|w| w.skill_injected_count).unwrap_or(0);
            let total = welcome.map(|w| w.skill_count).unwrap_or(0);
            ("Skills", format!("{inj}/{total} active ▸"), theme.text)
        }
        HubControl::Memory => {
            let en = welcome.map(|w| w.memory_enabled_count).unwrap_or(0);
            let total = welcome.map(|w| w.memory_count).unwrap_or(0);
            ("Memory", format!("{en}/{total} notes ▸"), theme.text)
        }
        HubControl::Mcp => {
            let n = cat.mcp.len();
            ("MCP", format!("{n} servers ▸"), theme.text)
        }
        HubControl::Theme => (
            "Theme",
            format!("◄ {} ►", THEMES[app.ui.theme].0),
            theme.accent,
        ),
        HubControl::Sidebar => {
            let label = if app.ui.show_sidebar {
                "● open"
            } else {
                "○ hidden"
            };
            (
                "Inspector",
                label.into(),
                if app.ui.show_sidebar {
                    theme.ok
                } else {
                    theme.dim
                },
            )
        }
    }
}
