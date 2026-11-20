use super::*;
use envoy_agent::agent::delegate_pref::{self, DelegateLevel};

fn tmpdir() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("envoy-slash-delegate-{n}"));
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
