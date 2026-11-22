use super::*;
use jade_agent::agent::delegate_pref::{self, DelegateLevel};
use jade_agent::agent::voice_pref::{self, VoiceStyle};
use std::path::Path;

fn tmpdir() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("jade-slash-delegate-{n}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn slash_delegate_persists_a_level_and_rejects_unknown() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.run_slash("/delegate");
    assert_eq!(delegate_pref::load(&dir), DelegateLevel::Low);
    app.run_slash("/delegate high");
    assert_eq!(delegate_pref::load(&dir), DelegateLevel::High);
    app.run_slash("/delegate nope");
    assert_eq!(delegate_pref::load(&dir), DelegateLevel::High);
    assert_eq!(
        app.ui.toast.as_ref().map(|item| item.0.as_str()),
        Some("usage: /delegate [off|low|normal|high]")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn slash_voice_persists_style_and_rejects_unknown() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.run_slash("/voice");
    assert_eq!(voice_pref::load(&dir), VoiceStyle::Engineer);
    app.run_slash("/voice plain");
    assert_eq!(voice_pref::load(&dir), VoiceStyle::Plain);
    app.run_slash("/style expert");
    assert_eq!(voice_pref::load(&dir), VoiceStyle::Expert);
    app.run_slash("/voice invalid");
    assert_eq!(voice_pref::load(&dir), VoiceStyle::Expert);
    assert_eq!(
        app.ui.toast.as_ref().map(|item| item.0.as_str()),
        Some("usage: /voice [expert|engineer|plain]")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn markdown_code_blocks_soft_wrap_instead_of_truncating() {
    let theme = THEMES[0].1;
    let index = links::WorkspaceIndex::default();
    let md = "```rust\nlet very_long_identifier_name = compute_something_important(alpha, beta);\n```";
    let rendered = markdown::render_markdown_linked(md, 30, &theme, Path::new("/tmp"), &[], &index);
    let rows: Vec<String> = rendered
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert!(rows.iter().any(|r| r.starts_with(" ↳ ")), "{rows:?}");
    let joined: String = rows
        .iter()
        .filter(|r| r.starts_with(" | ") || r.starts_with(" ↳ "))
        .map(|r| r.trim_start_matches(" | ").trim_start_matches(" ↳ "))
        .collect();
    assert!(joined.contains("beta);"), "tail was truncated: {joined}");
}

#[test]
fn markdown_table_truncation_uses_ellipsis_and_bare_urls_highlight() {
    let theme = THEMES[0].1;
    let index = links::WorkspaceIndex::default();
    let table = "| ColA | ColB |\n| --- | --- |\n| verylongvaluehere | anotherverylongvalue |";
    let rendered =
        markdown::render_markdown_linked(table, 22, &theme, Path::new("/tmp"), &[], &index);
    let text: String = rendered
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.span.content.as_ref()))
        .collect();
    assert!(text.contains('…'), "expected ellipsis in table: {text}");

    let spans = links::inline_spans_linked(
        "- [1] Docs — https://example.com/guide（官方文档）",
        &theme,
        Path::new("/tmp"),
        &[],
        &index,
    );
    let url_span = spans
        .iter()
        .find(|s| s.span.content.as_ref() == "https://example.com/guide")
        .expect("bare URL span");
    assert_eq!(url_span.span.style.fg, Some(theme.link));
}
