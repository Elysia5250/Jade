use super::*;

fn tmp() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("envoy-worktree-{n}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn git_ok(dir: &Path, args: &[&str]) {
    let output = git(dir, args).unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

fn init_repo() -> PathBuf {
    let root = tmp();
    git_ok(&root, &["init", "-b", "main"]);
    git_ok(&root, &["config", "user.email", "test@example.com"]);
    git_ok(&root, &["config", "user.name", "test"]);
    git_ok(&root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("note.txt"), "base\n").unwrap();
    git_ok(&root, &["add", "note.txt"]);
    git_ok(&root, &["commit", "-m", "base"]);
    root
}

fn head(dir: &Path) -> String {
    git_text(dir, &["rev-parse", "HEAD"]).unwrap()
}

#[test]
fn porcelain_keeps_a_branch_and_a_detached_tree() {
    let trees = parse_worktrees(
        "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\nworktree /side\nHEAD def\ndetached\n",
    );
    assert_eq!(trees.len(), 2);
    assert_eq!(trees[0].branch.as_deref(), Some("main"));
    assert_eq!(trees[1].path, PathBuf::from("/side"));
    assert!(trees[1].branch.is_none());
}

#[test]
fn names_and_paths_stay_beside_the_repository() {
    let main = Path::new("/work/envoy");
    assert_eq!(
        linked_path(main, "menu").unwrap(),
        PathBuf::from("/work/envoy.worktrees/menu")
    );
    assert!(validate_name("feat-menu").is_ok());
    assert!(validate_name("../menu").is_err());
    assert!(validate_name(".git").is_err());
    assert!(validate_name("-menu").is_err());
    let dir = tmp();
    std::fs::write(dir.join(".git"), "x").unwrap();
    assert!(execute(&dir, "side").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn create_is_idempotent_and_remove_keeps_a_dirty_tree() {
    let root = init_repo();
    let entered = execute(&root, "side").unwrap();
    let Effect::Enter { path, resume, .. } = entered else {
        panic!("expected enter");
    };
    assert!(!resume);
    assert_eq!(
        std::fs::read_to_string(path.join("note.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(branch_of(&path).unwrap(), "side");
    let again = execute(&root, "side").unwrap();
    let Effect::Enter { resume, .. } = again else {
        panic!("expected enter");
    };
    assert!(resume);
    std::fs::write(path.join("extra.txt"), "x").unwrap();
    assert!(execute(&root, "remove side").is_err());
    assert!(path.join("extra.txt").is_file());
    std::fs::remove_file(path.join("extra.txt")).unwrap();
    let Effect::Toast(notice) = execute(&root, "remove side").unwrap() else {
        panic!("expected toast");
    };
    assert!(notice.contains("removed"));
    assert!(!path.exists());
    let trees = linked_path(&root, "side").unwrap();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(trees.parent().unwrap());
}

#[test]
fn merge_fast_forwards_and_refuses_a_dirty_source() {
    let root = init_repo();
    let before = head(&root);
    let Effect::Enter { path: side, .. } = execute(&root, "side").unwrap() else {
        panic!("expected enter");
    };
    std::fs::write(side.join("note.txt"), "side\n").unwrap();
    git_ok(&side, &["add", "note.txt"]);
    git_ok(&side, &["commit", "-m", "side"]);
    std::fs::write(side.join("scratch.txt"), "nope").unwrap();
    let err = execute(&side, "merge").unwrap_err();
    assert!(err.contains("commit on side"), "{err}");
    assert_eq!(head(&root), before);
    std::fs::remove_file(side.join("scratch.txt")).unwrap();
    let Effect::Enter {
        path,
        notice,
        error,
        ..
    } = execute(&side, "merge").unwrap()
    else {
        panic!("expected enter");
    };
    assert!(!error);
    assert_eq!(path, canon(&root));
    assert!(notice.contains("merged side into main"), "{notice}");
    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).unwrap(),
        "side\n"
    );
    assert!(side.is_dir());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(linked_path(&root, "side").unwrap().parent().unwrap());
}

#[test]
fn create_and_remove_from_a_subdirectory() {
    let root = init_repo();
    let sub = root.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    let Effect::Enter { path, .. } = execute(&sub, "side").unwrap() else {
        panic!("expected enter");
    };
    assert!(path.join("note.txt").is_file());
    assert_ne!(canon(&path), canon(&root));
    let Effect::Toast(notice) = execute(&sub, "remove side").unwrap() else {
        panic!("expected toast");
    };
    assert!(notice.contains("removed"), "{notice}");
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(linked_path(&root, "side").unwrap().parent().unwrap());
}

#[test]
fn merge_conflict_stays_in_progress_on_the_main_tree() {
    let root = init_repo();
    let Effect::Enter { path: side, .. } = execute(&root, "topic").unwrap() else {
        panic!("expected enter");
    };
    std::fs::write(root.join("note.txt"), "main-line\n").unwrap();
    git_ok(&root, &["add", "note.txt"]);
    git_ok(&root, &["commit", "-m", "main"]);
    std::fs::write(side.join("note.txt"), "side-line\n").unwrap();
    git_ok(&side, &["add", "note.txt"]);
    git_ok(&side, &["commit", "-m", "side"]);
    let Effect::Enter { path, error, .. } = execute(&root, "merge topic").unwrap() else {
        panic!("expected enter");
    };
    assert!(error);
    assert_eq!(path, canon(&root));
    let note = std::fs::read_to_string(root.join("note.txt")).unwrap();
    assert!(note.contains("<<<<<<<"), "{note}");
    assert!(root.join(".git").join("MERGE_HEAD").is_file());
    assert!(side.is_dir());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(linked_path(&root, "topic").unwrap().parent().unwrap());
}

#[test]
fn list_and_merge_work_from_linked_worktree_subdirectory() {
    let root = init_repo();
    let Effect::Enter { path: side, .. } = execute(&root, "side").unwrap() else {
        panic!("expected enter");
    };
    let sub = side.join("nested/dir");
    std::fs::create_dir_all(&sub).unwrap();
    let Effect::Listed(listing) = execute(&sub, "").unwrap() else {
        panic!("expected list");
    };
    let starred = listing
        .lines()
        .find(|l| l.starts_with('*'))
        .expect("starred line");
    assert!(starred.contains("side"), "{listing}");

    std::fs::write(sub.join("f.txt"), "hi\n").unwrap();
    git_ok(&side, &["add", "nested/dir/f.txt"]);
    git_ok(&side, &["commit", "-m", "sub commit"]);

    let Effect::Enter { path, .. } = execute(&sub, "merge").unwrap() else {
        panic!("expected merge enter");
    };
    assert_eq!(path, canon(&root));
    assert_eq!(
        std::fs::read_to_string(root.join("nested/dir/f.txt")).unwrap(),
        "hi\n"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(linked_path(&root, "side").unwrap().parent().unwrap());
}

#[test]
fn main_root_succeeds_inside_a_git_submodule() {
    let parent = init_repo();
    let sub_src = init_repo();
    git_ok(
        &parent,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &sub_src.display().to_string(),
            "deps/sub",
        ],
    );
    let sub_checkout = parent.join("deps/sub");
    let root = main_root(&sub_checkout).expect("submodule main_root");
    assert_eq!(root, canon(&sub_checkout));
    let Effect::Enter { path: tree, .. } = execute(&sub_checkout, "subwt").unwrap() else {
        panic!("expected sub worktree");
    };
    assert_eq!(main_root(&tree).unwrap(), canon(&sub_checkout));
    let _ = execute(&sub_checkout, "remove subwt");
    let _ = std::fs::remove_dir_all(&parent);
    let _ = std::fs::remove_dir_all(&sub_src);
}
