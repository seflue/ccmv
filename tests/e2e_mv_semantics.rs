mod common;

use assert_cmd::Command;
use std::fs;

#[test]
fn e2e_move_into_existing_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, claude_home) = common::setup_claude_project(tmp.path(), "myproject");

    let old_path = project.to_string_lossy().to_string();

    // Target is an existing directory — mv-semantics should append project name
    let target_dir = tmp.path().join("destination");
    fs::create_dir_all(&target_dir).unwrap();
    let target_dir_str = target_dir.to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &target_dir_str])
        .assert()
        .success();

    // Project should be at destination/myproject (not destination itself)
    let expected_target = target_dir.join("myproject");
    assert!(!project.exists(), "source should be gone");
    assert!(
        expected_target.exists(),
        "project should be at {}",
        expected_target.display()
    );
    assert!(expected_target.join(".claude/settings.json").exists());

    // Session files should reference the new path
    let new_path = expected_target.to_string_lossy().to_string();
    let new_encoded: String = new_path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let session = fs::read_to_string(
        claude_home
            .join("projects")
            .join(&new_encoded)
            .join("abc-session.jsonl"),
    )
    .unwrap();
    assert!(
        session.contains(&new_path),
        "session should reference new path"
    );
    assert!(
        !session.contains(&old_path),
        "session should not reference old path"
    );
}

#[test]
fn e2e_move_with_dotdot_in_target() {
    let tmp = tempfile::tempdir().unwrap();
    // Project at sub/dir/myproject
    let (project, claude_home) = common::setup_claude_project(tmp.path(), "sub/dir/myproject");

    let old_path = project.to_string_lossy().to_string();
    // Target uses ".." — should resolve to sub/myproject
    let target_with_dots = tmp.path().join("sub/dir/../myproject");
    let target_str = target_with_dots.to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &target_str])
        .assert()
        .success();

    // Project should be at sub/myproject (normalized, no ".." in path)
    let expected = tmp.path().join("sub/myproject");
    assert!(
        expected.exists(),
        "project should be at {}",
        expected.display()
    );
    assert!(!project.exists(), "source should be gone");

    // Global dir should NOT contain "----" (the dotdot encoding bug)
    let new_path = expected.to_string_lossy().to_string();
    let new_encoded: String = new_path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    assert!(
        !new_encoded.contains("----"),
        "encoded path should not contain ---- (dotdot not normalized): {new_encoded}"
    );
    let global = claude_home.join("projects").join(&new_encoded);
    assert!(
        global.exists(),
        "global dir should exist at {}",
        global.display()
    );

    let session = fs::read_to_string(global.join("abc-session.jsonl")).unwrap();
    assert!(
        session.contains(&new_path),
        "session should reference normalized path"
    );
}
