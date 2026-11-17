use super::*;
use crate::session::OutputLine;

fn tmpdir() -> PathBuf {
    let mut b = [0u8; 6];
    let _ = getrandom::fill(&mut b);
    let p = std::env::temp_dir().join(format!(
        "envoy-tui-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn job(id: JobId, state: JobState) -> Job {
    Job {
        id,
        owner: "local".into(),
        spec: TaskSpec::Shell {
            command: format!("echo {id}"),
        },
        cwd: "/tmp".into(),
        label: None,
        group: DEFAULT_GROUP.into(),
        created_ms: now_ms(),
        state,
        pid: None,
        log_bytes: 0,
    }
}

#[test]
fn opens_with_input_focused_and_welcome_screen() {
    use ratatui::backend::TestBackend;
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, PathBuf::from("/tmp/project"));
    app.connected = true;
    assert_eq!(app.focus, Focus::Input);

    let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
    terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();

    for needle in ["/tmp/project", "shell", "sign in"] {
        assert!(text.contains(needle), "missing {needle:?}:\n{text}");
    }
    assert!(
        !text.contains("ctrl+p"),
        "shortcut crib should stay off the screen:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn status_line_shows_model_workspace_and_context() {
    use crate::auth::{record_provider_login, Credential, ProviderProtocol, StoredProvider};
    use ratatui::backend::TestBackend;

    let dir = tmpdir();
    record_provider_login(
        &dir,
        StoredProvider {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            protocol: ProviderProtocol::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            credential: Credential::ApiKey {
                key: "sk-test".into(),
            },
            custom_models: Vec::new(),
            updated_ms: now_ms(),
        },
        Some("claude-sonnet-4-5".into()),
    )
    .unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, PathBuf::from("/tmp/project"));
    app.connected = true;
    app.session.prompt_tokens = 50_000;
    assert_eq!(app.mode, InputMode::Agent);

    let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
    terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    for needle in ["/tmp/project", "agent", "claude-sonnet-4-5", "50k/160k"] {
        assert!(text.contains(needle), "missing {needle:?}:\n{text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn slash_menu_filters_completes_and_opens_login() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.input = "/log".into();
    app.input_cursor = 4;

    let names: Vec<&str> = app.filtered_slash().iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["login", "logout"]);

    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.input, "/login");

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(app.modal, Some(Modal::Login(LoginModal::SelectProvider { .. }))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn login_api_key_flow_persists_and_enables_agent_mode() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    assert_eq!(app.mode, InputMode::Shell);

    app.run_slash("/login");
    // Filter to deepseek
    for ch in "deepseek".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        app.modal,
        Some(Modal::Login(LoginModal::ApiKeyForm { .. }))
    ));

    for ch in "sk-test-key".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(app.modal.is_none());
    assert_eq!(app.mode, InputMode::Agent);
    let (prov, model) = resolve_active_provider(&dir).unwrap();
    assert_eq!(prov.id, "deepseek");
    assert_eq!(model.model_id, "deepseek-v4.1-flash");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cd_directory_autocomplete_lists_subdirs_and_switches() {
    let dir = tmpdir();
    let sub = dir.join("alpha-proj");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(dir.join("beta-proj")).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.input = "/cd al".into();
    app.input_cursor = app.input.chars().count();

    let comps = app.cd_completions();
    assert_eq!(comps, vec![("alpha-proj/".into(), "alpha-proj/".into())]);

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.session.cwd, std::fs::canonicalize(&sub).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multiline_shift_enter_inserts_newline_and_bang_runs_shell() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.mode = InputMode::Agent;

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    assert_eq!(app.input, "a\nb");

    app.input = "!echo hi".into();
    app.input_cursor = app.input.chars().count();
    match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
        Action::SubmitShell(req) => assert_eq!(req.spec.display(), "echo hi"),
        _ => panic!("!cmd should always submit shell even in Agent mode"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn renders_transcript_cards_and_sidebar() {
    use ratatui::backend::TestBackend;
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, PathBuf::from("/tmp"));
    app.connected = true;
    app.show_sidebar = true;
    app.jobs = vec![job(
        1,
        JobState::Done {
            at_ms: now_ms(),
            result: envoy_proto::JobResult::Success,
        },
    )];
    let uid = app.session.alloc_id();
    app.session.items.push(TranscriptItem::User {
        id: uid,
        text: "check files".into(),
        mode: "Agent".into(),
        cwd: "/tmp".into(),
        ts_ms: now_ms(),
    });
    let tid = app.session.alloc_id();
    app.session.items.push(TranscriptItem::Tool {
        id: tid,
        call_id: "c1".into(),
        name: "exec".into(),
        title: "$ ls -la".into(),
        subtitle: "/tmp".into(),
        job_id: Some(1),
        target: "local".into(),
        output: vec![OutputLine {
            text: "Cargo.toml".into(),
            err: false,
        }],
        status: CardStatus::Done,
        collapsed: false,
        duration_ms: Some(1200),
        exit_code: Some(0),
        started_ms: now_ms(),
    });

    let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
    terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();

    for needle in ["check files", "$ ls -la", "Cargo.toml", "jobs", "echo 1"] {
        assert!(text.contains(needle), "missing {needle:?}:\n{text}");
    }

    // ctrl+o collapses tool output
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(matches!(
        app.session.items[1],
        TranscriptItem::Tool { collapsed: true, .. }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn leader_requires_prefix_and_palette_filters() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    assert!(matches!(
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
        Action::None
    ));
    assert!(matches!(
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        Action::Quit
    ));

    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(matches!(app.modal, Some(Modal::Picker(_))));
    for ch in "quit".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert!(matches!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Quit
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn context_budget_compacted_and_handoff_render_cleanly() {
    use ratatui::backend::TestBackend;
    let dir = tmpdir();
    let prov = StoredProvider {
        id: "google".into(),
        name: "Google".into(),
        protocol: ProviderProtocol::OpenAiChat,
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai".into(),
        credential: Credential::ApiKey { key: "k".into() },
        custom_models: vec![],
        updated_ms: 0,
    };
    record_provider_login(&dir, prov, Some("gemini-4-hydrogen".into())).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.connected = true;
    app.session.prompt_tokens = 50_000;
    let uid = app.session.alloc_id();
    app.session.items.push(TranscriptItem::User {
        id: uid,
        text: "visible item".into(),
        mode: "Agent".into(),
        cwd: "/tmp".into(),
        ts_ms: now_ms(),
    });

    let draw_text = |app: &mut App| -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal.draw(|frame| view::draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    };

    let first = draw_text(&mut app);
    assert!(first.contains("50k/512k·1.0m"), "status bar:\n{first}");

    app.run_slash("/budget focused");
    let second = draw_text(&mut app);
    assert!(second.contains("50k/200k·1.0m"), "status bar:\n{second}");

    let old_id = app.session.id.clone();
    app.apply_event(
        1,
        AgentEvent::Compacted {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "<compacted_context>ok</compacted_context>".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                tool_name: None,
            }],
            before_tokens: 50_000,
            after_tokens: 4_000,
            summary: "ok".into(),
            auto: true,
        },
    );
    assert_eq!(app.session.messages.len(), 1);
    assert!(app.session.items.iter().any(|i| matches!(i, TranscriptItem::User { .. })));

    app.apply_event(
        2,
        AgentEvent::HandoffReady {
            summary: "## Goal\nfinish".into(),
            before_tokens: 50_000,
            after_tokens: 3_000,
            title: "finish".into(),
        },
    );
    assert_ne!(app.session.id, old_id);
    assert_eq!(app.session.messages.len(), 2);
    assert!(app.session.messages[0].content.contains("<handoff_context>"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn permission_mode_cycles_on_shift_tab_and_renders_status_badge() {
    use ratatui::backend::TestBackend;
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    assert_eq!(app.perm_mode, PermissionMode::Default);

    app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(app.perm_mode, PermissionMode::AcceptEdits);

    app.run_slash("/plan");
    assert_eq!(app.perm_mode, PermissionMode::Plan);

    let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
    terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("perm:plan"), "status bar:\n{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
