use super::*;
use ratatui::backend::TestBackend;

fn tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = std::env::temp_dir().join(format!(
        "jade-ux-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn bracketed_paste_preserves_newlines_in_composer_and_strips_in_single_line_fields() {
    let dir = tmpdir("paste");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.focus = Focus::Transcript;

    // Main composer: focuses Input and normalizes CRLF/CR to LF without submitting
    app.handle_paste("line1\r\nline2\rline3");
    assert_eq!(app.ui.focus, Focus::Input);
    assert_eq!(app.ui.input, "line1\nline2\nline3");
    assert_eq!(app.ui.input_cursor, "line1\nline2\nline3".chars().count());

    // Login API key form: strips trailing newline from pasted API key
    app.open_login_modal();
    app.handle_paste("deepseek\n");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        app.ui.modal,
        Some(Modal::Login(LoginModal::ApiKeyForm { .. }))
    ));
    app.handle_paste("sk-pasted-secret\r\n");
    if let Some(Modal::Login(LoginModal::ApiKeyForm { key, .. })) = app.ui.modal.as_ref() {
        assert_eq!(key, "sk-pasted-secret");
    } else {
        panic!("expected ApiKeyForm");
    }
    app.ui.modal = None;

    // Picker modal: strips newlines from search query
    app.open_picker("test", vec![("alpha".into(), Command::Quit)]);
    app.handle_paste("al\npha");
    if let Some(Modal::Picker(p)) = app.ui.modal.as_ref() {
        assert_eq!(p.query, "alpha");
    } else {
        panic!("expected Picker");
    }
    app.ui.modal = None;

    // Welcome path mode: strips newlines from pasted path
    app.open_welcome();
    if let Some(w) = app.ui.welcome.as_mut() {
        w.mode = WelcomeMode::OpenPath;
    }
    app.handle_paste("/tmp/my-dir\n");
    assert_eq!(app.ui.welcome.as_ref().unwrap().path, "/tmp/my-dir");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unauthenticated_agent_submit_keeps_input_and_opens_login() {
    let dir = tmpdir("authguard");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    assert_eq!(app.ui.mode, InputMode::Agent);

    for ch in "explain main.rs".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let prompt = match action {
        Action::SubmitAgent(p) => p,
        _ => panic!("unauthenticated Enter should yield SubmitAgent, not SubmitShell"),
    };
    app.submit_agent(prompt);
    assert!(matches!(app.ui.modal, Some(Modal::Login(_))));
    assert_eq!(app.ui.input, "explain main.rs");
    assert_eq!(app.ui.focus, Focus::Input);
    app.ui.modal = None;

    // Explicit `!cmd` shell escape still works without a provider
    app.ui.input = "!echo ok".into();
    app.ui.input_cursor = app.ui.input.chars().count();
    match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
        Action::SubmitShell(req) => assert_eq!(req.spec.display(), "echo ok"),
        _ => panic!("!cmd should still submit shell"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn word_editing_shortcuts_delete_and_move_by_word() {
    let dir = tmpdir("wordedit");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.ui.input = "one two three".into();
    app.ui.input_cursor = app.ui.input.chars().count();

    // Ctrl+Left jumps to start of "three"
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
    assert_eq!(app.ui.input_cursor, 8);

    // Alt+B jumps to start of "two"
    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
    assert_eq!(app.ui.input_cursor, 4);

    // Alt+F jumps past "two" to index 7
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT));
    assert_eq!(app.ui.input_cursor, 7);

    // Ctrl+W deletes "two"
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
    assert_eq!(app.ui.input, "one  three");
    assert_eq!(app.ui.input_cursor, 4);

    // Alt+Backspace at end deletes "three"
    app.ui.input_cursor = app.ui.input.chars().count();
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(app.ui.input, "one  ");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn at_file_completion_searches_completes_and_dismisses_cleanly() {
    let dir = tmpdir("atcomp");
    let work = dir.join("ws");
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::write(work.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(work.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    std::fs::write(work.join("README.md"), "# hi\n").unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, work.clone());
    for ch in "check @main".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let hits = app.file_completions();
    assert_eq!(hits.first().map(String::as_str), Some("src/main.rs"));

    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.ui.input, "check @src/main.rs ");
    assert!(app.file_completions().is_empty());

    // Email-like token must not trigger @ completion
    app.ui.input = "mail@example".into();
    app.ui.input_cursor = app.ui.input.chars().count();
    assert!(app.file_completions().is_empty());

    // Esc dismisses popup without erasing user's text, then Enter submits
    app.ui.input = "see ".into();
    app.ui.input_cursor = app.ui.input.chars().count();
    for ch in "@lib".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert!(!app.file_completions().is_empty());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.file_completions().is_empty());
    assert_eq!(app.ui.input, "see @lib");
    match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
        Action::SubmitAgent(p) => assert_eq!(p, "see @lib"),
        _ => panic!("expected SubmitAgent after Esc dismissed @ menu"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn welcome_shows_resumes_tag_and_n_starts_fresh_session() {
    let dir = tmpdir("welcomenew");
    let here = dir.join("here");
    std::fs::create_dir_all(&here).unwrap();
    let mut saved = Session::new(here.clone(), "local".into());
    saved.title = "prior".into();
    saved.updated_ms = now_ms();
    let item_id = saved.alloc_id();
    saved.items.push(TranscriptItem::User {
        id: item_id,
        text: "old".into(),
        mode: "Agent".into(),
        cwd: here.display().to_string(),
        ts_ms: now_ms(),
    });
    save_session(&dir, &saved).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, here.clone());
    app.open_welcome();
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));

    let mut terminal = Terminal::new(TestBackend::new(112, 26)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(screen.contains("(resumes)"), "welcome screen:\n{screen}");
    assert!(screen.contains("n new session"), "welcome hint:\n{screen}");

    // Pressing `n` starts a fresh session rather than resuming `saved.id`
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.ui.welcome.is_none());
    assert_ne!(app.session.current.id, saved.id);
    assert!(app.session.current.items.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clicking_status_sign_in_opens_login_modal() {
    let dir = tmpdir("statusclick");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.jobs.connected = true;
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();

    let region = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::OpenLogin)
        .cloned()
        .expect("OpenLogin hit region should be registered");
    app.dispatch_click(region.rect.x, region.rect.y);
    assert!(matches!(app.ui.modal, Some(Modal::Login(_))));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn complete_directories_is_case_insensitive_and_fuzzy() {
    let dir = tmpdir("dircmp");
    std::fs::create_dir_all(dir.join("Android")).unwrap();
    std::fs::create_dir_all(dir.join("Applications")).unwrap();
    std::fs::create_dir_all(dir.join("MyProject")).unwrap();

    let ci = complete_directories(&dir, "and");
    assert_eq!(ci.first().map(|(d, _)| d.as_str()), Some("Android/"));

    let sub = complete_directories(&dir, "proj");
    assert_eq!(sub.first().map(|(d, _)| d.as_str()), Some("MyProject/"));

    let fuzzy = complete_directories(&dir, "mpj");
    assert_eq!(fuzzy.first().map(|(d, _)| d.as_str()), Some("MyProject/"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn welcome_path_mode_supports_arrow_selection_and_enter() {
    let dir = tmpdir("warrow");
    let a = dir.join("Alpha");
    let b = dir.join("Beta");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();

    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));

    // Entries: [0] OpenCurrent(dir), [1] Parent(..), [2] Alpha/, [3] Beta/
    // Move down to Beta/ (index 3) and press Tab to descend into Beta/.
    for _ in 0..3 {
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    assert_eq!(app.ui.welcome.as_ref().unwrap().path_selected, 3);
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        std::fs::canonicalize(&app.ui.welcome.as_ref().unwrap().browse_dir).unwrap(),
        std::fs::canonicalize(&b).unwrap()
    );
    assert_eq!(app.ui.welcome.as_ref().unwrap().path_selected, 0);

    // Go back up with Left, filter "al", Enter descends into Alpha/, Enter opens Alpha/.
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    for ch in "al".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.ui.welcome.is_some());
    assert_eq!(
        std::fs::canonicalize(&app.ui.welcome.as_ref().unwrap().browse_dir).unwrap(),
        std::fs::canonicalize(&a).unwrap()
    );
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.ui.welcome.is_none());
    assert_eq!(app.session.current.cwd, std::fs::canonicalize(&a).unwrap());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transcript_folds_cross_step_activity_bidirectionally_and_hides_monitor_notice() {
    let dir = tmpdir("fold");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.jobs.connected = true;

    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "run checks".into(),
        mode: "Agent".into(),
        cwd: ".".into(),
        ts_ms: now_ms(),
    });

    // Cross-step tools separated by an empty collapsed-thought Assistant turn
    let t1 = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Tool {
        id: t1,
        call_id: "c1".into(),
        name: "read".into(),
        title: "read Cargo.toml".into(),
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
    let aid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Assistant {
        id: aid,
        text: String::new(),
        reasoning: "internal step".into(),
        reasoning_collapsed: true,
        model: "m".into(),
        done: true,
        error: None,
    });
    let t2 = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Tool {
        id: t2,
        call_id: "c2".into(),
        name: "monitor".into(),
        title: "monitor start watch".into(),
        subtitle: String::new(),
        job_id: Some(7),
        target: "local".into(),
        output: Vec::new(),
        status: CardStatus::Done,
        collapsed: true,
        duration_ms: Some(20),
        exit_code: Some(0),
        started_ms: now_ms(),
    });

    // MonitorFired appends onto monitor tool output instead of pushing top-level Notice
    let notice_before = app
        .session
        .current
        .items
        .iter()
        .filter(|i| matches!(i, TranscriptItem::Notice { .. }))
        .count();
    app.apply_event(
        10,
        AgentEvent::MonitorFired {
            id: 1,
            label: "watch".into(),
            job_id: 7,
            matched_lines: vec!["tick 1".into()],
            trigger_count: 1,
            max_triggers: Some(2),
            stopped: false,
        },
    );
    let notice_after = app
        .session
        .current
        .items
        .iter()
        .filter(|i| matches!(i, TranscriptItem::Notice { .. }))
        .count();
    assert_eq!(notice_before, notice_after);

    let rid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Assistant {
        id: rid,
        text: "Final summary ready.".into(),
        reasoning: String::new(),
        reasoning_collapsed: true,
        model: "m".into(),
        done: true,
        error: None,
    });

    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let collapsed: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        collapsed.contains("Explored 1 file, 1 more"),
        "collapsed:\n{collapsed}"
    );
    assert!(
        collapsed.contains("▌ run checks"),
        "user card:\n{collapsed}"
    );
    assert!(
        collapsed.contains("◆ Final summary ready."),
        "reply:\n{collapsed}"
    );

    // Click group -> expands with [collapse]; click [collapse] -> folds again
    let grp = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| matches!(hr.target, HitTarget::ToggleToolGroup(_)))
        .unwrap()
        .rect;
    app.dispatch_click(grp.x, grp.y);
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let expanded: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(expanded.contains("[collapse]"), "expanded:\n{expanded}");
    assert!(
        expanded.contains("monitor start watch"),
        "expanded:\n{expanded}"
    );

    let col_btn = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| matches!(hr.target, HitTarget::ToggleToolGroup(_)))
        .unwrap()
        .rect;
    app.dispatch_click(col_btn.x, col_btn.y);
    assert!(app.session.expanded_explore_groups.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lists_keep_nesting_and_code_fences_share_a_column() {
    let src = "intro\n\n```rust\nfn main() {\n    let x = 1;\n}\n```\n\n- parent\n  - child item\n1. first\n";
    let rendered = render_markdown_linked(
        src,
        60,
        &THEMES[0].1,
        std::path::Path::new("/tmp"),
        &[],
        &super::links::WorkspaceIndex::default(),
    );
    let lines: Vec<String> = rendered
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let fence = lines.iter().find(|line| line.contains("+--")).unwrap();
    let code = lines.iter().find(|line| line.contains("fn main")).unwrap();
    assert_eq!(
        fence.find('+'),
        code.find('|'),
        "fence {fence:?} code {code:?}"
    );
    assert!(lines.iter().any(|line| line.contains("+-- rust")));
    let parent = lines.iter().find(|line| line.contains("parent")).unwrap();
    let child = lines.iter().find(|line| line.contains("child")).unwrap();
    assert!(
        child.find('*') > parent.find('*'),
        "parent {parent:?} child {child:?}"
    );
    assert!(lines.iter().any(|line| line.contains("1. first")));
}

#[test]
fn settled_turn_folds_edits_explores_and_commands() {
    let dir = tmpdir("foldsum");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.jobs.connected = true;
    app.apply_event(
        1,
        AgentEvent::FileChanged {
            path: "src/a.rs".into(),
            before: "old\n".into(),
            after: "new\nline\n".into(),
            added: 2,
            removed: 1,
            existed_before: true,
            restorable_before: true,
        },
    );
    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "fix it".into(),
        mode: "Agent".into(),
        cwd: ".".into(),
        ts_ms: now_ms(),
    });
    for (name, title) in [
        ("edit", "edit src/a.rs"),
        ("read", "read src/a.rs"),
        ("exec", "$ cargo test"),
    ] {
        let id = app.session.current.alloc_id();
        app.session.current.items.push(TranscriptItem::Tool {
            id,
            call_id: id.to_string(),
            name: name.into(),
            title: title.into(),
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
    }
    let aid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Assistant {
        id: aid,
        text: "Done.".into(),
        reasoning: "looking".into(),
        reasoning_collapsed: true,
        model: "m".into(),
        done: false,
        error: None,
    });

    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    let draw = |app: &mut App, terminal: &mut Terminal<TestBackend>| {
        terminal.draw(|frame| paint(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };

    app.agent.running = true;
    let live = draw(&mut app, &mut terminal);
    assert!(
        !live.contains("Edited src/a.rs") && !live.contains('✓'),
        "a running turn folds edits/explores/commands cleanly without checkmarks:\n{live}"
    );
    assert!(
        live.contains("Edited 1 file, explored 1 file, ran 1 command"),
        "all collapsed tools fold into one clean summary row while open:\n{live}"
    );
    assert!(!live.contains("◆ ▍"), "no stray caret:\n{live}");

    let thinking = app.session.current.alloc_id();
    app.session.current.items.insert(
        1,
        TranscriptItem::Assistant {
            id: thinking,
            text: String::new(),
            reasoning: "still thinking about the diff".into(),
            reasoning_collapsed: true,
            model: "m".into(),
            done: false,
            error: None,
        },
    );
    let with_caret = draw(&mut app, &mut terminal);
    assert!(
        !with_caret.contains('⠋') && !with_caret.contains("still thinking about the diff"),
        "thinking row stays scrambled/collapsed without spinner:\n{with_caret}"
    );
    assert!(
        !with_caret.contains("◆ ▍"),
        "caret stays gone:\n{with_caret}"
    );
    app.session.current.items.remove(1);

    app.agent.running = false;
    if let Some(TranscriptItem::Assistant { done, .. }) = app.session.current.items.last_mut() {
        *done = true;
    }
    let settled = draw(&mut app, &mut terminal);
    assert!(
        settled.contains("Edited 1 file, explored 1 file, ran 1 command +2 -1"),
        "settled:\n{settled}"
    );
    assert!(
        !settled.contains("$ cargo test"),
        "commands fold into the summary:\n{settled}"
    );
    assert!(settled.contains("◆ Done."), "reply stays:\n{settled}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn turn_summary_paints_removed_lines_red() {
    let dir = tmpdir("foldcolor");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.jobs.connected = true;
    app.apply_event(
        1,
        AgentEvent::FileChanged {
            path: "src/a.rs".into(),
            before: "old\n".into(),
            after: "new\nline\n".into(),
            added: 2,
            removed: 1,
            existed_before: true,
            restorable_before: true,
        },
    );
    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "fix it".into(),
        mode: "Agent".into(),
        cwd: ".".into(),
        ts_ms: now_ms(),
    });
    let id = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Tool {
        id,
        call_id: id.to_string(),
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
    let aid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Assistant {
        id: aid,
        text: "Done.".into(),
        reasoning: String::new(),
        reasoning_collapsed: true,
        model: "m".into(),
        done: true,
        error: None,
    });

    let theme = app.theme();
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let buf = terminal.backend().buffer();

    let row = (0..buf.area.height)
        .find(|&y| {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            line.contains("Edited 1 file") && line.contains("+2 -1")
        })
        .unwrap_or_else(|| panic!("summary row with diff counts missing:\n{}", screen_of(buf)));

    let cell_at = |needle: char, next: char| {
        (0..buf.area.width).find(|&x| {
            buf[(x, row)].symbol() == needle.to_string()
                && x + 1 < buf.area.width
                && buf[(x + 1, row)].symbol() == next.to_string()
        })
    };
    let plus = cell_at('+', '2').expect("+2 present");
    let minus = cell_at('-', '1').expect("-1 present");
    assert_eq!(
        buf[(plus, row)].fg,
        theme.ok,
        "added count stays green:\n{}",
        screen_of(buf)
    );
    assert_eq!(
        buf[(minus, row)].fg,
        theme.bad,
        "removed count is red:\n{}",
        screen_of(buf)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn screen_of(buf: &ratatui::buffer::Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn open_file_card_paints_removed_lines_red() {
    let dir = tmpdir("cardcolor");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.jobs.connected = true;
    app.apply_event(
        1,
        AgentEvent::FileChanged {
            path: "src/a.rs".into(),
            before: "old\n".into(),
            after: "new\nline\n".into(),
            added: 2,
            removed: 1,
            existed_before: true,
            restorable_before: true,
        },
    );
    let id = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Tool {
        id,
        call_id: id.to_string(),
        name: "edit".into(),
        title: "edit src/a.rs".into(),
        subtitle: String::new(),
        job_id: None,
        target: "local".into(),
        output: Vec::new(),
        status: CardStatus::Done,
        // An opened card renders on its own instead of folding into the summary.
        collapsed: false,
        duration_ms: Some(10),
        exit_code: Some(0),
        started_ms: now_ms(),
    });

    let theme = app.theme();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let buf = terminal.backend().buffer();

    let row = (0..buf.area.height)
        .find(|&y| {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            line.contains("src/a.rs") && line.contains("+2") && line.contains("-1")
        })
        .unwrap_or_else(|| panic!("file card with diff counts missing:\n{}", screen_of(buf)));

    let cell_at = |needle: char, next: char| {
        (0..buf.area.width).find(|&x| {
            buf[(x, row)].symbol() == needle.to_string()
                && x + 1 < buf.area.width
                && buf[(x + 1, row)].symbol() == next.to_string()
        })
    };
    let plus = cell_at('+', '2').expect("+2 present");
    let minus = cell_at('-', '1').expect("-1 present");
    assert_eq!(buf[(plus, row)].fg, theme.ok, "card +2 stays green");
    assert_eq!(buf[(minus, row)].fg, theme.bad, "card -1 is red");

    let _ = std::fs::remove_dir_all(&dir);
}
