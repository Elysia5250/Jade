//! Plan sidechat keys and transcript notes.
use super::*;
use ratatui::backend::TestBackend;

fn tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = std::env::temp_dir().join(format!(
        "envoy-side-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn type_prompt(app: &mut App, text: &str) {
    app.ui.input = text.to_string();
    app.ui.input_cursor = text.chars().count();
}

#[test]
fn enter_while_running_opens_side_and_ctrl_enter_steers() {
    let dir = tmpdir("keys");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.agent.running = true;
    type_prompt(&mut app, "plan this");

    assert_eq!(app.send_mode_for_submit(), SendMode::Side);
    let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(action, Action::SubmitAgent(p) if p == "plan this"));

    type_prompt(&mut app, "fix the name");
    let steer = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
    assert!(matches!(steer, Action::SubmitAgentSteer(p) if p == "fix the name"));

    app.agent.running = false;
    assert_eq!(app.send_mode_for_submit(), SendMode::Prompt);

    app.ui.focus = Focus::Side;
    assert_eq!(app.send_mode_for_submit(), SendMode::SideTalk);
    app.ui.side_input = "keep talking".into();
    app.ui.side_cursor = app.ui.side_input.chars().count();
    let side = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(side, Action::SubmitAgent(p) if p == "keep talking"));

    app.ui.side_input.clear();
    app.ui.side_cursor = 0;
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.ui.focus, Focus::Input);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn enter_during_an_ask_answers_it() {
    let dir = tmpdir("ask");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.agent.running = true;
    app.ui.focus = Focus::Side;
    type_prompt(&mut app, "should not send");
    app.agent.pending_ask = Some(PendingAsk {
        id: 3,
        question: "计划已写好。要立刻打断正在跑的工人吗？".into(),
        options: vec![
            AskOption {
                label: "立刻打断".into(),
                description: None,
                recommended: false,
            },
            AskOption {
                label: "等这轮结束".into(),
                description: None,
                recommended: true,
            },
        ],
        recommended: 1,
        selected: 1,
        deadline: Instant::now() + Duration::from_secs(30),
        touched: false,
        paused_left: None,
    });

    let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(action, Action::None));
    assert!(app.agent.pending_ask.is_none());
    assert_eq!(app.ui.input, "should not send");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn side_notes_open_and_close_the_pane() {
    let dir = tmpdir("notes");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.apply_side_note(SideNote::User {
        text: "plan this".into(),
    });
    assert!(app.ui.side_open);
    assert!(app.ui.side_running);
    assert_eq!(app.ui.inspector.tab, InspectorTab::Side);
    assert!(matches!(
        app.ui.side_items.first(),
        Some(TranscriptItem::User { text, .. }) if text == "plan this"
    ));

    app.apply_side_note(SideNote::Closed);
    assert!(!app.ui.side_open);
    assert!(app.ui.side_items.is_empty());
    assert_eq!(app.ui.inspector.tab, InspectorTab::Overview);
    assert_eq!(app.ui.focus, Focus::Input);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn side_pane_draws_the_plan_composer() {
    let dir = tmpdir("draw");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.show_sidebar = true;
    app.ui.inspector.force_open = true;
    app.apply_side_note(SideNote::Say {
        text: "writing the plan".into(),
    });
    app.ui.focus = Focus::Side;
    app.ui.side_input = "next".into();
    app.ui.side_cursor = 4;

    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("Plan"), "missing Plan tab:\n{text}");
    assert!(text.contains("writing the plan"), "missing say:\n{text}");
    assert!(text.contains("next"), "missing side composer:\n{text}");
    assert!(text.contains('◇'), "say mark should be hollow:\n{text}");

    let _ = std::fs::remove_dir_all(&dir);
}
