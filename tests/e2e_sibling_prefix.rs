mod common;

use assert_cmd::Command;
use std::fs;

fn encode(path: &str) -> String {
    path.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[test]
fn e2e_move_does_not_touch_prefix_sibling_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, claude_home) = common::setup_claude_project(tmp.path(), "hyprland");
    let (sibling_kde, _) = common::setup_claude_project(tmp.path(), "hyprland_kde-apps");
    let (sibling_old, _) = common::setup_claude_project(tmp.path(), "hyprland-old");

    let old_path = project.to_string_lossy().to_string();
    let sibling_kde_path = sibling_kde.to_string_lossy().to_string();
    let sibling_old_path = sibling_old.to_string_lossy().to_string();
    let target = tmp.path().join("moved/hyprland");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let new_path = target.to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &new_path])
        .assert()
        .success();

    // Moved project's refs updated
    assert!(!project.exists());
    assert!(target.exists());
    let new_encoded = encode(&new_path);
    let session = fs::read_to_string(
        claude_home
            .join("projects")
            .join(&new_encoded)
            .join("abc-session.jsonl"),
    )
    .unwrap();
    assert!(session.contains(&new_path));
    assert!(!session.contains(&old_path));

    // Siblings' project directories are untouched
    assert!(sibling_kde.exists());
    assert!(sibling_old.exists());

    // Siblings' global project dirs still exist under their original encoding
    let sibling_kde_encoded = encode(&sibling_kde_path);
    let sibling_old_encoded = encode(&sibling_old_path);
    assert!(
        claude_home
            .join("projects")
            .join(&sibling_kde_encoded)
            .exists(),
        "kde sibling's global project dir should be untouched"
    );
    assert!(
        claude_home
            .join("projects")
            .join(&sibling_old_encoded)
            .exists(),
        "old sibling's global project dir should be untouched"
    );

    // Siblings' history.jsonl entries still reference their original paths
    let history = fs::read_to_string(claude_home.join("history.jsonl")).unwrap();
    assert!(history.contains(&sibling_kde_path));
    assert!(history.contains(&sibling_old_path));

    // Siblings' .claude.json keys still exist under their original paths
    let claude_json = fs::read_to_string(tmp.path().join(".claude.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).unwrap();
    assert!(
        parsed.get(&sibling_kde_path).is_some(),
        "kde sibling's key should still exist under its original path"
    );
    assert!(
        parsed.get(&sibling_old_path).is_some(),
        "old sibling's key should still exist under its original path"
    );
    assert!(
        parsed.get(&new_path).is_some(),
        "moved project key should exist under new path"
    );
    assert!(
        parsed.get(&old_path).is_none(),
        "moved project's old key should be gone"
    );
}
