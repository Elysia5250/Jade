use super::*;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::MouseEvent;

fn tmpdir() -> PathBuf {
    let mut b = [0u8; 6];
    let _ = getrandom::fill(&mut b);
    let p = std::env::temp_dir().join(format!(
        "envoy-insp-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn render_text(terminal: &mut Terminal<TestBackend>, app: &mut App) -> String {
    terminal.draw(|frame| view::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn click_at(app: &mut App, col: u16, row: u16) {
    app.handle_mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), col, row));
    app.handle_mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), col, row));
}

#[test]
fn inspector_opens_by_default_and_remembers_dragged_width() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.connected = true;

    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    render_text(&mut terminal, &mut app);

    assert!(app.show_sidebar);
    let div_x = app.inspector.divider_x.expect("divider should render");
    let default_overview_w = app.inspector.last_area.width;
    assert!(default_overview_w >= 36);

    // Drag divider left to column 54 to widen Inspector.
    app.handle_mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), div_x, 5));
    assert!(app.inspector.dragging_divider);
    app.handle_mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), 54, 5));
    app.handle_mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), 54, 5));
    assert!(!app.inspector.dragging_divider);
    assert!(app.inspector.dragged_ratio.is_some());

    render_text(&mut terminal, &mut app);
    let dragged_w = app.inspector.last_area.width;
    assert!(dragged_w > default_overview_w + 10, "dragged_w={dragged_w}, default={default_overview_w}");

    // Switching tabs and toggling Ctrl+B off & on preserves dragged width.
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT));
    render_text(&mut terminal, &mut app);
    assert_eq!(app.inspector.tab, InspectorTab::Diff);
    assert_eq!(app.inspector.last_area.width, dragged_w);

    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    render_text(&mut terminal, &mut app);
    assert!(!app.show_sidebar);
    assert!(app.inspector.divider_x.is_none());

    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    render_text(&mut terminal, &mut app);
    assert!(app.show_sidebar);
    assert_eq!(app.inspector.last_area.width, dragged_w);

    // /sidebar reset forgets dragged ratio and restores auto-width.
    app.run_slash("/sidebar reset");
    render_text(&mut terminal, &mut app);
    assert!(app.inspector.dragged_ratio.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clicking_underlined_file_link_opens_diff_or_file_preview() {
    let dir = tmpdir();
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let lib_rs = src_dir.join("lib.rs");
    let before = "pub fn greet() {\n    println!(\"old\");\n}\n";
    let after = "pub fn greet() {\n    println!(\"new\");\n}\n";
    std::fs::write(&lib_rs, after).unwrap();
    let readme = dir.join("README.md");
    std::fs::write(&readme, "# Title\nline two here\nline three\n").unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.connected = true;
    app.apply_event(
        1,
        AgentEvent::FileChanged {
            path: "src/lib.rs".into(),
            before: before.into(),
            after: after.into(),
            added: 1,
            removed: 1,
        },
    );
    let aid = app.session.alloc_id();
    app.session.items.push(TranscriptItem::Assistant {
        id: aid,
        text: "Updated src/lib.rs and documented in README.md:2.".into(),
        reasoning: String::new(),
        reasoning_collapsed: true,
        model: "claude-opus-4".into(),
        done: true,
        error: None,
    });

    let mut terminal = Terminal::new(TestBackend::new(124, 34)).unwrap();
    render_text(&mut terminal, &mut app);

    // Click src/lib.rs link -> opens Diff tab showing Unified diff.
    let lib_hit = app
        .hit_regions
        .iter()
        .find(|hr| matches!(&hr.target, HitTarget::OpenFile { path, .. } if path.ends_with("src/lib.rs")))
        .expect("underlined src/lib.rs hit region should exist")
        .rect;
    click_at(&mut app, lib_hit.x, lib_hit.y);
    assert_eq!(app.inspector.tab, InspectorTab::Diff);

    let out = render_text(&mut terminal, &mut app);
    assert!(out.contains("src/lib.rs"), "diff header missing:\n{out}");
    assert!(out.contains("new"), "diff content missing:\n{out}");

    // Toggle Unified <-> Split via 'v' key in Sidebar focus.
    app.focus = Focus::Sidebar;
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    assert_eq!(app.inspector.diff_layout, diff::DiffLayout::Split);
    let split_out = render_text(&mut terminal, &mut app);
    assert!(split_out.contains("│"), "split divider missing:\n{split_out}");

    // Click unmodified README.md:2 link -> opens File tab with line preview.
    let readme_hit = app
        .hit_regions
        .iter()
        .find(|hr| matches!(&hr.target, HitTarget::OpenFile { path, line: Some(2) } if path.ends_with("README.md")))
        .expect("README.md:2 hit region should exist")
        .rect;
    click_at(&mut app, readme_hit.x, readme_hit.y);
    assert_eq!(app.inspector.tab, InspectorTab::File);
    assert_eq!(app.inspector.file_line, Some(2));
    let file_out = render_text(&mut terminal, &mut app);
    assert!(file_out.contains("line two here"), "file preview missing:\n{file_out}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cursor_style_collapsed_explore_group_and_reasoning_expand_on_click() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.connected = true;

    let aid = app.session.alloc_id();
    app.session.items.push(TranscriptItem::Assistant {
        id: aid,
        text: "Done inspecting.".into(),
        reasoning: "First inspect Cargo.toml\nThen check main.rs".into(),
        reasoning_collapsed: true,
        model: "claude-opus-4".into(),
        done: true,
        error: None,
    });

    for (cid, name, title) in [
        ("c1", "read", "read Cargo.toml"),
        ("c2", "read", "read src/main.rs"),
        ("c3", "grep", "grep pattern"),
    ] {
        let tid = app.session.alloc_id();
        app.session.items.push(TranscriptItem::Tool {
            id: tid,
            call_id: cid.into(),
            name: name.into(),
            title: title.into(),
            subtitle: String::new(),
            job_id: None,
            target: "local".into(),
            output: vec![crate::session::OutputLine {
                text: "line".into(),
                err: false,
            }],
            status: CardStatus::Done,
            collapsed: true,
            duration_ms: Some(100),
            exit_code: Some(0),
            started_ms: now_ms(),
        });
    }

    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    let collapsed_text = render_text(&mut terminal, &mut app);
    assert!(
        collapsed_text.contains("▸ Thought"),
        "expected collapsed thought line:\n{collapsed_text}"
    );
    assert!(
        collapsed_text.contains("▸ Explored 3 ops (read ×2, grep ×1)"),
        "expected grouped explore summary line:\n{collapsed_text}"
    );
    assert!(
        !collapsed_text.contains("First inspect Cargo.toml"),
        "collapsed reasoning body should be hidden:\n{collapsed_text}"
    );

    // Click the grouped explore summary line -> expands individual read/grep rows.
    let group_hit = app
        .hit_regions
        .iter()
        .find(|hr| matches!(hr.target, HitTarget::ToggleToolGroup(_)))
        .expect("explore group hit region")
        .rect;
    click_at(&mut app, group_hit.x, group_hit.y);
    let expanded_text = render_text(&mut terminal, &mut app);
    assert!(
        expanded_text.contains("Read") && expanded_text.contains("Cargo.toml"),
        "expected individual Read items after expanding:\n{expanded_text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn inline_ask_keeps_inspector_visible_and_accepts_click() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.connected = true;
    app.pending_ask = Some(PendingAsk {
        id: 7,
        question: "Approve network?".into(),
        options: vec![
            AskOption {
                label: "Allow once".into(),
                description: Some("Allow this command".into()),
                recommended: true,
            },
            AskOption {
                label: "Deny".into(),
                description: None,
                recommended: false,
            },
        ],
        recommended: 0,
        selected: 0,
        deadline: Instant::now() + Duration::from_secs(30),
    });

    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    let text = render_text(&mut terminal, &mut app);
    assert!(text.contains("Approve network?"));
    assert!(text.contains("1 Overview"), "Inspector must stay visible beside inline ask");

    let opt_hit = app
        .hit_regions
        .iter()
        .find(|hr| matches!(hr.target, HitTarget::SelectAskOption(0)))
        .expect("ask option 0 hit region")
        .rect;
    click_at(&mut app, opt_hit.x, opt_hit.y);
    assert!(app.pending_ask.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}
