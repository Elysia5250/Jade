use super::*;

fn tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = std::env::temp_dir().join(format!(
        "envoy-home-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn select(app: &mut App, row: WelcomeRow) {
    let idx = app
        .welcome
        .as_ref()
        .unwrap()
        .rows
        .iter()
        .position(|item| std::mem::discriminant(item) == std::mem::discriminant(&row))
        .unwrap();
    app.welcome.as_mut().unwrap().selected = idx;
}

#[test]
fn home_rows_keep_open_path_before_the_new_sections() {
    let dir = tmpdir("rows");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    let kinds: Vec<_> = app
        .welcome
        .as_ref()
        .unwrap()
        .rows
        .iter()
        .map(|row| match row {
            WelcomeRow::Here(_, _) => "here",
            WelcomeRow::Recent(_, _) => "recent",
            WelcomeRow::OpenPath => "path",
            WelcomeRow::NewFolder => "folder",
            WelcomeRow::Account => "account",
            WelcomeRow::Skills => "skills",
            WelcomeRow::Memory => "memory",
        })
        .collect();
    assert_eq!(
        kinds,
        ["here", "path", "folder", "account", "skills", "memory"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn new_folder_enters_the_created_directory() {
    let dir = tmpdir("mkdir");
    let parent = dir.join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    let mut app = App::new(dir.clone(), "local".into(), None, parent.clone());
    let launch = app.session.id.clone();
    app.open_welcome();
    select(&mut app, WelcomeRow::NewFolder);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.welcome.as_ref().unwrap().folder_mode);
    app.welcome.as_mut().unwrap().path = parent.display().to_string();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.welcome.as_ref().unwrap().naming);
    for ch in "fresh".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.welcome.is_none());
    assert_ne!(app.session.id, launch);
    assert!(app.session.items.is_empty());
    assert_eq!(
        std::fs::canonicalize(&app.session.cwd).unwrap(),
        std::fs::canonicalize(parent.join("fresh")).unwrap()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn protected_folder_name_stays_on_the_picker() {
    let dir = tmpdir("prot");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    select(&mut app, WelcomeRow::NewFolder);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.welcome.as_mut().unwrap().path = dir.display().to_string();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.welcome.as_mut().unwrap().draft = ".git".into();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let err = app.welcome.as_ref().unwrap().error.clone().unwrap();
    assert!(err.contains("protected"), "{err}");
    assert!(!dir.join(".git").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn account_row_opens_login_above_the_home_screen() {
    let dir = tmpdir("acct");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    select(&mut app, WelcomeRow::Account);
    let selected = app.welcome.as_ref().unwrap().selected;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(app.modal, Some(Modal::Login(_))));
    assert!(app.welcome.is_some());
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(app.welcome.as_ref().unwrap().selected, selected);
    app.handle_paste("/tmp/nope\n");
    assert!(app.welcome.as_ref().unwrap().path.is_empty());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.modal.is_none());
    assert!(app.welcome.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn skill_toggle_writes_prefs_and_new_skill_returns_edit() {
    let dir = tmpdir("skill");
    let skill = dir.join("skills").join("demo");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: demo steps\n---\n\nbody\n",
    )
    .unwrap();
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    select(&mut app, WelcomeRow::Skills);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let idx = app
        .welcome
        .as_ref()
        .unwrap()
        .skills
        .iter()
        .position(|item| item.name == "demo")
        .expect("demo skill");
    app.welcome.as_mut().unwrap().list_selected = idx;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!crate::skills::prefs::injected(&dir, "demo", true));
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    for ch in "extra".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match action {
        Action::EditFile(path) => {
            assert!(path.ends_with("SKILL.md"));
            assert!(std::fs::read_to_string(&path)
                .unwrap()
                .contains("name: extra"));
        }
        _ => panic!("expected EditFile"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn memory_create_edit_toggle_and_delete() {
    let dir = tmpdir("mem");
    let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
    app.open_welcome();
    select(&mut app, WelcomeRow::Memory);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    for ch in "记事".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Action::EditFile(path) = action else {
        panic!("expected EditFile");
    };
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("title: 记事"));
    assert!(raw.contains("enabled: true"));
    assert!(app.welcome.as_ref().unwrap().memory_mode);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.welcome.as_ref().unwrap().notes[0].enabled);
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert!(app.welcome.as_ref().unwrap().notes.is_empty());
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}
