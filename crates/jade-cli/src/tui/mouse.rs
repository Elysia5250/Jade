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
                self.ui.inspector.dragging_divider = false;
                self.ui.inspector.hover_divider = self.ui.welcome.is_none()
                    && self.ui.modal.is_none()
                    && self
                        .ui
                        .inspector
                        .divider_x
                        .map(|dx| col.abs_diff(dx) <= 1)
                        .unwrap_or(false);
                self.update_hover(col, row);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.ui.inspector.dragging_divider = false;
                if self.ui.modal.is_none() && self.ui.welcome.is_none() {
                    if let Some(dx) = self.ui.inspector.divider_x {
                        if col.abs_diff(dx) <= 1 {
                            self.ui.inspector.dragging_divider = true;
                            self.ui.inspector.hover_divider = true;
                            self.ui.selection = None;
                            self.ui.drag_start = None;
                            return None;
                        }
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
                if self.ui.modal.is_some() {
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
                if drag_start.is_some() {
                    return self.dispatch_click(col, row);
                }
            }
            MouseEventKind::ScrollUp => {
                let step = self.scroll_step();
                if self.ui.modal.is_some() {
                    self.scroll_modal(-1);
                    return None;
                }
                if self.ui.welcome.is_some() {
                    self.scroll_welcome_popup(-1);
                    return None;
                }
                if self.is_over_inspector(col, row) {
                    self.ui.inspector.scroll = self.ui.inspector.scroll.saturating_sub(step);
                } else {
                    self.ui.follow = false;
                    self.ui.transcript_scroll = self.ui.transcript_scroll.saturating_sub(step);
                }
            }
            MouseEventKind::ScrollDown => {
                let step = self.scroll_step();
                if self.ui.modal.is_some() {
                    self.scroll_modal(1);
                    return None;
                }
                if self.ui.welcome.is_some() {
                    self.scroll_welcome_popup(1);
                    return None;
                }
                if self.is_over_inspector(col, row) {
                    self.ui.inspector.scroll = self
                        .ui
                        .inspector
                        .scroll
                        .saturating_add(step)
                        .min(self.ui.inspector.max_scroll);
                } else {
                    self.ui.transcript_scroll = self
                        .ui
                        .transcript_scroll
                        .saturating_add(step)
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

    fn scroll_step(&mut self) -> usize {
        let now = Instant::now();
        let rapid = self
            .ui
            .last_scroll_at
            .is_some_and(|prev| now.saturating_duration_since(prev) < Duration::from_millis(55));
        self.ui.last_scroll_at = Some(now);
        if rapid {
            1
        } else if cfg!(target_os = "macos") {
            2
        } else {
            3
        }
    }

    fn update_hover(&mut self, col: u16, row: u16) {
        let modal_open = self.ui.modal.is_some();
        let hit = self
            .ui
            .hit_regions
            .iter()
            .rev()
            .find(|hr| {
                if modal_open
                    && !matches!(
                        hr.target,
                        HitTarget::ModalItem(_) | HitTarget::ModalLoginField(_)
                    )
                {
                    return false;
                }
                rect_contains(hr.rect, col, row)
            })
            .map(|hr| (hr.target.clone(), hr.rect));
        match hit {
            Some((target, rect)) => {
                self.ui.hover_target = Some(target);
                self.ui.hover_rect = Some(rect);
            }
            None => {
                self.ui.hover_target = None;
                self.ui.hover_rect = None;
            }
        }
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
        if self.ui.modal.is_some() {
            let hit = self
                .ui
                .hit_regions
                .iter()
                .rev()
                .find(|hr| {
                    matches!(
                        hr.target,
                        HitTarget::ModalItem(_) | HitTarget::ModalLoginField(_)
                    ) && rect_contains(hr.rect, col, row)
                })
                .map(|hr| hr.target.clone());
            match hit {
                Some(HitTarget::ModalItem(idx)) => return self.click_modal_item(idx),
                Some(HitTarget::ModalLoginField(field)) => {
                    self.click_modal_login_field(field);
                    return None;
                }
                _ => {
                    if self
                        .ui
                        .modal_rect
                        .is_some_and(|r| !rect_contains(r, col, row))
                    {
                        self.dismiss_modal();
                    }
                    return None;
                }
            }
        }

        if self
            .ui
            .welcome
            .as_ref()
            .is_some_and(|w| w.mode != WelcomeMode::Home)
        {
            let hit = self
                .ui
                .hit_regions
                .iter()
                .rev()
                .find(|hr| rect_contains(hr.rect, col, row))
                .map(|hr| hr.target.clone());
            match hit {
                Some(HitTarget::HubPathEntry(idx)) => return Some(self.hub_click_path_entry(idx)),
                Some(HitTarget::HubListItem(idx)) => {
                    self.hub_click_list_item(idx);
                    return None;
                }
                _ => {
                    if self
                        .ui
                        .modal_rect
                        .is_some_and(|r| !rect_contains(r, col, row))
                    {
                        self.dismiss_welcome_subpanel();
                    }
                    return None;
                }
            }
        }

        let hit = self
            .ui
            .hit_regions
            .iter()
            .rev()
            .find(|hr| rect_contains(hr.rect, col, row))
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
                HitTarget::OpenModels => {
                    self.open_models_modal();
                }
                HitTarget::CycleEffort => {
                    self.cycle_effort(1);
                }
                HitTarget::OpenBudget => {
                    self.open_budget_picker();
                }
                HitTarget::OpenTrustPicker => {
                    self.open_trust_picker();
                }
                HitTarget::JumpToBottom => {
                    self.ui.follow = true;
                    self.ui.transcript_scroll = self.ui.transcript_max_scroll;
                }
                HitTarget::ToggleFileMarkdown => {
                    self.ui.inspector.markdown_raw = !self.ui.inspector.markdown_raw;
                    self.ui.inspector.scroll = 0;
                }
                HitTarget::OpenSubagent { call_id, index } => {
                    self.open_subagent(call_id, index);
                }
                HitTarget::OpenHub => {
                    self.return_to_hub();
                }
                HitTarget::HubReturn => {
                    self.close_hub_to_current();
                }
                HitTarget::HubSelectTab(tab) => {
                    self.hub_select_tab(tab);
                }
                HitTarget::HubSelectFocus(focus) => {
                    if let Some(w) = self.ui.welcome.as_mut() {
                        w.focus = focus;
                    }
                }
                HitTarget::HubSession(idx) => {
                    return self.hub_click_session(idx);
                }
                HitTarget::HubDeactivateSession => {
                    self.deactivate_selected_session();
                }
                HitTarget::HubWorkspace(idx) => {
                    self.hub_click_workspace(idx);
                }
                HitTarget::HubControl(ctrl) => {
                    return self.hub_click_control(ctrl);
                }
                HitTarget::HubOpenPath => {
                    self.hub_open_path_panel();
                }
                HitTarget::HubNewFolder => {
                    self.hub_open_new_folder_panel();
                }
                HitTarget::HubPathEntry(idx) => {
                    return Some(self.hub_click_path_entry(idx));
                }
                HitTarget::HubListItem(idx) => {
                    self.hub_click_list_item(idx);
                }
                HitTarget::ModalItem(idx) => {
                    return self.click_modal_item(idx);
                }
                HitTarget::ModalLoginField(field) => {
                    self.click_modal_login_field(field);
                }
                HitTarget::FocusSideInput => {
                    if self.ui.side_open {
                        self.ui.focus = Focus::Side;
                    }
                }
            }
            return None;
        }

        if self.ui.welcome.is_some() {
            return None;
        }

        if self.is_over_inspector(col, row) {
            self.ui.focus = Focus::Sidebar;
        } else if row >= self.ui.transcript_area.y
            && row < self.ui.transcript_area.y + self.ui.transcript_area.height
        {
            self.ui.focus = Focus::Transcript;
        } else if row >= self.ui.transcript_area.y + self.ui.transcript_area.height {
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

fn rect_contains(r: Rect, col: u16, row: u16) -> bool {
    r.width > 0
        && r.height > 0
        && col >= r.x
        && col < r.x + r.width
        && row >= r.y
        && row < r.y + r.height
}

fn copy_to_clipboard(text: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();

    let owned = text.to_string();
    let _ = std::thread::Builder::new()
        .name("jade-clip".into())
        .spawn(move || clipboard_process(&owned));
}

fn clipboard_process(text: &str) {
    #[cfg(target_os = "macos")]
    let cmds: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(not(target_os = "macos"))]
    let cmds: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for &(cmd, args) in cmds {
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
