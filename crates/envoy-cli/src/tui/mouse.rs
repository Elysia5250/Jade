//! Mouse wheel scrolling, divider drag-resize memory, link clicking, and OSC 52 copy.
use super::*;
use base64::Engine;
use std::io::Write;

impl App {
    pub(in crate::tui) fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        let col = mouse.column;
        let row = mouse.row;

        match mouse.kind {
            MouseEventKind::Moved => {
                self.ui.inspector.hover_divider = self
                    .ui.inspector
                    .divider_x
                    .map(|dx| col.abs_diff(dx) <= 1)
                    .unwrap_or(false);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.ui.modal.is_some() {
                    return None;
                }
                if let Some(dx) = self.ui.inspector.divider_x {
                    if col.abs_diff(dx) <= 1 {
                        self.ui.inspector.dragging_divider = true;
                        self.ui.inspector.hover_divider = true;
                        self.ui.selection = None;
                        self.ui.drag_start = None;
                        return None;
                    }
                }
                self.ui.selection = None;
                self.ui.drag_start = Some(GridPos { col, row });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.ui.inspector.dragging_divider {
                    let total_w = self.ui.inspector.last_area.x + self.ui.inspector.last_area.width;
                    if total_w > 40 {
                        let right_w = total_w.saturating_sub(col).max(24);
                        let ratio = (right_w as f32 / total_w as f32).clamp(0.22, 0.72);
                        self.ui.inspector.dragged_ratio = Some(ratio);
                    }
                    return None;
                }
                if let Some(start) = self.ui.drag_start {
                    let end = GridPos { col, row };
                    if start != end {
                        self.ui.selection = Some(MouseSelection {
                            start,
                            end,
                            active: true,
                        });
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.ui.inspector.dragging_divider {
                    self.ui.inspector.dragging_divider = false;
                    return None;
                }
                let drag_start = self.ui.drag_start.take();
                let finished_sel = if let Some(sel) = self.ui.selection.as_mut() {
                    if sel.active && sel.start != sel.end {
                        sel.active = false;
                        Some((sel.start, sel.end))
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some((s, e)) = finished_sel {
                    let copied = self.extract_selected_text(s, e);
                    if !copied.trim().is_empty() {
                        copy_to_clipboard(&copied);
                        let chars = copied.chars().count();
                        self.set_toast(format!("copied {chars} chars"), false);
                    }
                    self.ui.screen_lines.clear();
                    return None;
                }
                self.ui.selection = None;
                if drag_start.is_some() && self.ui.modal.is_none() {
                    return self.dispatch_click(col, row);
                }
            }
            MouseEventKind::ScrollUp => {
                if self.ui.modal.is_some() {
                    return None;
                }
                if self.is_over_inspector(col, row) {
                    self.ui.inspector.scroll = self.ui.inspector.scroll.saturating_sub(3);
                } else {
                    self.ui.follow = false;
                    self.ui.transcript_scroll = self.ui.transcript_scroll.saturating_sub(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.ui.modal.is_some() {
                    return None;
                }
                if self.is_over_inspector(col, row) {
                    self.ui.inspector.scroll = self
                        .ui.inspector
                        .scroll
                        .saturating_add(3)
                        .min(self.ui.inspector.max_scroll);
                } else {
                    self.ui.transcript_scroll = self
                        .ui.transcript_scroll
                        .saturating_add(3)
                        .min(self.ui.transcript_max_scroll);
                    if self.ui.transcript_scroll >= self.ui.transcript_max_scroll {
                        self.ui.follow = true;
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn is_over_inspector(&self, col: u16, row: u16) -> bool {
        let r = self.ui.inspector.last_area;
        r.width > 0
            && r.height > 0
            && col >= r.x
            && col < r.x + r.width
            && row >= r.y
            && row < r.y + r.height
    }

    pub(in crate::tui) fn dispatch_click(&mut self, col: u16, row: u16) -> Option<Action> {
        let hit = self
            .ui.hit_regions
            .iter()
            .rev()
            .find(|hr| {
                let r = hr.rect;
                r.width > 0
                    && r.height > 0
                    && col >= r.x
                    && col < r.x + r.width
                    && row >= r.y
                    && row < r.y + r.height
            })
            .map(|hr| hr.target.clone());

        if let Some(target) = hit {
            match target {
                HitTarget::OpenFile { path, line } => {
                    self.open_file_in_inspector(&path, line);
                }
                HitTarget::SelectInspectorTab(tab) => {
                    self.focus_inspector_tab(tab);
                }
                HitTarget::SelectChangedFile(idx) => {
                    self.open_diff_index(idx);
                }
                HitTarget::ToggleToolCard(id) => {
                    if let Some(TranscriptItem::Tool { collapsed, .. }) =
                        self.session.current.items.iter_mut().find(|i| i.id() == id)
                    {
                        *collapsed = !*collapsed;
                    }
                }
                HitTarget::ToggleToolGroup(ids) => {
                    if let Some(&first) = ids.first() {
                        if !self.session.expanded_explore_groups.remove(&first) {
                            self.session.expanded_explore_groups.insert(first);
                        }
                    }
                }
                HitTarget::ToggleReasoning(ids) => {
                    let any_collapsed = self.session.current.items.iter().any(|item| {
                        matches!(
                            item,
                            TranscriptItem::Assistant {
                                id,
                                reasoning_collapsed: true,
                                ..
                            } if ids.contains(id)
                        )
                    });
                    let next_collapsed = !any_collapsed;
                    for item in &mut self.session.current.items {
                        if let TranscriptItem::Assistant {
                            id,
                            reasoning_collapsed,
                            ..
                        } = item
                        {
                            if ids.contains(id) {
                                *reasoning_collapsed = next_collapsed;
                            }
                        }
                    }
                }
                HitTarget::OpenToolOutput(id) => {
                    self.open_tool_output(id);
                }
                HitTarget::OpenToolInspect(id) => {
                    self.open_tool_inspect(id);
                }
                HitTarget::OpenJobOutput(job_id) => {
                    if let Some(job) = self.jobs.list.iter().find(|j| j.id == job_id).cloned() {
                        return Some(Action::AttachJob(job));
                    }
                    self.open_job_output(job_id);
                }
                HitTarget::ToggleDiffLayout => {
                    self.ui.inspector.diff_layout = match self.ui.inspector.diff_layout {
                        diff::DiffLayout::Unified => diff::DiffLayout::Split,
                        diff::DiffLayout::Split => diff::DiffLayout::Unified,
                    };
                }
                HitTarget::JumpHunk(dir) => {
                    self.jump_diff_hunk(dir);
                }
                HitTarget::ToggleSidebar => {
                    return Some(self.run_command(Command::ToggleSidebar));
                }
                HitTarget::CycleMode => {
                    return Some(self.run_command(Command::ToggleMode));
                }
                HitTarget::CyclePermission => {
                    self.set_permission_mode(self.session.perm_mode.cycle());
                }
                HitTarget::SelectAskOption(idx) => {
                    if let Some(mut ask) = self.agent.pending_ask.take() {
                        if let Some(opt) = ask.options.get(idx) {
                            let label = opt.label.clone();
                            self.answer_ask(ask.id, label.clone());
                            self.set_toast(format!("chose: {label}"), false);
                        } else {
                            ask.selected = idx.min(ask.options.len().saturating_sub(1));
                            self.agent.pending_ask = Some(ask);
                        }
                    }
                }
                HitTarget::StopMonitor(monitor_id) => {
                    self.stop_monitor_by_id(monitor_id);
                }
                HitTarget::OpenLogin => {
                    self.open_login_modal();
                }
            }
            return None;
        }

        if self.is_over_inspector(col, row) {
            self.ui.focus = Focus::Sidebar;
        } else if row < self.ui.transcript_area.y + self.ui.transcript_area.height {
            self.ui.focus = Focus::Transcript;
        } else {
            self.ui.focus = Focus::Input;
        }
        None
    }

    fn extract_selected_text(&self, a: GridPos, b: GridPos) -> String {
        let (start, end) = if (a.row, a.col) <= (b.row, b.col) {
            (a, b)
        } else {
            (b, a)
        };
        let mut out = Vec::new();
        for row in start.row..=end.row {
            let Some(cells) = self.ui.screen_lines.get(row as usize) else {
                continue;
            };
            let lo = if row == start.row {
                (start.col as usize).min(cells.len())
            } else {
                0
            };
            let hi = if row == end.row {
                (end.col as usize + 1).min(cells.len())
            } else {
                cells.len()
            };
            if lo < hi {
                let mut line = String::new();
                for sym in &cells[lo..hi] {
                    if !sym.is_empty() {
                        line.push_str(sym);
                    }
                }
                out.push(line.trim_end().to_string());
            } else {
                out.push(String::new());
            }
        }
        out.join("\n").trim().to_string()
    }
}

fn copy_to_clipboard(text: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();

    let owned = text.to_string();
    let _ = std::thread::Builder::new()
        .name("envoy-clip".into())
        .spawn(move || clipboard_process(&owned));
}

fn clipboard_process(text: &str) {
    for (cmd, args) in [
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
        ("xsel", &["--clipboard", "--input"][..]),
        ("pbcopy", &[][..]),
    ] {
        let Ok(mut child) = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return,
                Ok(Some(_)) => break,
                Ok(None) if started.elapsed() >= Duration::from_secs(2) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
    }
}
