//! Composed state slices owned by [`App`].
use super::*;

pub(crate) struct AgentState {
    pub(crate) running: bool,
    pub(crate) stream: Option<mpsc::UnboundedReceiver<AgentResp>>,
    pub(crate) retry_at: Instant,
    pub(crate) offline: bool,
    /// True after the daemon has sent the attach snapshot. Until then sends
    /// are held in the outbox so they cannot race the session creation.
    pub(crate) ready: bool,
    /// Last daemon incarnation observed on `SnapshotStart`. `0` means none.
    pub(crate) daemon_incarnation: u64,
    pub(crate) snapshot: Option<SnapshotAccum>,
    /// Set while applying a replay that included `SessionReset`, so token
    /// totals already stored on the session are not added a second time.
    pub(crate) skip_replay_tokens: bool,
    pub(crate) pending_ask: Option<PendingAsk>,
    pub(crate) queued: VecDeque<String>,
    pub(crate) outbox: VecDeque<(String, Vec<jade_agent::session::ImageAttachment>, SendMode)>,
    pub(crate) active_assistant_id: Option<u64>,
    pub(crate) active_tool_items: HashMap<String, u64>,
    pub(crate) active_monitors: Vec<MonitorInfo>,
    pub(crate) turn_started_at: Option<Instant>,
    pub(crate) daemon_tx: std::cell::RefCell<Option<mpsc::UnboundedSender<DaemonCmd>>>,
}

pub(crate) struct SessionState {
    pub(crate) current: Session,
    pub(crate) perm_mode: PermissionMode,
    pub(crate) workspace_index: links::WorkspaceIndex,
    pub(crate) expanded_explore_groups: HashSet<u64>,
    pub(crate) transcript_cache: HashMap<(u64, usize, usize), (usize, Vec<LinkedLine>)>,
    /// While set, [`App::persist_session`] records one pending write.
    pub(crate) persist_hold: std::cell::Cell<bool>,
    pub(crate) persist_due: std::cell::Cell<bool>,
}

pub(crate) struct JobState {
    pub(crate) target: String,
    pub(crate) status: Option<Status>,
    pub(crate) watch: Option<watch::Receiver<Option<Status>>>,
    pub(crate) list: Vec<Job>,
    pub(crate) selected: usize,
    pub(crate) connected: bool,
    pub(crate) interval: Duration,
    pub(crate) last_refresh: Instant,
    pub(crate) shell_watches: Vec<ShellWatch>,
    pub(crate) last_shell_poll: Instant,
}

#[derive(Default)]
pub(crate) struct LoginState {
    pub(crate) rx: Option<mpsc::UnboundedReceiver<LoginEvent>>,
    pub(crate) task: Option<JoinHandle<()>>,
    pub(crate) cancel: Option<oneshot::Sender<()>>,
    pub(crate) models_rx:
        Option<oneshot::Receiver<(String, Vec<jade_agent::agent::context::ListedModel>)>>,
}

pub(crate) struct UiState {
    pub(crate) focus: Focus,
    pub(crate) mode: InputMode,
    pub(crate) show_sidebar: bool,
    pub(crate) follow: bool,
    pub(crate) transcript_scroll: usize,
    pub(crate) transcript_height: u16,
    pub(crate) transcript_max_scroll: usize,
    pub(crate) selected_card: Option<usize>,
    pub(crate) input: String,
    pub(crate) input_cursor: usize,
    pub(crate) pending_images: Vec<jade_agent::session::ImageAttachment>,
    pub(crate) history: Vec<String>,
    pub(crate) history_pos: Option<usize>,
    /// Unsent draft stashed while walking the send history.
    pub(crate) history_draft: Option<String>,
    pub(crate) menu_selected: usize,
    pub(crate) cd_navigated: bool,
    pub(crate) cd_dismissed: bool,
    pub(crate) at_dismissed_at: Option<usize>,
    pub(crate) modal: Option<Modal>,
    pub(crate) modal_rect: Option<Rect>,
    pub(crate) modal_cursor: usize,
    pub(crate) theme: usize,
    pub(crate) truecolor: bool,
    pub(crate) last_scroll_at: Option<Instant>,
    pub(crate) leader_at: Option<Instant>,
    /// Set by the first `Esc` of a double-`Esc` interrupt.
    pub(crate) interrupt_at: Option<Instant>,
    pub(crate) toast: Option<(String, bool, Instant)>,
    pub(crate) welcome: Option<Welcome>,
    pub(crate) inspector: InspectorState,
    pub(crate) hit_regions: Vec<HitRegion>,
    pub(crate) hover_target: Option<HitTarget>,
    pub(crate) hover_rect: Option<Rect>,
    pub(crate) transcript_area: Rect,
    pub(crate) mouse_capture: bool,
    pub(crate) selection: Option<MouseSelection>,
    pub(crate) drag_start: Option<GridPos>,
    pub(crate) screen_lines: Vec<Vec<String>>,
    /// Plan sidechat transcript. It is not part of the persisted session.
    pub(crate) side_open: bool,
    pub(crate) side_running: bool,
    pub(crate) side_items: Vec<TranscriptItem>,
    pub(crate) side_input: String,
    pub(crate) side_cursor: usize,
}
