use super::*;
use envoy_agent::session::OutputLine;

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

fn job(id: JobId, state: ProtoJobState) -> Job {
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
    let mut app = App::new(
        dir.clone(),
        "local".into(),
        None,
        PathBuf::from("/tmp/project"),
    );
    app.jobs.connected = true;
    assert_eq!(app.ui.focus, Focus::Input);

    let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();

    for needle in ["/tmp/project", "agent", "/login to sign in"] {
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
    use envoy_agent::auth::{record_provider_login, Credential, ProviderProtocol, StoredProvider};
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
            fetched_models: Vec::new(),
            model_routes: std::collections::BTreeMap::new(),
            updated_ms: now_ms(),
        },
        Some("claude-sonnet-4-5".into()),
    )
    .unwrap();

    let mut app = App::new(
        dir.clone(),
        "local".into(),
        None,
        PathBuf::from("/tmp/project"),
    );
    app.jobs.connected = true;
    app.session.current.prompt_tokens = 50_000;
    assert_eq!(app.ui.mode, InputMode::Agent);

    let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
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
    app.ui.input = "/log".into();
    app.ui.input_cursor = 4;

    let names: Vec<&str> = app.filtered_slash().iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["login", "logout", "logs"]);

    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.ui.input, "/login");

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        app.ui.modal,
        Some(Modal::Login(LoginModal::SelectProvider { .. }))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn login_api_key_flow_persists_and_enables_agent_mode() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    assert_eq!(app.ui.mode, InputMode::Agent);

    app.run_slash("/login");
    // Filter to deepseek
    for ch in "deepseek".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        app.ui.modal,
        Some(Modal::Login(LoginModal::ApiKeyForm { .. }))
    ));

    for ch in "sk-test-key".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(app.ui.modal.is_none());
    assert_eq!(app.ui.mode, InputMode::Agent);
    let (prov, model) = resolve_active_provider(&dir).unwrap();
    assert_eq!(prov.id, "deepseek");
    assert_eq!(
        model.model_id,
        envoy_agent::model_registry::default_env_model("deepseek")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cd_directory_autocomplete_lists_subdirs_and_switches() {
    let dir = tmpdir();
    let sub = dir.join("alpha-proj");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(dir.join("beta-proj")).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.input = "/cd al".into();
    app.ui.input_cursor = app.ui.input.chars().count();

    let comps = app.cd_completions();
    assert_eq!(comps, vec![("alpha-proj/".into(), "alpha-proj/".into())]);

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.session.current.cwd, std::fs::canonicalize(&sub).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multiline_shift_enter_inserts_newline_and_bang_runs_shell() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.mode = InputMode::Agent;

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    assert_eq!(app.ui.input, "a\nb");

    app.ui.input = "!echo hi".into();
    app.ui.input_cursor = app.ui.input.chars().count();
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
    app.jobs.connected = true;
    app.ui.show_sidebar = true;
    app.jobs.list = vec![job(
        1,
        ProtoJobState::Done {
            at_ms: now_ms(),
            result: envoy_proto::JobResult::Success,
        },
    )];
    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "check files".into(),
        mode: "Agent".into(),
        cwd: "/tmp".into(),
        ts_ms: now_ms(),
    });
    let tid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Tool {
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
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
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
        app.session.current.items[1],
        TranscriptItem::Tool {
            collapsed: true,
            ..
        }
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
    assert!(matches!(app.ui.modal, Some(Modal::Picker(_))));
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
        fetched_models: vec![],
        model_routes: std::collections::BTreeMap::new(),
        updated_ms: 0,
    };
    record_provider_login(&dir, prov, Some("gemini-4-hydrogen".into())).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.jobs.connected = true;
    app.session.current.prompt_tokens = 50_000;
    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "visible item".into(),
        mode: "Agent".into(),
        cwd: "/tmp".into(),
        ts_ms: now_ms(),
    });

    let draw_text = |app: &mut App| -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal.draw(|frame| paint(frame, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let width = buf.area.width;
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect();
        if let Some(status) = rows.iter().find(|row| row.contains("~1.0m")) {
            assert!(
                status.contains("agent"),
                "status row clipped the left side:\n{status}"
            );
            assert_eq!(
                display_width(status),
                width as usize,
                "status row:\n{status}"
            );
        }
        rows.concat()
    };

    let first = draw_text(&mut app);
    assert!(first.contains("50k/512k~1.0m"), "status bar:\n{first}");

    app.run_slash("/budget focused");
    let second = draw_text(&mut app);
    assert!(second.contains("50k/200k~1.0m"), "status bar:\n{second}");

    let old_id = app.session.current.id.clone();
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
            
                reasoning_blocks: Vec::new(),}],
            before_tokens: 50_000,
            after_tokens: 4_000,
            summary: "ok".into(),
            auto: true,
        },
    );
    assert_eq!(app.session.current.messages.len(), 1);
    assert!(app
        .session.current
        .items
        .iter()
        .any(|i| matches!(i, TranscriptItem::User { .. })));

    app.apply_event(
        2,
        AgentEvent::HandoffReady {
            summary: "## Goal\nfinish".into(),
            before_tokens: 50_000,
            after_tokens: 3_000,
            title: "finish".into(),
        },
    );
    assert_ne!(app.session.current.id, old_id);
    assert_eq!(app.session.current.messages.len(), 2);
    assert!(app.session.current.messages[0]
        .content
        .contains("<handoff_context>"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn permission_mode_cycles_on_shift_tab_and_renders_status_badge() {
    use ratatui::backend::TestBackend;
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    assert_eq!(app.session.perm_mode, PermissionMode::Default);

    app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(app.session.perm_mode, PermissionMode::AcceptEdits);

    app.run_slash("/plan");
    assert_eq!(app.session.perm_mode, PermissionMode::Plan);

    let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
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

#[test]
fn slash_choices_open_a_picker_instead_of_cycling() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    assert_eq!(app.session.perm_mode, PermissionMode::Default);
    assert_eq!(app.ui.mode, InputMode::Agent);

    app.run_slash("/permission");
    {
        let Some(Modal::Picker(picker)) = app.ui.modal.as_ref() else {
            panic!("expected a permission picker");
        };
        assert_eq!(picker.title, "permissions");
        assert_eq!(picker.selected, 0);
        assert!(picker
            .items
            .iter()
            .any(|(label, _)| label.contains("accept-edits")));
    }
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.ui.modal.is_none());
    assert_eq!(app.session.perm_mode, PermissionMode::AcceptEdits);

    app.run_slash("/permissions plan");
    assert!(app.ui.modal.is_none());
    assert_eq!(app.session.perm_mode, PermissionMode::Plan);

    app.run_slash("/mode");
    {
        let Some(Modal::Picker(picker)) = app.ui.modal.as_ref() else {
            panic!("expected a mode picker");
        };
        assert_eq!(picker.selected, 0);
    }
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.ui.mode, InputMode::Shell);

    app.run_slash("/theme");
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.ui.theme, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn welcome_picker_selects_a_recent_workspace() {
    use ratatui::backend::TestBackend;
    let dir = tmpdir();
    let here = dir.join("here");
    let other = dir.join("other");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let mut saved = Session::new(other.clone(), "local".into());
    saved.title = "other workspace".into();
    saved.updated_ms = now_ms();
    let item_id = saved.alloc_id();
    saved.items.push(TranscriptItem::User {
        id: item_id,
        text: "hello".into(),
        mode: "Agent".into(),
        cwd: other.display().to_string(),
        ts_ms: now_ms(),
    });
    save_session(&dir, &saved).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, here.clone());
    app.open_welcome();
    assert_eq!(app.ui.welcome.as_ref().unwrap().rows.len(), 7);
    assert_eq!(app.ui.welcome.as_ref().unwrap().selected, 0);

    let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("Pick a workspace"), "{text}");
    assert!(text.contains("open a path"), "{text}");

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.ui.welcome.as_ref().unwrap().mode, WelcomeMode::OpenPath);
    for ch in dir.join("missing").display().to_string().chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let err = app
        .ui.welcome
        .as_ref()
        .unwrap()
        .error
        .clone()
        .unwrap_or_default();
    assert!(err.contains("not a directory"), "{err}");

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    assert_eq!(app.ui.welcome.as_ref().unwrap().selected, 1);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.ui.welcome.is_none());
    assert_eq!(
        std::fs::canonicalize(&app.session.current.cwd).unwrap(),
        std::fs::canonicalize(&other).unwrap()
    );
    assert_eq!(app.session.current.id, saved.id);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn welcome_without_a_saved_session_mints_a_new_id() {
    let dir = tmpdir();
    let here = dir.join("here");
    let fresh = dir.join("fresh");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&fresh).unwrap();
    let mut app = App::new(dir.clone(), "local".into(), None, here.clone());
    let launch_id = app.session.current.id.clone();
    app.open_welcome();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.ui.welcome.is_none());
    assert_ne!(app.session.current.id, launch_id);
    assert_eq!(
        std::fs::canonicalize(&app.session.current.cwd).unwrap(),
        std::fs::canonicalize(&here).unwrap()
    );

    let picked = app.session.current.id.clone();
    app.open_welcome();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    for ch in fresh.display().to_string().chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.ui.welcome.is_none());
    assert_ne!(app.session.current.id, picked);
    assert_eq!(
        std::fs::canonicalize(&app.session.current.cwd).unwrap(),
        std::fs::canonicalize(&fresh).unwrap()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn composer_and_turns_keep_vertical_room() {
    use ratatui::backend::TestBackend;
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, PathBuf::from("/tmp"));
    app.jobs.connected = true;
    app.ui.show_sidebar = false;

    for (user, reply) in [
        ("first-question", "alpha-reply"),
        ("second-question", "beta-reply"),
    ] {
        let uid = app.session.current.alloc_id();
        app.session.current.items.push(TranscriptItem::User {
            id: uid,
            text: user.into(),
            mode: "Agent".into(),
            cwd: "/tmp".into(),
            ts_ms: now_ms(),
        });
        let aid = app.session.current.alloc_id();
        app.session.current.items.push(TranscriptItem::Assistant {
            id: aid,
            text: reply.into(),
            reasoning: String::new(),
            reasoning_collapsed: true,
            model: "m".into(),
            done: true,
            error: None,
        });
    }

    let width = 80u16;
    let height = 36u16;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..height)
        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
        .collect();
    let row_of = |needle: &str| -> usize {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle}\n{}", rows.join("\n")))
    };
    let blanks_between = |from: usize, to: usize| -> usize {
        rows[from + 1..to]
            .iter()
            .filter(|row| row.trim().is_empty())
            .count()
    };

    let first = row_of("first-question");
    let alpha = row_of("alpha-reply");
    let second = row_of("second-question");
    let within = blanks_between(first, alpha);
    let between = blanks_between(alpha, second);
    assert!(
        between >= 2 && between > within,
        "turn gap {between} should exceed the in-turn gap {within}\n{}",
        rows.join("\n")
    );

    let top = rows
        .iter()
        .position(|row| row.contains('╭'))
        .expect("composer top");
    let bottom = rows
        .iter()
        .rposition(|row| row.contains('╰'))
        .expect("composer bottom");
    assert!(
        top >= 2 && rows[top - 1].trim().is_empty() && rows[top - 2].trim().is_empty(),
        "composer should have two blank rows above it\n{}",
        rows.join("\n")
    );
    assert!(
        rows[bottom + 1].trim().is_empty() && rows[bottom + 2].contains("agent"),
        "composer should sit one row above the status line\n{}",
        rows.join("\n")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
