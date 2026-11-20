use super::*;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::MouseEvent;

fn test_app(dir: &std::path::Path) -> App {
    let mut app = App::new(dir.to_path_buf(), "local".into(), None, dir.to_path_buf());
    app.jobs.connected = true;
    app.ui.show_sidebar = false;
    app
}

fn temp_workspace() -> PathBuf {
    let mut b = [0u8; 6];
    let _ = getrandom::fill(&mut b);
    let p = std::env::temp_dir().join(format!(
        "envoy-fold-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn draw_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| paint(frame, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

fn click_at(app: &mut App, col: u16, row: u16) {
    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let up = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let _ = app.handle_mouse(down);
    let _ = app.handle_mouse(up);
}

#[test]
fn consecutive_failed_commands_and_non_edit_tools_fold_into_one_line() {
    let dir = temp_workspace();
    let mut app = test_app(&dir);
    app.agent.running = true;

    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "run all tools".into(),
        mode: "Agent".into(),
        cwd: dir.display().to_string(),
        ts_ms: now_ms(),
    });

    let push_tool = |app: &mut App, name: &str, title: &str, status: CardStatus| {
        let id = app.session.current.alloc_id();
        app.session.current.items.push(TranscriptItem::Tool {
            id,
            call_id: format!("c-{id}"),
            name: name.into(),
            title: title.into(),
            subtitle: String::new(),
            job_id: None,
            target: "local".into(),
            output: Vec::new(),
            status,
            collapsed: true,
            duration_ms: Some(120),
            exit_code: (status == CardStatus::Error).then_some(7),
            started_ms: now_ms(),
        });
    };

    push_tool(&mut app, "read", "read Cargo.toml", CardStatus::Done);
    push_tool(&mut app, "exec", "$ pwd; exit 7", CardStatus::Error);
    push_tool(
        &mut app,
        "skill",
        "skill nonexistent-skill",
        CardStatus::Error,
    );
    let mid_thought = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Assistant {
        id: mid_thought,
        text: String::new(),
        reasoning: "internal step note".into(),
        reasoning_collapsed: true,
        model: "m".into(),
        done: true,
        error: None,
    });
    push_tool(
        &mut app,
        "webfetch",
        "fetch https://example.com",
        CardStatus::Done,
    );
    push_tool(
        &mut app,
        "delegate",
        "delegate explore workspace",
        CardStatus::Done,
    );

    let text = draw_rows(&mut app, 110, 32).join("\n");
    assert!(
        !text.contains("nonexistent-skill") && !text.contains("example.com"),
        "consecutive failed commands and non-edit tools should fold together:\n{text}"
    );
    assert!(
        text.contains("Explored 1 file, ran 1 command, 3 more (failed)"),
        "expected combined folded summary with failed badge:\n{text}"
    );

    let hit = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| matches!(hr.target, HitTarget::ToggleToolGroup(_)))
        .expect("folded tool group hit region")
        .rect;
    click_at(&mut app, hit.x, hit.y);
    let expanded = draw_rows(&mut app, 110, 36).join("\n");
    assert!(
        expanded.contains("nonexistent-skill") && expanded.contains("[collapse]"),
        "clicking folded group should expand underlying commands & tools:\n{expanded}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn thinking_wording_is_unified_collapsed_and_coalesces_consecutive_steps() {
    let dir = temp_workspace();
    let mut app = test_app(&dir);
    app.agent.running = true;

    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "inspect".into(),
        mode: "Agent".into(),
        cwd: dir.display().to_string(),
        ts_ms: now_ms(),
    });
    let r1 = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Assistant {
        id: r1,
        text: String::new(),
        reasoning: "first hidden chain of thought".into(),
        reasoning_collapsed: true,
        model: "m".into(),
        done: true,
        error: None,
    });
    let r2 = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Assistant {
        id: r2,
        text: String::new(),
        reasoning: "live streaming secret reasoning snippet".into(),
        reasoning_collapsed: true,
        model: "m".into(),
        done: false,
        error: None,
    });

    let live_text = draw_rows(&mut app, 100, 28).join("\n");
    let braille = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    assert!(
        braille.iter().all(|c| !live_text.contains(*c))
            && !live_text.contains("Thought")
            && !live_text.contains("Thinking.")
            && !live_text.contains("secret reasoning snippet"),
        "streaming collapsed thinking must use Decrypt Effect without spinners:\n{live_text}"
    );
    assert!(
        app.ui
            .hit_regions
            .iter()
            .any(|hr| matches!(hr.target, HitTarget::ToggleReasoning(_))),
        "live thinking row should keep ToggleReasoning hit target"
    );

    if let Some(TranscriptItem::Assistant { done, .. }) =
        app.session.current.items.iter_mut().find(|i| i.id() == r2)
    {
        *done = true;
    }
    app.agent.running = false;

    let settled = draw_rows(&mut app, 100, 28).join("\n");
    let count = settled.matches("> Thinking").count();
    assert_eq!(
        count, 1,
        "consecutive reasoning steps should fold into a single '> Thinking' row:\n{settled}"
    );
    assert!(!settled.contains("> Thought"));

    let hit = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| matches!(hr.target, HitTarget::ToggleReasoning(_)))
        .expect("thinking toggle hit region")
        .rect;
    click_at(&mut app, hit.x, hit.y);
    let expanded = draw_rows(&mut app, 100, 32).join("\n");
    assert!(
        expanded.contains("v Thinking")
            && expanded.contains("first hidden chain of thought")
            && expanded.contains("live streaming secret reasoning snippet"),
        "clicking '> Thinking' should expand to 'v Thinking' with all coalesced steps:\n{expanded}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn markdown_blocks_and_transcript_phases_have_vertical_breathing_room() {
    let dir = temp_workspace();
    let theme = &THEMES[0].1;
    let idx = links::WorkspaceIndex::default();
    let rendered = render_markdown_linked(
        "# Heading\nIntro paragraph.\n```\ncode line\n```\n- first wrapped item that needs more than forty columns to fit cleanly\n- second item",
        44,
        theme,
        &dir,
        &[],
        &idx,
    );
    let plain: Vec<String> = rendered
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert!(
        plain.iter().filter(|l| l.trim().is_empty()).count() >= 4,
        "heading, fence, and wrapped list should have clean blank separators:\n{}",
        plain.join("\n")
    );
    assert!(
        !plain.last().is_some_and(|l| l.trim().is_empty()),
        "rendered markdown should not leave trailing blank rows at EOF"
    );

    let mut app = test_app(&dir);
    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "go".into(),
        mode: "Agent".into(),
        cwd: dir.display().to_string(),
        ts_ms: now_ms(),
    });
    let t1 = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Tool {
        id: t1,
        call_id: "c1".into(),
        name: "write".into(),
        title: "write src/a.rs".into(),
        subtitle: String::new(),
        job_id: None,
        target: "local".into(),
        output: Vec::new(),
        status: CardStatus::Done,
        collapsed: true,
        duration_ms: Some(10),
        exit_code: Some(0),
        started_ms: now_ms(),
    });
    let s1 = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Say {
        id: s1,
        text: "phase update".into(),
        level: "info".into(),
    });
    let t2 = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Tool {
        id: t2,
        call_id: "c2".into(),
        name: "edit".into(),
        title: "edit src/a.rs".into(),
        subtitle: String::new(),
        job_id: None,
        target: "local".into(),
        output: Vec::new(),
        status: CardStatus::Done,
        collapsed: true,
        duration_ms: Some(10),
        exit_code: Some(0),
        started_ms: now_ms(),
    });
    app.agent.running = true;

    let rows = draw_rows(&mut app, 100, 24);
    let say_row = rows
        .iter()
        .position(|r| r.contains("phase update"))
        .expect("say row");
    assert!(
        rows[say_row - 1].trim().is_empty() && rows[say_row + 1].trim().is_empty(),
        "Say milestone should have blank rows above and below:\n{}",
        rows.join("\n")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn running_tools_and_groups_render_progressive_phrases_with_decrypt_effect() {
    assert_eq!(running_tool_phrase("exec"), "Running command");
    assert_eq!(running_tool_phrase("edit"), "Editing");
    assert_eq!(running_tool_phrase("webfetch"), "Fetching");

    let dir = temp_workspace();
    let mut app = test_app(&dir);
    app.agent.running = true;

    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "run checks".into(),
        mode: "Agent".into(),
        cwd: dir.display().to_string(),
        ts_ms: now_ms(),
    });
    for (idx, cmd) in ["$ cargo check", "$ cargo test"].iter().enumerate() {
        let id = app.session.current.alloc_id();
        app.session.current.items.push(TranscriptItem::Tool {
            id,
            call_id: format!("exec-{idx}"),
            name: "exec".into(),
            title: (*cmd).into(),
            subtitle: String::new(),
            job_id: None,
            target: "local".into(),
            output: Vec::new(),
            status: CardStatus::Running,
            collapsed: true,
            duration_ms: None,
            exit_code: None,
            started_ms: now_ms(),
        });
    }
    let sep_id = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Say {
        id: sep_id,
        text: "editing file".into(),
        level: "info".into(),
    });
    let edit_id = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Tool {
        id: edit_id,
        call_id: "edit-1".into(),
        name: "edit".into(),
        title: "edit src/main.rs".into(),
        subtitle: String::new(),
        job_id: None,
        target: "local".into(),
        output: Vec::new(),
        status: CardStatus::Running,
        collapsed: true,
        duration_ms: None,
        exit_code: None,
        started_ms: now_ms(),
    });

    assert_eq!(
        running_group_phrase(&app.session.current.items[1..3]),
        "Running commands"
    );

    let rendered = draw_rows(&mut app, 110, 28).join("\n");
    for spinner_ch in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
        assert!(
            !rendered.contains(spinner_ch),
            "unexpected spinner {spinner_ch}:\n{rendered}"
        );
    }
    assert!(
        !rendered.contains("src/main.rs") && !rendered.contains("cargo test"),
        "collapsed running tools must hide concrete paths and commands:\n{rendered}"
    );
    assert!(
        app.ui
            .hit_regions
            .iter()
            .any(|hr| matches!(hr.target, HitTarget::ToggleToolGroup(_))),
        "running command group should remain expandable"
    );

    let card_hit = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| matches!(hr.target, HitTarget::ToggleToolCard(_)))
        .expect("single running edit hit region")
        .rect;
    click_at(&mut app, card_hit.x, card_hit.y);
    let opened = draw_rows(&mut app, 110, 28).join("\n");
    assert!(
        opened.contains("src/main.rs"),
        "clicking collapsed running edit should reveal target path:\n{opened}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn done_and_failed_single_tools_fold_without_check_or_cross_badges() {
    let dir = temp_workspace();
    let mut app = test_app(&dir);
    app.agent.running = true;

    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "verify".into(),
        mode: "Agent".into(),
        cwd: dir.display().to_string(),
        ts_ms: now_ms(),
    });
    for (name, title, status) in [
        ("todo", "todo write", CardStatus::Done),
        ("edit", "edit src/a.rs", CardStatus::Done),
        ("exec", "$ false", CardStatus::Error),
    ] {
        let tid = app.session.current.alloc_id();
        app.session.current.items.push(TranscriptItem::Tool {
            id: tid,
            call_id: format!("t-{tid}"),
            name: name.into(),
            title: title.into(),
            subtitle: String::new(),
            job_id: None,
            target: "local".into(),
            output: Vec::new(),
            status,
            collapsed: true,
            duration_ms: Some(25),
            exit_code: (status == CardStatus::Error).then_some(1),
            started_ms: now_ms(),
        });
        let sid = app.session.current.alloc_id();
        app.session.current.items.push(TranscriptItem::Say {
            id: sid,
            text: format!("step {sid}"),
            level: "info".into(),
        });
    }

    let text = draw_rows(&mut app, 110, 32).join("\n");
    assert!(
        !text.contains('✓') && !text.contains('✗'),
        "collapsed transcript should hide leading check and cross glyphs:\n{text}"
    );
    assert!(
        text.contains("> 1 tool")
            && text.contains("> Edited 1 file")
            && text.contains("> Ran 1 command (failed)"),
        "single finished/failed tools should fold cleanly with uniform '>':\n{text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn markdown_table_keeps_escaped_and_inline_code_pipes_inside_cell() {
    let cols = split_table_row("| grep | `def (add\\|multiply)` | passed |");
    assert_eq!(cols, vec!["grep", "`def (add|multiply)`", "passed"]);
    let code_cols = split_table_row("| grep | `a|b` | passed |");
    assert_eq!(code_cols, vec!["grep", "`a|b`", "passed"]);
}
