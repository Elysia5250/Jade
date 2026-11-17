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
                self.inspector.hover_divider = self
                    .inspector
                    .divider_x
                    .map(|dx| col.abs_diff(dx) <= 1)
                    .unwrap_or(false);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.modal.is_some() {
                    return None;
                }
                if let Some(dx) = self.inspector.divider_x {
                    if col.abs_diff(dx) <= 1 {
                        self.inspector.dragging_divider = true;
                        self.inspector.hover_divider = true;
                        self.selection = None;
                        self.drag_start = None;
                        return None;
                    }
                }
                self.selection = None;
                self.drag_start = Some(GridPos { col, row });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.inspector.dragging_divider {
                    let total_w = self.inspector.last_area.x + self.inspector.last_area.width;
                    if total_w > 40 {
                        let right_w = total_w.saturating_sub(col).max(24);
                        let ratio = (right_w as f32 / total_w as f32).clamp(0.22, 0.72);
                        self.inspector.dragged_ratio = Some(ratio);
                    }
                    return None;
                }
                if let Some(start) = self.drag_start {
                    let end = GridPos { col, row };
                    if start != end {
                        self.selection = Some(MouseSelection {
                            start,
                            end,
                            active: true,
                        });
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.inspector.dragging_divider {
                    self.inspector.dragging_divider = false;
                    return None;
                }
                let drag_start = self.drag_start.take();
                let finished_sel = if let Some(sel) = self.selection.as_mut() {
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
                    return None;
                }
                self.selection = None;
                if drag_start.is_some() && self.modal.is_none() {
                    return self.dispatch_click(col, row);
                }
            }
            MouseEventKind::ScrollUp => {
                if self.modal.is_some() {
                    return None;
                }
                if self.is_over_inspector(col, row) {
                    self.inspector.scroll = self.inspector.scroll.saturating_sub(3);
                } else {
                    self.follow = false;
                    self.transcript_scroll = self.transcript_scroll.saturating_sub(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.modal.is_some() {
                    return None;
                }
                if self.is_over_inspector(col, row) {
                    self.inspector.scroll = self
                        .inspector
                        .scroll
                        .saturating_add(3)
                        .min(self.inspector.max_scroll);
                } else {
                    self.transcript_scroll = self
                        .transcript_scroll
                        .saturating_add(3)
                        .min(self.transcript_max_scroll);
                    if self.transcript_scroll >= self.transcript_max_scroll {
                        self.follow = true;
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn is_over_inspector(&self, col: u16, row: u16) -> bool {
        let r = self.inspector.last_area;
        r.width > 0
            && r.height > 0
            && col >= r.x
            && col < r.x + r.width
            && row >= r.y
            && row < r.y + r.height
    }

    pub(in crate::tui) fn dispatch_click(&mut self, col: u16, row: u16) -> Option<Action> {
        let hit = self
            .hit_regions
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
                        self.session.items.iter_mut().find(|i| i.id() == id)
                    {
                        *collapsed = !*collapsed;
                    }
                }
                HitTarget::ToggleToolGroup(ids) => {
                    if let Some(&first) = ids.first() {
                        if !self.expanded_explore_groups.remove(&first) {
                            self.expanded_explore_groups.insert(first);
                        }
                    }
                }
                HitTarget::ToggleReasoning(id) => {
                    if let Some(TranscriptItem::Assistant {
                        reasoning_collapsed,
                        ..
                    }) = self.session.items.iter_mut().find(|i| i.id() == id)
                    {
                        *reasoning_collapsed = !*reasoning_collapsed;
                    }
                }
                HitTarget::OpenToolOutput(id) => {
                    self.open_tool_output(id);
                }
                HitTarget::OpenToolInspect(id) => {
                    self.open_tool_inspect(id);
                }
                HitTarget::OpenJobOutput(job_id) => {
                    if let Some(job) = self.jobs.iter().find(|j| j.id == job_id).cloned() {
                        return Some(Action::AttachJob(job));
                    }
                    self.open_job_output(job_id);
                }
                HitTarget::ToggleDiffLayout => {
                    self.inspector.diff_layout = match self.inspector.diff_layout {
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
                    self.set_permission_mode(self.perm_mode.cycle());
                }
                HitTarget::SelectAskOption(idx) => {
                    if let Some(mut ask) = self.pending_ask.take() {
                        if let Some(opt) = ask.options.get(idx) {
                            let label = opt.label.clone();
                            self.answer_ask(ask.id, label.clone());
                            self.set_toast(format!("chose: {label}"), false);
                        } else {
                            ask.selected = idx.min(ask.options.len().saturating_sub(1));
                            self.pending_ask = Some(ask);
                        }
                    }
                }
            }
            return None;
        }

        if self.is_over_inspector(col, row) {
            self.focus = Focus::Sidebar;
        } else if row < self.transcript_area.y + self.transcript_area.height {
            self.focus = Focus::Transcript;
        } else {
            self.focus = Focus::Input;
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
            let Some(cells) = self.screen_lines.get(row as usize) else {
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

    for (cmd, args) in [
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
        ("xsel", &["--clipboard", "--input"][..]),
        ("pbcopy", &[][..]),
    ] {
        if let Ok(mut child) = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            break;
        }
    }
}
