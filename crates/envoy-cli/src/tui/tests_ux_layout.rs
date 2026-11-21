use super::*;
use ratatui::backend::TestBackend;

fn tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = std::env::temp_dir().join(format!(
        "envoy-ux-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn markdown_table_formats_inline_cells_and_blockquotes_cleanly() {
    let src = "| Item | Result |\n|---|---|\n| **Workspace** | `retest-probe.txt` ok |\n\n> Quoted insight\n\n---\n";
    let rendered = render_markdown_linked(
        src,
        72,
        &THEMES[0].1,
        std::path::Path::new("/tmp"),
        &[],
        &super::links::WorkspaceIndex::default(),
    );
    let lines: Vec<String> = rendered
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let joined = lines.join("\n");
    assert!(joined.contains("Workspace"), "{joined}");
    assert!(joined.contains("retest-probe.txt ok"), "{joined}");
    assert!(
        !joined.contains("**Workspace**"),
        "raw stars leaked:\n{joined}"
    );
    assert!(
        !joined.contains("`retest-probe.txt`"),
        "raw backticks leaked:\n{joined}"
    );
    assert!(joined.contains("┼"), "expected thin rule:\n{joined}");
    assert!(joined.contains("│ Quoted insight"), "{joined}");
}

#[test]
fn composer_and_user_block_have_vertical_breathing_room() {
    let dir = tmpdir("breathing");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.jobs.connected = true;
    let uid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::User {
        id: uid,
        text: "short prompt".into(),
        mode: "Agent".into(),
        cwd: ".".into(),
        ts_ms: now_ms(),
    });

    let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    let rows: Vec<String> = (0..32u16)
        .map(|y| {
            (0..100u16)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();

    // User card spans 3 rows (top pad, text, bottom pad) with `▌` gutter
    let user_row = rows
        .iter()
        .position(|r| r.contains("▌ short prompt"))
        .expect("user prompt row");
    assert!(
        rows[user_row - 1].contains("▌"),
        "missing top pad row:\n{}",
        rows.join("\n")
    );
    assert!(
        rows[user_row + 1].contains("▌"),
        "missing bot pad row:\n{}",
        rows.join("\n")
    );

    // Composer box spans 6 rows (1 border + 1 top pad + 2 content rows + 1 bottom pad + 1 border)
    let top = rows
        .iter()
        .position(|r| r.contains('╭'))
        .expect("top border");
    let bottom = rows
        .iter()
        .rposition(|r| r.contains('╰'))
        .expect("bottom border");
    assert_eq!(
        bottom - top + 1,
        6,
        "rows 22..32:\n{}",
        rows[22..].join("\n")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn setup_signed_in_provider(dir: &std::path::Path, model: &str) {
    use envoy_agent::auth::{record_provider_login, Credential, ProviderProtocol, StoredProvider};
    record_provider_login(
        dir,
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
        Some(model.into()),
    )
    .unwrap();
}

fn row_strings(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            let mut out = String::new();
            let mut x = 0u16;
            while x < buf.area.width {
                let sym = buf[(x, y)].symbol();
                out.push_str(sym);
                x += (display_width(sym).max(1)) as u16;
            }
            out
        })
        .collect()
}

#[test]
fn three_slot_bars_render_cleanly_on_wide_standard_and_narrow_screens() {
    let dir = tmpdir("3slot");
    let ws = dir.join("项目").join("envoy");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::write(ws.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    setup_signed_in_provider(&dir, "deepseek-flash");

    let mut app = App::new(dir.clone(), "local".into(), None, ws);
    app.jobs.connected = true;
    app.session.current.title = "TUI 状态栏升级".into();
    app.session.current.prompt_tokens = 293_300;
    app.session
        .current
        .record_file_change("src/main.rs".into(), "a\n".into(), "a\nb\nc\nd\n".into());

    for (w, h) in [(180u16, 45u16), (110, 32)] {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| paint(frame, &mut app)).unwrap();
        let rows = row_strings(terminal.backend().buffer());

        assert!(rows[0].contains("hub"), "row 0 missing hub:\n{}", rows[0]);
        assert!(
            rows[0].contains("项目/envoy"),
            "row 0 truncated CJK workspace path at {w}x{h}:\n{}",
            rows[0]
        );
        assert!(
            rows[0].contains("⎇ main"),
            "row 0 missing git branch at {w}x{h}:\n{}",
            rows[0]
        );
        assert!(
            rows[0].contains("1 file") && rows[0].contains("+3 -0"),
            "row 0 missing diff summary at {w}x{h}:\n{}",
            rows[0]
        );
        assert!(
            rows[0].contains("[panel]"),
            "row 0 missing [panel] at {w}x{h}:\n{}",
            rows[0]
        );
        if w >= 180 {
            assert!(
                rows[0].contains("TUI 状态栏升级"),
                "4K row 0 missing session title:\n{}",
                rows[0]
            );
        }
        assert!(
            rows[1].contains("────"),
            "row 1 missing top separator at {w}x{h}:\n{}",
            rows[1]
        );

        let comp_top = rows
            .iter()
            .position(|r| r.contains('╭'))
            .expect("composer top border");
        let comp_bar = &rows[comp_top - 1];
        assert!(
            comp_bar.contains("deepseek-flash"),
            "composer bar missing model at {w}x{h}:\n{comp_bar}"
        );
        assert!(
            comp_bar.contains('━') && comp_bar.contains('─'),
            "composer bar missing thin unicode context bar:\n{comp_bar}"
        );
        assert!(
            comp_bar.contains("293.3k/"),
            "composer bar missing token ratio:\n{comp_bar}"
        );

        let bottom = &rows[(h - 1) as usize];
        assert!(
            bottom.contains("agent") && bottom.contains("perm:") && bottom.contains("sandbox:"),
            "bottom bar missing controls at {w}x{h}:\n{bottom}"
        );
    }

    for (w, h) in [(80u16, 24u16), (10, 3)] {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| paint(frame, &mut app)).unwrap();
        for hr in &app.ui.hit_regions {
            assert!(
                hr.rect.x + hr.rect.width <= w && hr.rect.y + hr.rect.height <= h,
                "hit region {:?} out of bounds for {w}x{h}",
                hr.rect
            );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn composer_bar_shows_running_tool_elapsed_and_queued_above_bar() {
    let dir = tmpdir("running-bar");
    setup_signed_in_provider(&dir, "claude-sonnet-4-5");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.jobs.connected = true;
    app.agent.running = true;
    app.agent.turn_started_at = Some(Instant::now() - Duration::from_secs(5));
    let tid = app.session.current.alloc_id();
    app.session.current.items.push(TranscriptItem::Tool {
        id: tid,
        call_id: "c1".into(),
        name: "bash".into(),
        title: "cargo test".into(),
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
    app.agent.queued.push_back("follow up task".into());

    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let rows = row_strings(terminal.backend().buffer());
    let comp_top = rows
        .iter()
        .position(|r| r.contains('╭'))
        .expect("composer top");
    let comp_bar = &rows[comp_top - 1];
    assert!(
        comp_bar.contains("· bash"),
        "expected running tool in composer bar:\n{comp_bar}"
    );
    assert!(
        comp_bar.contains("5s") || comp_bar.contains("6s"),
        "expected elapsed timer in composer bar:\n{comp_bar}"
    );
    assert!(
        comp_bar.contains("+1 queued"),
        "expected queued badge in composer bar:\n{comp_bar}"
    );
    assert!(
        rows[comp_top - 2].contains("follow up task"),
        "queued preview should float above composer bar, not cover it:\n{}",
        rows[comp_top - 3..=comp_top].join("\n")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bar_hit_regions_dispatch_and_top_bar_click_keeps_input_focus() {
    let dir = tmpdir("bar-hits");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.jobs.connected = true;
    for i in 0..40 {
        let uid = app.session.current.alloc_id();
        app.session.current.items.push(TranscriptItem::User {
            id: uid,
            text: format!("line {i}"),
            mode: "Agent".into(),
            cwd: "/tmp".into(),
            ts_ms: now_ms(),
        });
    }

    let mut terminal = Terminal::new(TestBackend::new(120, 28)).unwrap();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    assert!(app.ui.transcript_max_scroll > 0);
    app.ui.follow = false;
    app.ui.transcript_scroll = 0;

    terminal.draw(|frame| paint(frame, &mut app)).unwrap();
    let jump = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::JumpToBottom)
        .cloned()
        .expect("JumpToBottom hit region");
    app.dispatch_click(jump.rect.x, jump.rect.y);
    assert!(app.ui.follow);
    assert_eq!(app.ui.transcript_scroll, app.ui.transcript_max_scroll);

    // Click empty space on row 0 (top bar) -> must not focus Transcript.
    app.ui.focus = Focus::Input;
    let main_w = app
        .ui
        .inspector
        .divider_x
        .unwrap_or(120);
    app.dispatch_click(main_w.saturating_sub(14), 0);
    assert_eq!(app.ui.focus, Focus::Input);

    // Untrusted workspace opens trust picker from bottom bar.
    let trust_hit = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::OpenTrustPicker)
        .cloned()
        .expect("OpenTrustPicker hit region");
    app.dispatch_click(trust_hit.rect.x, trust_hit.rect.y);
    assert!(matches!(app.ui.modal, Some(Modal::Picker(_))));
    app.ui.modal = None;

    // Signed-in composer bar registers OpenModels, CycleEffort, and OpenBudget.
    setup_signed_in_provider(&dir, "claude-sonnet-4-5");
    app.inputs.invalidate_catalog();
    terminal.draw(|frame| paint(frame, &mut app)).unwrap();

    for target in [
        HitTarget::OpenHub,
        HitTarget::OpenModels,
        HitTarget::CycleEffort,
        HitTarget::OpenBudget,
        HitTarget::CycleMode,
        HitTarget::CyclePermission,
    ] {
        assert!(
            app.ui.hit_regions.iter().any(|hr| hr.target == target),
            "missing hit region for {target:?}"
        );
    }

    let effort_hit = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::CycleEffort)
        .cloned()
        .unwrap();
    app.dispatch_click(effort_hit.rect.x, effort_hit.rect.y);
    assert_eq!(envoy_agent::agent::effort::load(&dir), "high");

    let models_hit = app
        .ui
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::OpenModels)
        .cloned()
        .unwrap();
    app.dispatch_click(models_hit.rect.x, models_hit.rect.y);
    assert!(matches!(app.ui.modal, Some(Modal::Models(_))));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wrapped_lists_align_with_headings_and_avoid_orphan_closing_punctuation() {
    let src = "### 实用结论（不变）\n* 若确实需要临时文件跨命令传递，用工作目录下的子目录，或 ~（家目录不受此限制，但会在磁盘留痕）\n";
    // Pick width so `)` would fall immediately after `痕` on a naive char wrap.
    for width in 60..=92 {
        let rendered = render_markdown_linked(
            src,
            width,
            &THEMES[0].1,
            std::path::Path::new("/tmp"),
            &[],
            &super::links::WorkspaceIndex::default(),
        );
        let lines: Vec<String> = rendered
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let h_col = lines
            .iter()
            .find(|l| l.contains("实用结论"))
            .unwrap()
            .find('实')
            .unwrap();
        let b_col = lines
            .iter()
            .find(|l| l.contains("* "))
            .unwrap()
            .find('*')
            .unwrap();
        assert_eq!(
            h_col,
            b_col,
            "bullet should align with heading:\n{}",
            lines.join("\n")
        );
        for line in &lines {
            let trimmed = line.trim();
            assert!(
                !matches!(trimmed, ")" | "）" | "。" | "，"),
                "width {width} produced orphan closing punctuation line:\n{}",
                lines.join("\n")
            );
        }
    }
}
