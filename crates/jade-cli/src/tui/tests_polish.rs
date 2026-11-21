//! Regression tests for TUI UX polish (directory browser, hover, modals, safety, focus).
use super::*;
use ratatui::backend::TestBackend;

fn tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = std::env::temp_dir().join(format!(
        "jade-polish-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn draw_buf(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|frame| paint(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn save_test_session(data_dir: &std::path::Path, cwd: &std::path::Path, title: &str) -> Session {
    let mut s = Session::new(cwd.to_path_buf(), "local".into());
    s.title = title.into();
    s.updated_ms = now_ms();
    let id = s.alloc_id();
    s.items.push(TranscriptItem::User {
        id,
        text: format!("prompt for {title}"),
        mode: "Agent".into(),
        cwd: cwd.display().to_string(),
        ts_ms: s.updated_ms,
    });
    save_session(data_dir, &s).unwrap();
    s
}

#[tokio::test]
async fn path_browser_descends_navigates_up_and_lists_more_than_ten_dirs() {
    let dir = tmpdir("browser");
    for i in 0..14 {
        std::fs::create_dir_all(dir.join(format!("dir-{i:02}"))).unwrap();
    }
    let nested = dir.join("dir-12").join("deep-child");
    std::fs::create_dir_all(&nested).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    // Stale path should be cleared on `o`
    app.ui.welcome.as_mut().unwrap().path = "stale/path".into();
    app.handle_key(key(KeyCode::Char('o')));
    assert!(app.ui.welcome.as_ref().unwrap().path.is_empty());

    let entries = state::welcome::build_path_browser_entries(&dir, "");
    // 1 OpenCurrent + 1 Parent + 14 subdirs = 16 (not truncated at 10!)
    assert_eq!(entries.len(), 16);

    // Move to dir-12 (index 2 + 12 = 14) and press Enter -> descends, does NOT open yet
    for _ in 0..14 {
        app.handle_key(key(KeyCode::Down));
    }
    app.handle_key(key(KeyCode::Enter));
    assert!(app.ui.welcome.is_some());
    assert_eq!(
        std::fs::canonicalize(&app.ui.welcome.as_ref().unwrap().browse_dir).unwrap(),
        std::fs::canonicalize(dir.join("dir-12")).unwrap()
    );

    // Descend into deep-child (index 2: OpenCurrent, Parent, deep-child/)
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert!(app.ui.welcome.is_some());
    assert_eq!(
        std::fs::canonicalize(&app.ui.welcome.as_ref().unwrap().browse_dir).unwrap(),
        std::fs::canonicalize(&nested).unwrap()
    );

    // Empty Backspace goes up to dir-12 and highlights deep-child
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(
        std::fs::canonicalize(&app.ui.welcome.as_ref().unwrap().browse_dir).unwrap(),
        std::fs::canonicalize(dir.join("dir-12")).unwrap()
    );
    assert_eq!(app.ui.welcome.as_ref().unwrap().path_selected, 2);

    // Re-enter with Right, then Enter on row 0 (OpenCurrent) opens deep-child
    app.handle_key(key(KeyCode::Right));
    app.handle_key(key(KeyCode::Enter));
    assert!(app.ui.welcome.is_none());
    assert_eq!(
        std::fs::canonicalize(&app.session.current.cwd).unwrap(),
        std::fs::canonicalize(&nested).unwrap()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cd_trailing_slash_enters_typed_dir_and_esc_keeps_input() {
    let dir = tmpdir("cd");
    let sub = dir.join("crates");
    let child = sub.join("alpha");
    std::fs::create_dir_all(&child).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.input = "/cd cr".into();
    app.ui.input_cursor = app.ui.input.chars().count();

    // Tab completes "/cd crates/"
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.ui.input, "/cd crates/");

    // Esc dismisses completion menu without wiping input
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.ui.input, "/cd crates/");
    assert!(app.cd_completions().is_empty());

    // Re-trigger completion and press Enter directly: must enter crates/, NOT crates/alpha/
    app.ui.cd_dismissed = false;
    assert!(!app.cd_completions().is_empty());
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        std::fs::canonicalize(&app.session.current.cwd).unwrap(),
        std::fs::canonicalize(&sub).unwrap()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mouse_hover_highlights_hit_regions_and_modals_support_click_scroll_and_dismiss() {
    let dir = tmpdir("hover");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    let _ = draw_buf(&mut app, 110, 30);

    // Find top-bar "hub" hit region and move mouse over it
    let hub_hr = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::OpenHub)
        .expect("hub hit region")
        .rect;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: hub_hr.x,
        row: hub_hr.y,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.ui.hover_target, Some(HitTarget::OpenHub));
    let buf = draw_buf(&mut app, 110, 30);
    assert_eq!(buf[(hub_hr.x, hub_hr.y)].bg, app.theme().hover);

    // Open theme picker modal: wheel scrolls, click selects, outside click dismisses
    app.open_theme_picker();
    let _ = draw_buf(&mut app, 110, 30);
    let modal_rect = app.ui.modal_rect.expect("modal rect");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: modal_rect.x + 2,
        row: modal_rect.y + 2,
        modifiers: KeyModifiers::NONE,
    });
    if let Some(Modal::Picker(p)) = &app.ui.modal {
        assert_eq!(p.selected, 1);
    } else {
        panic!("expected picker modal");
    }

    // Click outside modal_rect dismisses modal
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert!(app.ui.modal.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hub_subpanel_scroll_does_not_scroll_background_and_narrow_controls_tabs_click() {
    let dir = tmpdir("hubmouse");
    for name in ["s1", "s2"] {
        save_test_session(&dir, &dir, name);
    }
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    app.handle_key(key(KeyCode::Char('o')));
    let _ = draw_buf(&mut app, 110, 30);
    assert_eq!(app.ui.welcome.as_ref().unwrap().selected_session, 0);
    assert_eq!(app.ui.welcome.as_ref().unwrap().path_selected, 0);

    // Scrolling in OpenPath moves path_selected, NOT selected_session
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 40,
        row: 15,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.ui.welcome.as_ref().unwrap().path_selected, 1);
    assert_eq!(app.ui.welcome.as_ref().unwrap().selected_session, 0);

    // Esc back to Hub, narrow layout Controls tab still has clickable [1] Sessions hit region
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Char('3')));
    let _ = draw_buf(&mut app, 80, 24);
    let tab1 = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::HubSelectTab(HubTab::Sessions))
        .expect("narrow [1] Sessions hit region")
        .rect;
    app.dispatch_click(tab1.x, tab1.y);
    assert_eq!(app.ui.welcome.as_ref().unwrap().focus, HubFocus::Left);
    assert_eq!(app.ui.welcome.as_ref().unwrap().tab, HubTab::Sessions);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn safety_guards_no_bare_q_quit_esc_keeps_draft_and_sidebar_job_scope() {
    let dir = tmpdir("safety");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());

    // Esc in Input preserves draft and moves focus to Transcript
    app.ui.input = "important draft".into();
    app.ui.input_cursor = app.ui.input.chars().count();
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.ui.input, "important draft");
    assert_eq!(app.ui.focus, Focus::Transcript);

    // Bare `q` in Transcript does NOT quit
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('q'))),
        Action::None
    ));

    // Sidebar on Diff tab: `c` and `Enter` must NOT cancel/attach jobs
    app.jobs.list.push(Job {
        id: 42,
        spec: TaskSpec::Shell {
            command: "sleep 10".into(),
        },
        cwd: dir.display().to_string(),
        label: None,
        group: DEFAULT_GROUP.into(),
        created_ms: now_ms(),
        owner: String::new(),
        state: ProtoJobState::Running { since_ms: now_ms() },
        pid: Some(1234),
        log_bytes: 0,
    });
    app.ui.focus = Focus::Sidebar;
    app.ui.inspector.tab = InspectorTab::Diff;
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('c'))),
        Action::None
    ));
    assert!(matches!(app.handle_key(key(KeyCode::Enter)), Action::None));

    // Overview tab: j/k scrolls instead of locking on job selection, [ / ] steps jobs
    app.ui.inspector.tab = InspectorTab::Overview;
    app.ui.inspector.max_scroll = 20;
    app.ui.inspector.scroll = 0;
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.ui.inspector.scroll, 2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sessions_modal_two_step_delete_and_ctrl_c_closes_modal() {
    let dir = tmpdir("sessmodal");
    let other = save_test_session(&dir, &dir, "old task");

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_sessions_modal();

    // First `d` arms confirm_delete without deleting
    app.handle_key(key(KeyCode::Char('d')));
    let Modal::Sessions(s) = app.ui.modal.as_ref().unwrap() else {
        panic!("expected sessions modal");
    };
    assert_eq!(s.confirm_delete.as_deref(), Some(other.id.as_str()));
    assert_eq!(s.sessions.len(), 1);

    // Esc cancels confirmation without closing modal
    app.handle_key(key(KeyCode::Esc));
    assert!(app.ui.modal.is_some());

    // Two `d` presses delete
    app.handle_key(key(KeyCode::Char('d')));
    app.handle_key(key(KeyCode::Char('d')));
    let Modal::Sessions(s) = app.ui.modal.as_ref().unwrap() else {
        panic!("expected sessions modal");
    };
    assert!(s.sessions.is_empty());

    // Ctrl+C closes modal cleanly
    app.handle_key(ctrl('c'));
    assert!(app.ui.modal.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hub_trust_and_catalog_match_selected_hub_cwd_and_workspaces_search_stays_on_tab() {
    let dir = tmpdir("hubcwd");
    let ws_a = dir.join("ws-a");
    let ws_b = dir.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();

    let _s_b = save_test_session(&dir, &ws_b, "session in b");

    let mut app = App::new(dir.clone(), "local".into(), None, ws_a.clone());
    app.open_welcome();
    inputs::sync(&mut app);
    // Selected session is s_b (in ws_b), while app.session.current.cwd is ws_a
    assert_eq!(
        std::fs::canonicalize(app.selected_hub_cwd()).unwrap(),
        std::fs::canonicalize(&ws_b).unwrap()
    );
    assert!(!app.inputs.catalog.trusted);

    // Toggle Trust in Hub Controls -> must update ws_b AND reflect in catalog.trusted
    let idx = HubControl::ALL
        .iter()
        .position(|c| *c == HubControl::Trust)
        .unwrap();
    app.ui.welcome.as_mut().unwrap().focus = HubFocus::Controls;
    app.ui.welcome.as_mut().unwrap().selected_control = idx;
    app.handle_key(key(KeyCode::Enter));
    inputs::sync(&mut app);
    assert!(is_workspace_trusted(&dir, &ws_b));
    assert!(app.inputs.catalog.trusted);

    // Switch to Workspaces tab and press '/' -> stays on Workspaces tab and filters
    app.handle_key(key(KeyCode::Char('2')));
    app.handle_key(key(KeyCode::Char('/')));
    for ch in "ws-b".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.ui.welcome.as_ref().unwrap().tab, HubTab::Workspaces);
    assert_eq!(app.filtered_hub_workspaces().len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sidechat_refocus_paste_and_esc_preserve_draft() {
    let dir = tmpdir("side");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_side_pane();
    app.ui.side_input = "plan note".into();
    app.ui.side_cursor = app.ui.side_input.chars().count();

    // Paste while in Focus::Side inserts into side_input (not main input)
    app.handle_paste(" extra");
    assert_eq!(app.ui.side_input, "plan note extra");
    assert!(app.ui.input.is_empty());

    // Esc returns to Focus::Input WITHOUT wiping side_input
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.ui.focus, Focus::Input);
    assert_eq!(app.ui.side_input, "plan note extra");

    // Clicking Sidechat input hit region regains Focus::Side
    let _ = draw_buf(&mut app, 120, 30);
    let side_hit = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::FocusSideInput)
        .expect("side input hit region")
        .rect;
    app.dispatch_click(side_hit.x, side_hit.y);
    assert_eq!(app.ui.focus, Focus::Side);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unsigned_empty_enter_opens_login_and_login_esc_steps_back() {
    let dir = tmpdir("login");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.inputs.catalog.signed_in = false;

    // Empty Enter opens login modal matching editor placeholder
    app.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        app.ui.modal,
        Some(Modal::Login(LoginModal::SelectProvider { .. }))
    ));

    // Find a multi-method provider (e.g., index 0 = OpenAI/Anthropic)
    let multi_idx = BUILTIN_PROVIDERS
        .iter()
        .position(|p| p.methods.len() > 1)
        .expect("at least one multi-method provider");
    app.ui.modal = Some(Modal::Login(LoginModal::ApiKeyForm {
        provider_idx: multi_idx,
        key: String::new(),
        base_url: "https://example.com".into(),
        model: String::new(),
        focus_field: 1,
        reveal: false,
        error: None,
    }));
    app.ui.modal_cursor = "https://example.com".chars().count();
    // Ctrl+U clears the prefilled base_url cleanly
    app.handle_key(ctrl('u'));
    if let Some(Modal::Login(LoginModal::ApiKeyForm { base_url, .. })) = &app.ui.modal {
        assert!(base_url.is_empty());
    } else {
        panic!("expected ApiKeyForm");
    }

    // Esc steps back: ApiKeyForm -> SelectMethod -> SelectProvider -> None
    app.handle_key(key(KeyCode::Esc));
    assert!(matches!(
        app.ui.modal,
        Some(Modal::Login(LoginModal::SelectMethod { .. }))
    ));
    app.handle_key(key(KeyCode::Esc));
    assert!(matches!(
        app.ui.modal,
        Some(Modal::Login(LoginModal::SelectProvider { .. }))
    ));
    app.handle_key(key(KeyCode::Esc));
    assert!(app.ui.modal.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ask_supports_ctrl_c_double_esc_jk_and_number_keys() {
    let dir = tmpdir("ask");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    let make_ask = || PendingAsk {
        id: 7,
        question: "Proceed?".into(),
        options: vec![
            AskOption {
                label: "Option A".into(),
                description: None,
                recommended: true,
            },
            AskOption {
                label: "Option B".into(),
                description: None,
                recommended: false,
            },
        ],
        recommended: 0,
        selected: 0,
        deadline: Instant::now() + Duration::from_secs(60),
        touched: false,
        paused_left: None,
    };

    // Ctrl+C interrupts instead of being swallowed
    app.agent.pending_ask = Some(make_ask());
    assert!(matches!(app.handle_key(ctrl('c')), Action::Cancel));

    // Esc arms double-Esc interrupt instead of silently picking recommended
    app.agent.pending_ask = Some(make_ask());
    assert!(matches!(app.handle_key(key(KeyCode::Esc)), Action::None));
    assert!(app.agent.pending_ask.is_some());
    assert!(matches!(app.handle_key(key(KeyCode::Esc)), Action::Cancel));

    // j/k navigation and number key `2` picks Option B
    app.agent.pending_ask = Some(make_ask());
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.agent.pending_ask.as_ref().unwrap().selected, 1);
    app.handle_key(key(KeyCode::Char('2')));
    assert!(app.agent.pending_ask.is_none());
    let (toast, _, _) = app.ui.toast.as_ref().unwrap();
    assert!(toast.contains("Option B"), "{toast}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn double_esc_and_ctrl_c_interrupt_shell_subagent_sidechat_and_outbox() {
    let dir = tmpdir("esc-work");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());

    // 1. Running shell watch: Esc arms + cancels instead of toggling focus; Ctrl+C cancels instead of quitting.
    app.jobs.shell_watches.push(ShellWatch {
        item_id: 1,
        job_id: 7,
        cursor: 0,
        partial_out: Vec::new(),
        partial_err: Vec::new(),
        started: Instant::now(),
    });
    assert!(matches!(app.handle_key(key(KeyCode::Esc)), Action::None));
    assert_eq!(
        app.ui.focus,
        Focus::Input,
        "must not toggle focus while shell runs"
    );
    assert!(matches!(app.handle_key(key(KeyCode::Esc)), Action::Cancel));
    assert!(matches!(app.handle_key(ctrl('c')), Action::Cancel));
    app.jobs.shell_watches.clear();

    // 2. Running subagent even if main agent.running is false.
    app.session.current.subagents.insert(
        "call-1".into(),
        vec![SubagentLane {
            index: 0,
            name: "explore".into(),
            description: "scan".into(),
            status: CardStatus::Running,
            items: Vec::new(),
        }],
    );
    assert!(matches!(app.handle_key(key(KeyCode::Esc)), Action::None));
    assert_eq!(app.ui.focus, Focus::Input);
    assert!(matches!(app.handle_key(key(KeyCode::Esc)), Action::Cancel));
    app.session.current.subagents.clear();

    // 3. Focus::Side while a turn is running arms + interrupts rather than bouncing focus.
    app.open_side_pane();
    app.agent.running = true;
    assert_eq!(app.ui.focus, Focus::Side);
    assert!(matches!(app.handle_key(key(KeyCode::Esc)), Action::None));
    assert_eq!(app.ui.focus, Focus::Side);
    assert!(matches!(app.handle_key(key(KeyCode::Esc)), Action::Cancel));
    app.agent.running = false;
    app.ui.focus = Focus::Input;

    // 4. SnapshotEnd with running=false while an outbox prompt is waiting to flush keeps running=true.
    app.agent
        .outbox
        .push_back(("queued prompt".into(), Vec::new(), SendMode::Prompt));
    app.drain_agent_batch(Some(AgentResp::SnapshotStart {
        session_id: app.session.current.id.clone(),
        running: false,
        incarnation_id: 1,
        seq: 0,
        total_items: 0,
    }));
    app.drain_agent_batch(Some(AgentResp::SnapshotEnd));
    assert!(
        app.agent.running,
        "outbox prompt flush must keep running armed"
    );
    assert!(matches!(app.handle_key(key(KeyCode::Esc)), Action::None));
    assert_eq!(app.ui.focus, Focus::Input);
    assert!(matches!(app.handle_key(key(KeyCode::Esc)), Action::Cancel));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn macos_friendly_history_leader_and_super_modifier_guards() {
    let dir = tmpdir("macos-keys");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.history = vec!["first".into(), "second".into()];

    // Empty composer: plain Up/Down walks history without needing Ctrl+Up (stolen by Mission Control).
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.ui.input, "second");
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.ui.input, "first");
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.ui.input, "second");
    // Editing recalled entry leaves history mode.
    app.handle_key(key(KeyCode::Char('!')));
    assert_eq!(app.ui.input, "second!");
    assert_eq!(app.ui.history_pos, None);

    // SUPER/META modifiers must not insert stray characters in composer or line_edit.
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER));
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::META));
    assert_eq!(app.ui.input, "second!");
    let mut line = "abc".to_string();
    let mut cur = 3;
    assert_eq!(
        line_edit::handle_key(
            &mut line,
            &mut cur,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::SUPER)
        ),
        line_edit::EditOutcome::Ignored
    );
    assert_eq!(line, "abc");

    // Ctrl+X leader fallbacks for Alt+1..5 and Alt+M.
    app.handle_key(ctrl('x'));
    app.handle_key(key(KeyCode::Char('2')));
    assert!(app.ui.show_sidebar);
    assert_eq!(app.ui.inspector.tab, InspectorTab::Diff);

    app.handle_key(ctrl('x'));
    let toggle = app.handle_key(key(KeyCode::Char('M')));
    assert!(matches!(toggle, Action::ToggleMouseCapture));

    let _ = std::fs::remove_dir_all(&dir);
}
