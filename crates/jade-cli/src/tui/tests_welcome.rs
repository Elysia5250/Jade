use super::*;
use ratatui::backend::TestBackend;

fn tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = std::env::temp_dir().join(format!(
        "jade-home-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn select_control(app: &mut App, ctrl: HubControl) {
    let idx = HubControl::ALL.iter().position(|c| *c == ctrl).unwrap();
    let w = app.ui.welcome.as_mut().unwrap();
    w.focus = HubFocus::Controls;
    w.selected_control = idx;
}

fn render_text(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| paint(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

fn save_dummy_session(dir: &std::path::Path, cwd: &std::path::Path, title: &str) -> Session {
    let mut s = Session::new(cwd.to_path_buf(), "local".into());
    s.title = title.into();
    s.updated_ms = jade_core::now_ms();
    let id = s.alloc_id();
    s.items.push(TranscriptItem::User {
        id,
        text: format!("prompt for {title}"),
        mode: "Agent".into(),
        cwd: cwd.display().to_string(),
        ts_ms: s.updated_ms,
    });
    save_session(dir, &s).unwrap();
    s
}

#[test]
fn hub_renders_wide_two_column_and_narrow_single_column() {
    let dir = tmpdir("layout");
    save_dummy_session(&dir, &dir, "alpha task");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();

    let wide = render_text(&mut app, 124, 34);
    assert!(wide.contains("jade hub"), "{wide}");
    assert!(wide.contains("Quick Controls"), "{wide}");
    assert!(wide.contains("alpha task"), "{wide}");
    assert!(wide.contains("PREVIEW"), "{wide}");

    let narrow = render_text(&mut app, 80, 24);
    assert!(narrow.contains("jade hub"), "{narrow}");
    assert!(narrow.contains("[3] Controls"), "{narrow}");
    assert!(!narrow.contains("Quick Controls"), "{narrow}");

    // Switching to Controls tab on narrow screen shows Quick Controls
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let narrow_ctrl = render_text(&mut app, 80, 24);
    assert!(narrow_ctrl.contains("MODEL & REASONING"), "{narrow_ctrl}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn return_to_hub_via_home_and_esc_returns_without_resetting_session() {
    let dir = tmpdir("return");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    let orig_id = app.session.current.id.clone();

    // Ctrl+X h returns to Hub
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    assert!(app.ui.welcome.as_ref().is_some_and(|w| w.can_return));

    let screen = render_text(&mut app, 120, 30);
    assert!(screen.contains("[Esc] return to session"), "{screen}");

    // Esc closes Hub back to the same session
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.ui.welcome.is_none());
    assert_eq!(app.session.current.id, orig_id);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_daemon_sessions_pin_to_top_and_search_rename_delete_work() {
    let dir = tmpdir("multisess");
    let s1 = save_dummy_session(&dir, &dir, "alpha-idle");
    let s2 = save_dummy_session(&dir, &dir, "background job");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();

    app.apply_notice(UiNotice::LiveSessions(vec![
        jade_agent::runtime::SessionInfo {
            id: s2.id.clone(),
            cwd: dir.clone(),
            target: "local".into(),
            running: true,
            queued: 2,
        },
    ]));
    let w = app.ui.welcome.as_ref().unwrap();
    assert_eq!(w.sessions[0].id, s2.id);
    assert!(w.sessions[0].running);
    assert_eq!(w.sessions[0].queued, 2);

    let screen = render_text(&mut app, 120, 32);
    assert!(screen.contains("ACTIVE IN DAEMON"), "{screen}");
    assert!(screen.contains("working · q:2"), "{screen}");

    // Search filters to s1
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    for ch in "alpha".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.filtered_hub_sessions().len(), 1);
    assert_eq!(app.selected_hub_session().unwrap().id, s1.id);

    // Rename selected session
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    assert_eq!(
        app.ui.welcome.as_ref().unwrap().mode,
        WelcomeMode::RenameSession
    );
    app.ui.welcome.as_mut().unwrap().draft = "renamed-task".into();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.ui.welcome.as_mut().unwrap().search.clear();
    assert!(app
        .ui
        .welcome
        .as_ref()
        .unwrap()
        .sessions
        .iter()
        .any(|s| s.id == s1.id && s.title == "renamed-task"));

    // Two-step delete on idle s1
    let s1_idx = app
        .ui
        .welcome
        .as_ref()
        .unwrap()
        .sessions
        .iter()
        .position(|s| s.id == s1.id)
        .unwrap();
    app.ui.welcome.as_mut().unwrap().selected_session = s1_idx;
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert_eq!(
        app.ui.welcome.as_ref().unwrap().confirm_delete.as_deref(),
        Some(s1.id.as_str())
    );
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert!(!app
        .ui
        .welcome
        .as_ref()
        .unwrap()
        .sessions
        .iter()
        .any(|s| s.id == s1.id));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quick_controls_cycle_theme_effort_delegate_and_permissions() {
    let dir = tmpdir("controls");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    let orig_theme = app.ui.theme;

    select_control(&mut app, HubControl::Theme);
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(app.ui.theme, (orig_theme + 1) % THEMES.len());

    select_control(&mut app, HubControl::Delegate);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        jade_agent::agent::delegate_pref::load(&dir),
        jade_agent::agent::delegate_pref::DelegateLevel::Normal
    );

    select_control(&mut app, HubControl::Permission);
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(app.session.perm_mode, PermissionMode::AcceptEdits);

    select_control(&mut app, HubControl::Effort);
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(jade_agent::agent::effort::load(&dir), "high");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn new_folder_enters_the_created_directory() {
    let dir = tmpdir("mkdir");
    let parent = dir.join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    let mut app = App::new(dir.clone(), "local".into(), None, parent.clone());
    let launch = app.session.current.id.clone();
    app.open_welcome();
    app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE));
    assert_eq!(
        app.ui.welcome.as_ref().unwrap().mode,
        WelcomeMode::NewFolderPickParent
    );
    app.ui.welcome.as_mut().unwrap().path = parent.display().to_string();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.ui.welcome.as_ref().unwrap().mode,
        WelcomeMode::NewFolderName
    );
    for ch in "fresh".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.ui.welcome.is_none());
    assert_ne!(app.session.current.id, launch);
    assert!(app.session.current.items.is_empty());
    assert_eq!(
        std::fs::canonicalize(&app.session.current.cwd).unwrap(),
        std::fs::canonicalize(parent.join("fresh")).unwrap()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn protected_folder_name_stays_on_the_picker() {
    let dir = tmpdir("prot");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE));
    app.ui.welcome.as_mut().unwrap().path = dir.display().to_string();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.ui.welcome.as_mut().unwrap().draft = ".git".into();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let err = app.ui.welcome.as_ref().unwrap().error.clone().unwrap();
    assert!(err.contains("protected"), "{err}");
    assert!(!dir.join(".git").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn provider_control_opens_login_above_the_hub() {
    let dir = tmpdir("acct");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    select_control(&mut app, HubControl::Provider);
    let selected = app.ui.welcome.as_ref().unwrap().selected_control;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(app.ui.modal, Some(Modal::Login(_))));
    assert!(app.ui.welcome.is_some());
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(app.ui.welcome.as_ref().unwrap().selected_control, selected);
    app.handle_paste("/tmp/nope\n");
    assert!(app.ui.welcome.as_ref().unwrap().path.is_empty());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.ui.modal.is_none());
    assert!(app.ui.welcome.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn skill_toggle_writes_prefs_and_new_skill_returns_edit() {
    let dir = tmpdir("skill");
    let skill = dir.join("skills").join("demo");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: demo steps\n---\n\nbody\n",
    )
    .unwrap();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    select_control(&mut app, HubControl::Skills);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let idx = app
        .ui
        .welcome
        .as_ref()
        .unwrap()
        .skills
        .iter()
        .position(|item| item.name == "demo")
        .expect("demo skill");
    app.ui.welcome.as_mut().unwrap().list_selected = idx;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!jade_agent::skills::prefs::injected(&dir, "demo", true));
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    for ch in "extra".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match action {
        Action::EditFile(path) => {
            assert!(path.ends_with("SKILL.md"));
            assert!(std::fs::read_to_string(&path)
                .unwrap()
                .contains("name: extra"));
        }
        _ => panic!("expected EditFile"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn memory_create_edit_toggle_and_delete() {
    let dir = tmpdir("mem");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    select_control(&mut app, HubControl::Memory);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    for ch in "记事".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Action::EditFile(path) = action else {
        panic!("expected EditFile");
    };
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("title: 记事"));
    assert!(raw.contains("enabled: true"));
    assert_eq!(app.ui.welcome.as_ref().unwrap().mode, WelcomeMode::Memory);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.ui.welcome.as_ref().unwrap().notes[0].enabled);
    // Two-step delete confirmation for memory notes
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert_eq!(app.ui.welcome.as_ref().unwrap().notes.len(), 1);
    assert!(app.ui.welcome.as_ref().unwrap().confirm_delete.is_some());
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert!(app.ui.welcome.as_ref().unwrap().notes.is_empty());
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}
