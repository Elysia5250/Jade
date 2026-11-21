use super::*;

fn tmpdir() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("jade-slash-worktree-{n}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn a_rejected_worktree_command_keeps_the_session() {
    let dir = tmpdir();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    let id = app.session.current.id.clone();
    app.run_slash("/worktree ../side");
    assert_eq!(app.session.current.id, id);
    assert_eq!(app.session.current.cwd, dir);
    app.agent.running = true;
    app.run_slash("/worktree side");
    assert_eq!(app.session.current.id, id);
    assert_eq!(
        app.ui.toast.as_ref().map(|item| item.0.as_str()),
        Some("wait for the active turn to finish")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
