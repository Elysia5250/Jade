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
