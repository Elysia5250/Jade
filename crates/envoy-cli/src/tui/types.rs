//! Shared UI layout, hit-testing, inspector, and mouse selection types.
use super::diff::DiffLayout;
use super::links::LinkedSpan;
use envoy_agent::memory::MemoryNote;
use envoy_agent::skills::SkillSummary;
use envoy_proto::JobId;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InspectorTab {
    Overview,
    Diff,
    File,
    Output,
    Inspect,
}

impl InspectorTab {
    pub(crate) const ALL: [Self; 5] = [
        Self::Overview,
        Self::Diff,
        Self::File,
        Self::Output,
        Self::Inspect,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Overview => "1 Overview",
            Self::Diff => "2 Diff",
            Self::File => "3 File",
            Self::Output => "4 Out",
            Self::Inspect => "5 Info",
        }
    }

    pub(crate) fn prefers_wide(self) -> bool {
        !matches!(self, Self::Overview)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HitTarget {
    OpenFile { path: String, line: Option<usize> },
    SelectInspectorTab(InspectorTab),
    SelectChangedFile(usize),
    ToggleToolCard(u64),
    ToggleToolGroup(Vec<u64>),
    ToggleReasoning(Vec<u64>),
    OpenToolOutput(u64),
    OpenToolInspect(u64),
    OpenJobOutput(JobId),
    ToggleDiffLayout,
    JumpHunk(isize),
    ToggleSidebar,
    CycleMode,
    CyclePermission,
    SelectAskOption(usize),
    StopMonitor(u64),
    OpenLogin,
}

#[derive(Debug, Clone)]
pub(crate) struct HitRegion {
    pub(crate) rect: Rect,
    pub(crate) target: HitTarget,
}

pub(crate) struct InspectorState {
    pub(crate) tab: InspectorTab,
    /// User-dragged right pane width ratio (`right_w / total_w`), remembered across the session.
    pub(crate) dragged_ratio: Option<f32>,
    pub(crate) force_open: bool,
    pub(crate) scroll: u16,
    pub(crate) max_scroll: u16,
    pub(crate) height: u16,
    pub(crate) diff_layout: DiffLayout,
    pub(crate) selected_diff: usize,
    pub(crate) file_path: Option<String>,
    pub(crate) file_line: Option<usize>,
    pub(crate) output_item_id: Option<u64>,
    pub(crate) output_job_id: Option<JobId>,
    pub(crate) inspect_item_id: Option<u64>,
    pub(crate) divider_x: Option<u16>,
    pub(crate) dragging_divider: bool,
    pub(crate) hover_divider: bool,
    pub(crate) last_area: Rect,
    pub(crate) diff_hunk_lines: Vec<usize>,
}

impl Default for InspectorState {
    fn default() -> Self {
        Self {
            tab: InspectorTab::Overview,
            dragged_ratio: None,
            force_open: false,
            scroll: 0,
            max_scroll: 0,
            height: 0,
            diff_layout: DiffLayout::Unified,
            selected_diff: 0,
            file_path: None,
            file_line: None,
            output_item_id: None,
            output_job_id: None,
            inspect_item_id: None,
            divider_x: None,
            dragging_divider: false,
            hover_divider: false,
            last_area: Rect::default(),
            diff_hunk_lines: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GridPos {
    pub(crate) col: u16,
    pub(crate) row: u16,
}

#[derive(Debug, Clone)]
pub(crate) struct MouseSelection {
    pub(crate) start: GridPos,
    pub(crate) end: GridPos,
    pub(crate) active: bool,
}

#[derive(Clone)]
pub(crate) struct LinkedLine {
    pub(crate) spans: Vec<LinkedSpan>,
    pub(crate) line_style: Option<Style>,
}

impl LinkedLine {
    pub(crate) fn from_line(line: Line<'_>) -> Self {
        let line_style = Some(line.style);
        let spans = line
            .spans
            .into_iter()
            .map(|s| LinkedSpan {
                span: ratatui::text::Span::styled(s.content.into_owned(), s.style),
                target: None,
            })
            .collect();
        Self { spans, line_style }
    }

    pub(crate) fn from_linked(spans: Vec<LinkedSpan>) -> Self {
        Self {
            spans,
            line_style: None,
        }
    }
}

/// Startup workspace picker. Shown before the console attaches to a directory.
pub(crate) struct Welcome {
    pub(crate) rows: Vec<WelcomeRow>,
    pub(crate) selected: usize,
    pub(crate) path_mode: bool,
    pub(crate) path: String,
    pub(crate) path_selected: usize,
    pub(crate) error: Option<String>,
    pub(crate) folder_mode: bool,
    pub(crate) skills_mode: bool,
    pub(crate) memory_mode: bool,
    pub(crate) naming: bool,
    pub(crate) folder_parent: Option<PathBuf>,
    pub(crate) draft: String,
    pub(crate) list_selected: usize,
    pub(crate) skills: Vec<SkillSummary>,
    pub(crate) notes: Vec<MemoryNote>,
    pub(crate) skill_count: usize,
    pub(crate) memory_count: usize,
}

#[derive(Clone)]
pub(crate) enum WelcomeRow {
    Here(PathBuf, bool),
    Recent(PathBuf, String),
    OpenPath,
    NewFolder,
    Account,
    Skills,
    Memory,
}
