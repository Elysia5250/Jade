//! Shared UI layout, hit-testing, inspector, and mouse selection types.
use super::diff::DiffLayout;
use super::links::LinkedSpan;
use jade_agent::memory::MemoryNote;
use jade_agent::skills::SkillSummary;
use jade_proto::JobId;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InspectorTab {
    Overview,
    Diff,
    File,
    Output,
    Inspect,
    /// One delegated child's transcript. Shown only after a subagent row is opened.
    Agent,
    /// Plan sidechat. Shown only while that thread is open.
    Side,
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
            Self::Agent => "Agent",
            Self::Side => "Plan",
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
    OpenModels,
    CycleEffort,
    OpenBudget,
    OpenTrustPicker,
    JumpToBottom,
    ToggleFileMarkdown,
    OpenSubagent { call_id: String, index: u32 },
    OpenHub,
    HubReturn,
    HubSelectTab(HubTab),
    HubSelectFocus(HubFocus),
    HubSession(usize),
    HubWorkspace(usize),
    HubControl(HubControl),
    HubOpenPath,
    HubNewFolder,
    HubPathEntry(usize),
    HubListItem(usize),
    ModalItem(usize),
    ModalLoginField(usize),
    FocusSideInput,
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
    pub(crate) scroll: usize,
    pub(crate) max_scroll: usize,
    pub(crate) height: u16,
    pub(crate) diff_layout: DiffLayout,
    pub(crate) markdown_raw: bool,
    pub(crate) selected_diff: usize,
    pub(crate) file_path: Option<String>,
    pub(crate) file_line: Option<usize>,
    pub(crate) output_item_id: Option<u64>,
    pub(crate) output_job_id: Option<JobId>,
    pub(crate) inspect_item_id: Option<u64>,
    /// Parent delegate call id and 1-based child index shown in the Agent tab.
    pub(crate) subagent: Option<(String, u32)>,
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
            markdown_raw: false,
            selected_diff: 0,
            file_path: None,
            file_line: None,
            output_item_id: None,
            output_job_id: None,
            inspect_item_id: None,
            subagent: None,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HubFocus {
    #[default]
    Left,
    Controls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HubTab {
    #[default]
    Sessions,
    Workspaces,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HubControl {
    Provider,
    Model,
    Effort,
    Budget,
    Delegate,
    Permission,
    Trust,
    Target,
    Skills,
    Memory,
    Mcp,
    Theme,
    Sidebar,
}

impl HubControl {
    pub(crate) const ALL: [Self; 13] = [
        Self::Provider,
        Self::Model,
        Self::Effort,
        Self::Budget,
        Self::Delegate,
        Self::Permission,
        Self::Trust,
        Self::Target,
        Self::Skills,
        Self::Memory,
        Self::Mcp,
        Self::Theme,
        Self::Sidebar,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum WelcomeMode {
    #[default]
    Home,
    OpenPath,
    NewFolderPickParent,
    NewFolderName,
    RenameSession,
    Skills,
    SkillsNew,
    Memory,
    MemoryNew,
    Mcp,
}

impl WelcomeMode {
    pub(crate) fn is_naming(self) -> bool {
        matches!(
            self,
            Self::NewFolderName | Self::RenameSession | Self::SkillsNew | Self::MemoryNew
        )
    }

    pub(crate) fn is_path_input(self) -> bool {
        matches!(self, Self::OpenPath | Self::NewFolderPickParent)
    }
}

#[derive(Clone)]
pub(crate) struct HubSessionRow {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) cwd: PathBuf,
    pub(crate) target: String,
    pub(crate) updated_ms: u64,
    pub(crate) item_count: usize,
    pub(crate) prompt_tokens: u64,
    pub(crate) changed_files: usize,
    pub(crate) last_preview: String,
    pub(crate) running: bool,
    pub(crate) queued: usize,
    pub(crate) is_current: bool,
}

#[derive(Clone)]
pub(crate) struct HubWorkspaceRow {
    pub(crate) cwd: PathBuf,
    pub(crate) title: String,
    pub(crate) updated_ms: u64,
    pub(crate) session_count: usize,
    pub(crate) running_count: usize,
    pub(crate) is_here: bool,
    pub(crate) resumes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathBrowserEntry {
    /// Confirm and open (or select as parent) the current directory.
    OpenCurrent(PathBuf),
    /// Navigate up to the parent directory (`../`).
    Parent(PathBuf),
    /// Descend into a subdirectory (`name/`).
    Dir { display: String, path: PathBuf },
    /// Submit an explicitly typed path (e.g. `/tmp/foo` or `~/work`).
    TypedPath { display: String, raw: String },
}

/// Central Mission Control Hub. Shown on startup and reachable anytime via `/home` or `Ctrl+X h`.
pub(crate) struct Welcome {
    pub(crate) can_return: bool,
    pub(crate) focus: HubFocus,
    pub(crate) tab: HubTab,
    pub(crate) mode: WelcomeMode,
    pub(crate) sessions: Vec<HubSessionRow>,
    pub(crate) workspaces: Vec<HubWorkspaceRow>,
    pub(crate) selected_session: usize,
    pub(crate) selected_workspace: usize,
    pub(crate) selected_control: usize,
    pub(crate) search: String,
    pub(crate) search_cursor: usize,
    pub(crate) searching: bool,
    pub(crate) confirm_delete: Option<String>,
    pub(crate) rename_target: Option<String>,
    pub(crate) browse_dir: PathBuf,
    pub(crate) path: String,
    pub(crate) path_cursor: usize,
    pub(crate) path_selected: usize,
    pub(crate) error: Option<String>,
    pub(crate) folder_parent: Option<PathBuf>,
    pub(crate) draft: String,
    pub(crate) draft_cursor: usize,
    pub(crate) list_selected: usize,
    pub(crate) skills: Vec<SkillSummary>,
    pub(crate) notes: Vec<MemoryNote>,
    pub(crate) skill_count: usize,
    pub(crate) skill_injected_count: usize,
    pub(crate) memory_count: usize,
    pub(crate) memory_enabled_count: usize,
    pub(crate) mcp_probe: Vec<String>,
    pub(crate) mcp_probing: bool,
    pub(crate) live_refreshed_at: Option<Instant>,
}
