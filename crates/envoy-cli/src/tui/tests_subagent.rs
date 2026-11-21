use super::*;
use ratatui::backend::TestBackend;

fn tmpdir() -> PathBuf {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    let path = std::env::temp_dir().join(format!(
        "envoy-sub-{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn render_text(terminal: &mut Terminal<TestBackend>, app: &mut App) -> String {
    terminal.draw(|frame| paint(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn note(index: u32, name: &str, description: &str, note: SubagentNote) -> AgentEvent {
    AgentEvent::Subagent {
        parent_call_id: "d1".into(),
        index,
        total: 2,
        name: name.into(),
        description: description.into(),
        note,
    }
}

#[test]
fn delegate_rows_open_a_child_transcript_in_the_inspector() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.apply_event(
        1,
        AgentEvent::ToolStart {
            call_id: "d1".into(),
            name: "delegate".into(),
            title: "delegate batch".into(),
            subtitle: String::new(),
        },
    );
    app.apply_event(
        2,
        note(
            1,
            "explore",
            "find auth",
            SubagentNote::Started {
                prompt: "unique-prompt-token".into(),
            },
        ),
    );
    app.apply_event(
        3,
        note(
            1,
            "explore",
            "find auth",
            SubagentNote::Tool {
                call_id: "t1".into(),
                name: "read".into(),
                title: "Read".into(),
                subtitle: "auth.rs".into(),
                running: true,
                ok: true,
                duration_ms: 0,
            },
        ),
    );
    app.apply_event(
        4,
        note(
            1,
            "explore",
            "find auth",
            SubagentNote::Finished { ok: true },
        ),
    );
    app.apply_event(
        5,
        note(
            2,
            "review",
            "check tests",
            SubagentNote::Started {
                prompt: "second-prompt-token".into(),
            },
        ),
    );

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    let text = render_text(&mut terminal, &mut app);
    assert!(text.contains("delegating"), "{text}");
    assert!(text.contains("subagent 1"), "{text}");
    assert!(text.contains("subagent 2"), "{text}");
    assert!(
        !text.contains("unique-prompt-token"),
        "child prompt leaked into the main transcript:\n{text}"
    );

    let hit = app
        .ui
        .hit_regions
        .iter()
        .find(|region| matches!(region.target, HitTarget::OpenSubagent { index: 1, .. }))
        .unwrap();
    let col = hit.rect.x + 1;
    let row = hit.rect.y;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    });
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    });

    assert_eq!(app.ui.inspector.tab, InspectorTab::Agent);
    let opened = render_text(&mut terminal, &mut app);
    assert!(
        opened.contains("unique-prompt-token"),
        "inspector missing the child prompt:\n{opened}"
    );
    assert!(opened.contains("Read"), "{opened}");
    assert!(opened.contains("auth.rs"), "{opened}");

    app.apply_event(6, AgentEvent::TurnError("interrupted by user".into()));
    let lanes = app.session.current.subagents.get("d1").unwrap();
    assert_eq!(lanes[0].status, CardStatus::Done);
    assert_eq!(lanes[1].status, CardStatus::Error);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn subagent_streaming_markdown_preserves_newlines_and_renders_in_inspector() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.apply_event(
        1,
        AgentEvent::ToolStart {
            call_id: "d1".into(),
            name: "delegate".into(),
            title: "delegate batch".into(),
            subtitle: String::new(),
        },
    );
    app.apply_event(
        2,
        note(
            1,
            "explore",
            "plan",
            SubagentNote::Started {
                prompt: "inspect".into(),
            },
        ),
    );
    for (seq, chunk) in [
        "## Plan\n",
        "\n- **step 1**\n",
        "- step 2\n\n",
        "```rs\nlet x = 1;\n```",
    ]
    .into_iter()
    .enumerate()
    {
        app.apply_event(
            (seq + 3) as u64,
            note(
                1,
                "explore",
                "plan",
                SubagentNote::Text {
                    text: chunk.into(),
                },
            ),
        );
    }

    app.open_subagent("d1".into(), 1);
    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    let text = render_text(&mut terminal, &mut app);
    assert!(text.contains("Plan"), "{text}");
    assert!(text.contains("* step 1"), "list item missing:\n{text}");
    assert!(text.contains("* step 2"), "second list item missing:\n{text}");
    assert!(text.contains("+-- rs"), "code block missing:\n{text}");
    assert!(text.contains("let x = 1;"), "{text}");
    assert!(!text.contains("**step 1**"), "raw ** leaked:\n{text}");
    assert!(!text.contains("## Plan-"), "newlines stripped:\n{text}");

    let theme = app.theme();
    let buf = terminal.backend().buffer();
    let area = app.ui.inspector.last_area;
    let code_has_bg = (area.y..area.y + area.height).any(|y| {
        let row: String = (area.x..area.x + area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect();
        row.contains("let x = 1;") && buf[(area.x + 4, y)].bg == theme.bg
    });
    assert!(code_has_bg, "agent view code block should use theme.bg");

    let _ = std::fs::remove_dir_all(&dir);
}
