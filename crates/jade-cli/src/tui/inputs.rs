//! Memory snapshots the console paints from.
//!
//! `view/` reads these fields only. Disk, `git`, and provider catalogs are
//! refreshed here, on a path change or a short TTL, never inside `view::draw`.

use super::*;
use std::io::Read;
use std::path::{Path, PathBuf};

pub(crate) const FILE_PREVIEW_LIMIT: usize = 512 * 1024;
const CATALOG_TTL: Duration = Duration::from_secs(2);
const GIT_DIRTY_TTL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TodoMark {
    Pending,
    InProgress,
    Completed,
}

pub(crate) struct TodoSnap {
    pub(crate) content: String,
    pub(crate) mark: TodoMark,
}

pub(crate) struct McpSnap {
    pub(crate) name: String,
    pub(crate) scope: String,
    pub(crate) disabled: bool,
}

pub(crate) struct SkillSnap {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) scope: String,
}

pub(crate) struct SkillFlag {
    pub(crate) kind: &'static str,
    pub(crate) injected: bool,
}

#[derive(Default)]
pub(crate) struct Catalog {
    pub(crate) signed_in: bool,
    pub(crate) connected_providers: HashMap<String, &'static str>,
    pub(crate) provider_id: String,
    pub(crate) model_id: String,
    pub(crate) compact_at: u64,
    pub(crate) hard_window: u64,
    pub(crate) tier: String,
    pub(crate) effort_level: String,
    pub(crate) effort_options: Vec<String>,
    pub(crate) delegate_level: jade_agent::agent::delegate_pref::DelegateLevel,
    pub(crate) sandbox_label: String,
    pub(crate) bwrap_present: bool,
    pub(crate) trusted: bool,
    pub(crate) daemon_online: bool,
    pub(crate) git_branch: Option<String>,
    pub(crate) git_worktree: Option<String>,
    pub(crate) git_dirty: bool,
    pub(crate) mcp: Vec<McpSnap>,
    pub(crate) skills: Vec<SkillSnap>,
    pub(crate) todos: Vec<TodoSnap>,
    pub(crate) welcome_flags: Vec<SkillFlag>,
}

pub(crate) enum PreviewBody {
    Absent,
    Loading { rel: String },
    Missing { rel: String },
    Binary { rel: String, bytes: u64 },
    Text { rel: String, body: String },
}

struct RemoteFill {
    target: String,
    path: String,
    bytes: Vec<u8>,
    total: u64,
    missing: bool,
}

pub(crate) struct Inputs {
    pub(crate) catalog: Catalog,
    pub(crate) live_sessions: Vec<jade_agent::runtime::SessionInfo>,
    pub(crate) cd_comps: Vec<(String, String)>,
    pub(crate) welcome_path_comps: Vec<(String, String)>,
    pub(crate) preview: PreviewBody,
    pub(crate) truncated: bool,
    pub(crate) has_diff: bool,
    pub(crate) diff_rel: String,
    pub(crate) diff_git: Option<(String, String)>,
    catalog_key: String,
    catalog_at: Option<Instant>,
    git_dirty_cache: Option<(PathBuf, bool, Instant)>,
    cd_key: Option<(PathBuf, String)>,
    welcome_path_key: Option<(PathBuf, String)>,
    preview_key: String,
    remote_want: Option<String>,
    remote_fill: Option<RemoteFill>,
}

impl Default for Inputs {
    fn default() -> Self {
        Self {
            catalog: Catalog::default(),
            live_sessions: Vec::new(),
            cd_comps: Vec::new(),
            welcome_path_comps: Vec::new(),
            preview: PreviewBody::Absent,
            truncated: false,
            has_diff: false,
            diff_rel: String::new(),
            diff_git: None,
            catalog_key: String::new(),
            catalog_at: None,
            git_dirty_cache: None,
            cd_key: None,
            welcome_path_key: None,
            preview_key: String::new(),
            remote_want: None,
            remote_fill: None,
        }
    }
}

impl Inputs {
    pub(crate) fn invalidate_catalog(&mut self) {
        self.catalog_key.clear();
        self.catalog_at = None;
        self.git_dirty_cache = None;
    }

    pub(crate) fn preview_key_clear(&mut self) {
        self.preview_key.clear();
    }

    pub(crate) fn clear_for_target_switch(&mut self) {
        self.remote_want = None;
        self.remote_fill = None;
        self.preview_key.clear();
        self.cd_key = None;
        self.welcome_path_key = None;
        self.git_dirty_cache = None;
        self.invalidate_catalog();
    }
}

pub(crate) fn sync(app: &mut App) {
    let cwd = app.session.current.cwd.clone();
    app.session.workspace_index.ensure_fresh(&cwd);
    sync_completions(app, &cwd);
    if catalog_due(app) {
        fill_catalog(app);
    }
    refresh_media(app);
}

fn sync_completions(app: &mut App, cwd: &Path) {
    if let Some(raw) = app.cd_query().map(str::to_string) {
        let want = (cwd.to_path_buf(), raw.clone());
        if app.inputs.cd_key.as_ref() != Some(&want) {
            app.inputs.cd_comps = complete_directories(cwd, &raw);
            app.inputs.cd_key = Some(want);
        }
    } else if app.inputs.cd_key.take().is_some() {
        app.inputs.cd_comps.clear();
    }
    if let Some(welcome) = app.ui.welcome.as_ref().filter(|w| w.mode.is_path_input()) {
        let want = (cwd.to_path_buf(), welcome.path.clone());
        if app.inputs.welcome_path_key.as_ref() != Some(&want) {
            app.inputs.welcome_path_comps = complete_directories(cwd, &welcome.path);
            app.inputs.welcome_path_key = Some(want);
        }
    } else if app.inputs.welcome_path_key.take().is_some() {
        app.inputs.welcome_path_comps.clear();
    }
}

pub(crate) fn remote_pending(app: &App) -> bool {
    match &app.inputs.remote_want {
        Some(path) => !app
            .inputs
            .remote_fill
            .as_ref()
            .is_some_and(|fill| fill.target == app.jobs.target && fill.path == *path),
        None => false,
    }
}

pub(crate) async fn pull_remote(app: &mut App, remote: &Remote<AnyDialer>) -> bool {
    let Some(path) = app.inputs.remote_want.clone() else {
        return false;
    };
    if app
        .inputs
        .remote_fill
        .as_ref()
        .is_some_and(|fill| fill.target == app.jobs.target && fill.path == path)
    {
        return false;
    }
    let target = app.jobs.target.clone();
    let limit = FILE_PREVIEW_LIMIT as u64;
    let filled = match remote.read(&target, &path, 0, limit).await {
        Ok((total, _, bytes)) => RemoteFill {
            target: target.clone(),
            path,
            bytes,
            total,
            missing: false,
        },
        Err(_) => RemoteFill {
            target: target.clone(),
            path,
            bytes: Vec::new(),
            total: 0,
            missing: true,
        },
    };
    if app.jobs.target != target {
        return false;
    }
    app.inputs.remote_fill = Some(filled);
    app.inputs.preview_key.clear();
    true
}

fn catalog_due(app: &App) -> bool {
    let key = catalog_key(app);
    if key != app.inputs.catalog_key {
        return true;
    }
    app.inputs
        .catalog_at
        .map(|at| at.elapsed() >= CATALOG_TTL)
        .unwrap_or(true)
}

fn catalog_key(app: &App) -> String {
    format!(
        "{}|{}|{}|{}",
        app.data_dir.display(),
        app.session.current.cwd.display(),
        app.session.current.id,
        app.jobs.target
    )
}

fn fill_catalog(app: &mut App) {
    let key = catalog_key(app);
    let backend = jade_core::sandbox::probe_backend();
    let git = if target_is_local(&app.jobs.target) {
        git_info::probe_git_info(
            &app.session.current.cwd,
            &mut app.inputs.git_dirty_cache,
            GIT_DIRTY_TTL,
        )
    } else {
        git_info::GitInfo::default()
    };
    let mut catalog = Catalog {
        connected_providers: effective_providers(&app.data_dir)
            .into_iter()
            .map(|(id, sp)| (id, sp.credential.kind_label()))
            .collect(),
        effort_level: jade_agent::agent::effort::load(&app.data_dir),
        delegate_level: jade_agent::agent::delegate_pref::load(&app.data_dir),
        sandbox_label: jade_core::sandbox::backend_label().to_string(),
        bwrap_present: matches!(backend, jade_core::sandbox::SandboxBackend::Bwrap(_)),
        trusted: is_workspace_trusted(&app.data_dir, &app.session.current.cwd),
        daemon_online: daemon::socket_path(&app.data_dir).exists(),
        git_branch: git.branch,
        git_worktree: git.worktree,
        git_dirty: git.dirty,
        ..Catalog::default()
    };
    if let Some((prov, model)) = resolve_active_provider(&app.data_dir) {
        let budget =
            jade_agent::agent::context::resolve_budget(&app.data_dir, &prov.id, &model.model_id);
        let mut opts = Vec::new();
        if let Some(spec) =
            jade_agent::agent::effort::advertised(&app.data_dir, &prov.id, &model.model_id)
        {
            opts = spec.efforts.clone();
            if spec.toggle
                && !opts.iter().any(|l| l == "off" || l == "none")
                && jade_agent::agent::effort::accepts(&spec, prov.protocol, "off")
            {
                opts.push("off".to_string());
            }
        }
        catalog.signed_in = true;
        catalog.provider_id = prov.id;
        catalog.model_id = model.model_id;
        catalog.compact_at = budget.compact_at;
        catalog.hard_window = budget.hard_window;
        catalog.tier = budget.tier.to_string();
        catalog.effort_options = opts;
    }
    let mcp = jade_agent::mcp::config::load_mcp_configs(&app.data_dir, &app.session.current.cwd);
    catalog.mcp = mcp
        .servers
        .iter()
        .map(|srv| McpSnap {
            name: srv.name.clone(),
            scope: srv.scope.label().to_string(),
            disabled: srv.disabled,
        })
        .collect();
    let skills = jade_agent::skills::discover_skills(&app.data_dir, &app.session.current.cwd);
    catalog.skills = skills
        .iter()
        .map(|skill| SkillSnap {
            name: skill.name.clone(),
            description: skill.description.clone(),
            scope: skill.scope.label().to_string(),
        })
        .collect();
    catalog.todos =
        jade_agent::agent::tools::todo::load_todos(&app.data_dir, &app.session.current.id)
            .into_iter()
            .map(|item| TodoSnap {
                content: item.content,
                mark: match item.status {
                    jade_agent::agent::tools::todo::TodoStatus::Pending => TodoMark::Pending,
                    jade_agent::agent::tools::todo::TodoStatus::InProgress => TodoMark::InProgress,
                    jade_agent::agent::tools::todo::TodoStatus::Completed => TodoMark::Completed,
                },
            })
            .collect();
    if let Some(welcome) = &app.ui.welcome {
        catalog.welcome_flags = welcome
            .skills
            .iter()
            .map(|skill| SkillFlag {
                kind: jade_agent::skills::skill_root_kind(&app.data_dir, skill),
                injected: jade_agent::skills::skill_is_injected(
                    &app.data_dir,
                    &app.session.current.cwd,
                    skill,
                ),
            })
            .collect();
    }
    app.inputs.catalog = catalog;
    app.inputs.catalog_key = key;
    app.inputs.catalog_at = Some(Instant::now());
}

fn refresh_media(app: &mut App) {
    let Some(raw) = active_raw(app) else {
        return;
    };
    let key = media_key(app, &raw);
    if key == app.inputs.preview_key {
        return;
    }
    load_media(app, &raw, key);
}

fn active_raw(app: &App) -> Option<String> {
    if !matches!(
        app.ui.inspector.tab,
        InspectorTab::File | InspectorTab::Diff
    ) {
        return None;
    }
    if let Some(path) = &app.ui.inspector.file_path {
        return Some(path.clone());
    }
    app.session
        .current
        .changed_files
        .get(app.ui.inspector.selected_diff)
        .or_else(|| app.session.current.changed_files.last())
        .map(|fc| fc.path.clone())
}

fn media_key(app: &App, raw: &str) -> String {
    let stamp = app
        .find_changed_file_index(raw)
        .and_then(|idx| app.session.current.changed_files.get(idx))
        .map(|fc| fc.before.len() + fc.after.len() + fc.added + fc.removed)
        .unwrap_or(0);
    format!("{}|{}|{}", app.jobs.target, raw, stamp)
}

fn load_media(app: &mut App, raw: &str, key: String) {
    let local = target_is_local(&app.jobs.target);
    let abs = absolute_path(&app.session.current.cwd, raw);
    let rel = if local {
        links::normalize_display_path(&app.session.current.cwd, &abs)
    } else {
        links::display_rel(&app.session.current.cwd, Path::new(raw))
    };
    let session_diff =
        app.find_changed_file_index(&rel).is_some() || app.find_changed_file_index(raw).is_some();
    let git = if local && !session_diff {
        diff::load_git_pair(&app.session.current.cwd, &rel)
    } else {
        None
    };
    let has_diff = session_diff || git.is_some();
    let truncated = if local {
        file_len(&abs)
            .map(|len| len > FILE_PREVIEW_LIMIT as u64)
            .unwrap_or(false)
    } else {
        let want = abs.to_string_lossy();
        app.inputs.remote_fill.as_ref().is_some_and(|fill| {
            fill.target == app.jobs.target
                && fill.path == want
                && fill.total > FILE_PREVIEW_LIMIT as u64
        })
    };
    let preview = if local {
        local_preview(&abs, rel.clone())
    } else {
        remote_preview(app, &abs, rel.clone())
    };
    app.inputs.diff_rel = rel;
    app.inputs.diff_git = git;
    app.inputs.has_diff = has_diff;
    app.inputs.truncated = truncated;
    app.inputs.remote_want = if local {
        None
    } else {
        Some(abs.to_string_lossy().into_owned())
    };
    app.inputs.preview = preview;
    app.inputs.preview_key = key;
}

fn local_preview(abs: &Path, rel: String) -> PreviewBody {
    let Some((bytes, total)) = read_prefix(abs) else {
        return PreviewBody::Missing { rel };
    };
    body_from_bytes(rel, &bytes, total, false)
}

fn remote_preview(app: &App, abs: &Path, rel: String) -> PreviewBody {
    let want = abs.to_string_lossy();
    let Some(fill) = &app.inputs.remote_fill else {
        return PreviewBody::Loading { rel };
    };
    if fill.target != app.jobs.target || fill.path != want {
        return PreviewBody::Loading { rel };
    }
    body_from_bytes(rel, &fill.bytes, fill.total, fill.missing)
}

fn body_from_bytes(rel: String, bytes: &[u8], total: u64, missing: bool) -> PreviewBody {
    if missing {
        return PreviewBody::Missing { rel };
    }
    if bytes.iter().take(4096).any(|&byte| byte == 0) {
        return PreviewBody::Binary {
            rel,
            bytes: total.max(bytes.len() as u64),
        };
    }
    PreviewBody::Text {
        rel,
        body: String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn read_prefix(path: &Path) -> Option<(Vec<u8>, u64)> {
    let file = std::fs::File::open(path).ok()?;
    let total = file.metadata().ok().map(|meta| meta.len()).unwrap_or(0);
    let mut limited = file.take(FILE_PREVIEW_LIMIT as u64);
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf).ok()?;
    Some((buf, total))
}

fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

fn absolute_path(cwd: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

pub(crate) fn target_is_local(target: &str) -> bool {
    target.is_empty() || target == "local"
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn tmpdir() -> PathBuf {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "jade-in-{:02x}{:02x}{:02x}{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn oversized_file_preview_keeps_a_prefix() {
        let dir = tmpdir();
        let path = dir.join("big.txt");
        let mut data = vec![b'a'; FILE_PREVIEW_LIMIT + 64 * 1024];
        data[0] = b'Z';
        std::fs::write(&path, &data).unwrap();

        let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
        app.ui.show_sidebar = true;
        app.ui.inspector.force_open = true;
        app.ui.inspector.tab = InspectorTab::File;
        app.ui.inspector.file_path = Some(path.display().to_string());
        sync(&mut app);

        let PreviewBody::Text { body, .. } = &app.inputs.preview else {
            panic!("expected a text prefix");
        };
        assert!(app.inputs.truncated);
        assert!(body.len() <= FILE_PREVIEW_LIMIT);
        assert!(body.starts_with('Z'));

        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|frame| paint(frame, &mut app)).unwrap();
        let painted: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            painted.contains('Z'),
            "filled preview missing from the frame"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn idle_selection_does_not_keep_screen_cells() {
        let dir = tmpdir();
        let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal.draw(|frame| paint(frame, &mut app)).unwrap();
        assert!(app.ui.screen_lines.is_empty());

        app.ui.selection = Some(MouseSelection {
            start: GridPos { col: 0, row: 0 },
            end: GridPos { col: 3, row: 0 },
            active: true,
        });
        terminal.draw(|frame| paint(frame, &mut app)).unwrap();
        assert!(!app.ui.screen_lines.is_empty());

        if let Some(sel) = app.ui.selection.as_mut() {
            sel.active = false;
        }
        terminal.draw(|frame| paint(frame, &mut app)).unwrap();
        assert!(app.ui.screen_lines.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
