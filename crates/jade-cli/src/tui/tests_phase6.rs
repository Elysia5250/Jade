//! Regression tests for P6-1 audit fixes and Phase 6 hardening.

use super::*;
use ratatui::backend::TestBackend;

fn tmpdir(tag: &str) -> PathBuf {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "jade-p6-{tag}-{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn symlink_loop_indexes_each_inode_once() {
    let dir = tmpdir("idx");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/a.txt"), "hi").unwrap();
    std::os::unix::fs::symlink(dir.join("sub/a.txt"), dir.join("sub/b.txt")).unwrap();
    std::os::unix::fs::symlink(&dir, dir.join("sub/loop")).unwrap();
    let mut index = links::WorkspaceIndex::default();
    let started = Instant::now();
    index.ensure_fresh(&dir);
    assert!(started.elapsed() < Duration::from_secs(2));
    let files = index.search_files("", 20);
    let copies = files
        .iter()
        .filter(|path| path.ends_with("a.txt") || path.ends_with("b.txt"))
        .count();
    assert_eq!(copies, 1, "{files:?}");
    let (resolved, _) = index
        .resolve_path_cached(&dir, &[], &files[0])
        .expect("cached resolution hits indexed file without stat");
    assert_eq!(resolved, files[0]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn workspace_tree_shows_on_open_and_hides_gitignored_files() {
    let dir = tmpdir("tree");
    let init = std::process::Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(init.success(), "git init");
    std::fs::write(dir.join(".gitignore"), "secret.log\n").unwrap();
    std::fs::write(dir.join("visible.rs"), "fn visible() {}\n").unwrap();
    std::fs::write(dir.join("secret.log"), "nope").unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.show_sidebar = true;
    app.ui.inspector.force_open = true;
    app.ui.inspector.tab = InspectorTab::Overview;

    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let painted: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        painted.contains("visible.rs"),
        "workspace tree should list visible.rs:\n{painted}"
    );
    assert!(
        !painted.contains("secret.log"),
        "gitignored files stay out of the tree:\n{painted}"
    );
    assert!(app.session.workspace_index.tree().contains("visible.rs"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn truncated_preview_renders_banner_in_file_inspector() {
    let dir = tmpdir("trunc");
    let path = dir.join("huge.txt");
    let data = vec![b'x'; inputs::FILE_PREVIEW_LIMIT + 1024];
    std::fs::write(&path, &data).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.show_sidebar = true;
    app.ui.inspector.force_open = true;
    app.ui.inspector.tab = InspectorTab::File;
    app.ui.inspector.file_path = Some(path.display().to_string());

    let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let painted: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(app.inputs.truncated);
    assert!(
        painted.contains("truncated"),
        "expected truncation banner in file inspector:\n{painted}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn p6_2a_terminal_restore_clears_all_flags_in_reverse_order_and_reports_editor_exit() {
    let mut seq = Vec::new();
    let disable_called = std::cell::Cell::new(false);
    let mask = terminal::MODE_RAW
        | terminal::MODE_ALT
        | terminal::MODE_MOUSE
        | terminal::MODE_PASTE
        | terminal::MODE_ENHANCED;
    terminal::restore_to(&mut seq, mask, &|| {
        disable_called.set(true);
        Ok(())
    });
    assert!(disable_called.get());
    let text = String::from_utf8_lossy(&seq);
    assert!(!text.is_empty());

    let ok_status = std::process::Command::new("true").status();
    assert!(edit_file::finish_status("true", ok_status).is_ok());

    let fail_status = std::process::Command::new("false").status();
    let err = edit_file::finish_status("false", fail_status).unwrap_err();
    assert!(err.contains("exited with"), "{err}");
}

#[test]
fn p6_2b_large_scroll_does_not_wrap_and_zero_geometry_does_not_panic() {
    let dir = tmpdir("p62b");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.follow = false;
    app.ui.transcript_scroll = 70_000;
    assert_eq!(app.ui.transcript_scroll, 70_000usize);

    app.session.current.items.push(TranscriptItem::User {
        id: 1,
        text: (0..70_050)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
        mode: "Agent".into(),
        cwd: dir.display().to_string(),
        ts_ms: 0,
    });
    let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
    term.draw(|frame| paint(frame, &mut app)).unwrap();
    assert!(
        app.ui.transcript_max_scroll > 70_000,
        "max_scroll={}",
        app.ui.transcript_max_scroll
    );
    assert_eq!(app.ui.transcript_scroll, 70_000);

    for (w, h) in [(0u16, 0u16), (0, 20), (20, 0), (1, 1)] {
        let mut tiny = Terminal::new(TestBackend::new(w, h)).unwrap();
        tiny.draw(|frame| paint(frame, &mut app)).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn p6_2c_input_semantics_ctrl_u_picker_jk_and_combining_width() {
    let dir = tmpdir("p62c");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.focus = Focus::Input;
    app.ui.input = "hello world".into();
    app.ui.input_cursor = 11;
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(app.ui.input.is_empty(), "Ctrl+U in Input must clear text");

    let items = app.command_items();
    app.open_picker("commands", items);
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    let Some(Modal::Picker(p)) = app.ui.modal.as_ref() else {
        panic!("picker missing");
    };
    assert_eq!(p.query, "jk");

    assert_eq!(char_width('\u{0301}'), 0);
    assert_eq!(
        "e\u{0301}".chars().map(char_width).sum::<usize>(),
        display_width("e\u{0301}")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn p6_2d_session_isolation_notices_rewind_shell_delete_guard_handoff_and_cancel() {
    let dir = tmpdir("p62d");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    let old_id = app.session.current.id.clone();

    // 1. Stale notice dropped; current session notice applied.
    app.apply_notice(UiNotice::Notice {
        session_id: "other-session".into(),
        msg: "stale".into(),
        err: false,
    });
    assert!(app.session.current.items.is_empty());
    app.apply_notice(UiNotice::Toast {
        session_id: Some("other-session".into()),
        msg: "stale toast".into(),
        err: false,
    });
    assert!(app.ui.toast.is_none());

    // 2. Active session deletion is refused in /sessions modal.
    app.session.current.items.push(TranscriptItem::User {
        id: 1,
        text: "keep me".into(),
        mode: "Agent".into(),
        cwd: dir.display().to_string(),
        ts_ms: 1,
    });
    app.flush_session_now();
    app.open_sessions_modal();
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert!(
        load_session(&dir, &old_id).is_ok(),
        "active session must not be deleted"
    );
    app.ui.modal = None;

    // 3. Rewind ignores `Shell` cards when counting remaining user turns.
    app.session.current.items.push(TranscriptItem::User {
        id: 2,
        text: "$ pwd".into(),
        mode: "Shell".into(),
        cwd: dir.display().to_string(),
        ts_ms: 2,
    });
    app.session.current.items.push(TranscriptItem::User {
        id: 3,
        text: "second prompt".into(),
        mode: "Agent".into(),
        cwd: dir.display().to_string(),
        ts_ms: 3,
    });
    app.apply_checkpoint_rewound(1, "cp1".into(), vec![], 1, vec![], vec![], vec![]);
    let kept: Vec<_> = app
        .session
        .current
        .items
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(kept, vec!["keep me", "$ pwd"]);

    // 4. `cancel_active` leaves `agent_running = true` until daemon TurnError/TurnDone arrives.
    app.agent.running = true;
    let remote = Remote::new(dialer_for(&dir, "local", None));
    app.cancel_active(&remote).await;
    assert!(
        app.agent.running,
        "daemon remains authoritative until TurnError"
    );
    app.apply_event(99, AgentEvent::TurnError("interrupted".into()));
    assert!(!app.agent.running);

    // 5. `HandoffReady` inside `drain_agent_batch` flushes old session before switching.
    let (_tx, rx) = mpsc::unbounded_channel();
    app.agent.stream = Some(rx);
    app.drain_agent_batch(Some(AgentResp::Event(SeqEvent {
        seq: 100,
        event: AgentEvent::HandoffReady {
            summary: "handed off".into(),
            before_tokens: 1000,
            after_tokens: 100,
            title: "handoff: test".into(),
        },
    })));
    assert_ne!(app.session.current.id, old_id);
    let old_saved = load_session(&dir, &old_id).expect("old session flushed");
    assert!(!old_saved.items.is_empty());

    // 6. `NewSession` clears inspector, expanded groups, and transcript cache.
    app.session.expanded_explore_groups.insert(42);
    app.ui.inspector.file_path = Some("foo.rs".into());
    app.session.transcript_cache.insert((1, 40, 0), (0, vec![]));
    app.run_command(Command::NewSession);
    assert!(app.session.expanded_explore_groups.is_empty());
    assert!(app.session.transcript_cache.is_empty());
    assert!(app.ui.inspector.file_path.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn p6_2e_bound_port_short_modal_screens_and_mouse_actions() {
    let dir = tmpdir("p62e");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());

    // 1. OpenAiBrowserBound updates OAuthBrowser redirect_uri and authorize URL to bound port.
    app.ui.modal = Some(Modal::Login(LoginModal::OAuthBrowser {
        provider_name: "ChatGPT".into(),
        url: "http://auth.openai.com?redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
            .into(),
        redirect_uri: "http://localhost:1455/auth/callback".into(),
        verifier: "ver".into(),
        pasted: String::new(),
        status: "waiting".into(),
        busy: false,
        error: None,
    }));
    let (tx, rx) = mpsc::unbounded_channel();
    app.login.rx = Some(rx);
    tx.send(LoginEvent::OpenAiBrowserBound {
        url: "http://auth.openai.com?redirect_uri=http%3A%2F%2Flocalhost%3A49152%2Fauth%2Fcallback"
            .into(),
        redirect_uri: "http://localhost:49152/auth/callback".into(),
    })
    .unwrap();
    app.drain_login_events();
    let Some(Modal::Login(LoginModal::OAuthBrowser {
        url, redirect_uri, ..
    })) = &app.ui.modal
    else {
        panic!("expected OAuthBrowser");
    };
    assert_eq!(redirect_uri, "http://localhost:49152/auth/callback");
    assert!(url.contains("49152"), "{url}");

    // 2. Every modal renders safely on short/narrow screens (30x6 and 10x3).
    let modals: Vec<Modal> = vec![
        Modal::Picker(Picker {
            title: "commands".into(),
            query: String::new(),
            items: vec![("help".into(), Command::FocusInput)],
            selected: 0,
        }),
        Modal::Models(ModelsModal {
            filter: String::new(),
            selected: 0,
            models: Vec::new(),
            fetching: false,
        }),
        Modal::Sessions(SessionsModal {
            sessions: Vec::new(),
            selected: 0,
        }),
        Modal::Rewind(RewindModal {
            checkpoints: Vec::new(),
            selected: 0,
        }),
        Modal::Login(LoginModal::ApiKeyForm {
            provider_idx: 0,
            key: String::new(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5".into(),
            focus_field: 0,
            reveal: false,
            error: None,
        }),
        Modal::Login(LoginModal::CustomForm {
            protocol_idx: 0,
            name: "Custom".into(),
            base_url: "http://127.0.0.1:8080".into(),
            key: String::new(),
            model: "m".into(),
            focus_field: 0,
            error: None,
        }),
        Modal::Login(LoginModal::OAuthBrowser {
            provider_name: "ChatGPT".into(),
            url: "https://auth.openai.com/authorize?redirect_uri=49152".into(),
            redirect_uri: "http://localhost:49152/auth/callback".into(),
            verifier: "v".into(),
            pasted: String::new(),
            status: "waiting".into(),
            busy: false,
            error: None,
        }),
        Modal::Login(LoginModal::OAuthCodePaste {
            provider_name: "Claude".into(),
            url: "https://claude.ai/oauth/authorize".into(),
            verifier: "v".into(),
            state: "s".into(),
            console: false,
            code_input: String::new(),
            busy: false,
            error: None,
        }),
        Modal::Login(LoginModal::OAuthDevice {
            provider_name: "GitHub".into(),
            verification_uri: "https://github.com/login/device".into(),
            user_code: "ABCD-1234".into(),
            status: "waiting".into(),
            error: None,
        }),
        Modal::Login(LoginModal::SelectProvider {
            filter: String::new(),
            selected: 0,
        }),
        Modal::Login(LoginModal::SelectMethod {
            provider_idx: 0,
            selected: 0,
        }),
    ];
    for modal in modals {
        app.ui.modal = Some(modal);
        for (w, h) in [(30u16, 6u16), (10, 3)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|frame| paint(frame, &mut app)).unwrap();
        }
    }
    app.ui.modal = None;

    // 3. Mouse hit-region dispatches non-AttachJob actions cleanly and status bar clips on 10x3.
    app.ui.hit_regions.push(HitRegion {
        rect: Rect {
            x: 0,
            y: 0,
            width: 5,
            height: 1,
        },
        target: HitTarget::CycleMode,
    });
    let action = app.dispatch_click(1, 0).expect("CycleMode emits Action");
    assert!(!matches!(action, Action::AttachJob(_)));
    let mut remote = Remote::new(dialer_for(&dir, "local", None));
    assert!(
        !drive::dispatch_action_for_test(&mut app, &mut remote, action)
            .await
            .unwrap()
    );
    assert_eq!(app.ui.mode, InputMode::Shell);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn p6_3a_welcome_mode_and_single_source_slash_commands() {
    let dir = tmpdir("p63a");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());

    for (prefix, want) in [("job", "jobs"), ("log", "logs"), ("out", "output")] {
        app.ui.input = format!("/{prefix}");
        app.ui.input_cursor = app.ui.input.chars().count();
        assert!(
            app.filtered_slash().iter().any(|c| c.name == want),
            "expected /{want} in completion for {prefix}"
        );
    }

    let _ = app.run_slash("/jobs");
    assert_eq!(app.ui.inspector.tab, InspectorTab::Overview);
    assert_eq!(app.ui.focus, Focus::Sidebar);

    let _ = app.run_slash("/output");
    assert_eq!(app.ui.inspector.tab, InspectorTab::Output);
    let _ = app.run_slash("/logs");
    assert_eq!(app.ui.inspector.tab, InspectorTab::Output);

    let _ = app.run_slash("/definitely_unknown_cmd");
    let (msg, is_err, _) = app
        .ui
        .toast
        .as_ref()
        .expect("unknown command sets error toast");
    assert!(*is_err && msg.contains("unknown command"), "{msg}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn status_bar_preserves_toast_when_inspector_is_open() {
    let dir = tmpdir("status-toast");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.show_sidebar = true;
    app.inputs.catalog.signed_in = true;
    app.inputs.catalog.model_id = "claude-sonnet-4-5".into();
    app.inputs.catalog.compact_at = 200_000;
    app.inputs.catalog.hard_window = 200_000;
    app.inputs.catalog.sandbox_label = "bwrap".into();
    app.inputs.catalog.bwrap_present = true;

    for ch in "/clear".chars() {
        let _ = app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let _ = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.ui.input.is_empty());

    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    let mut screen = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            screen.push_str(buf[(x, y)].symbol());
        }
        screen.push('\n');
    }
    assert!(
        screen.contains("transcript cleared"),
        "expected toast in narrow status bar:\n{screen}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
