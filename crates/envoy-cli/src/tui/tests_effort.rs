use super::*;
use envoy_agent::agent::effort;

fn tmpdir() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("envoy-slash-effort-{n}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn slash_effort_persists_a_level_and_rejects_unknown() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.run_slash("/effort");
    assert_eq!(effort::load(&dir), "low");
    app.run_slash("/effort xhigh");
    assert_eq!(effort::load(&dir), "xhigh");
    app.run_slash("/effort off");
    assert_eq!(effort::load(&dir), "off");
    app.run_slash("/effort Nope!");
    assert_eq!(effort::load(&dir), "off");
    assert_eq!(
        app.toast.as_ref().map(|item| item.0.as_str()),
        Some("usage: /effort [level]")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
