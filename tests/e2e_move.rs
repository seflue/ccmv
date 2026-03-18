mod common;

use assert_cmd::Command;
use std::fs;

#[test]
fn e2e_move_happy_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, claude_home) = common::setup_claude_project(tmp.path(), "source/myapp");

    let old_path = project.to_string_lossy().to_string();
    let target = tmp.path().join("target/myapp");
    // Parent directory must exist for rename/move to succeed
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let new_path = target.to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &new_path])
        .assert()
        .success();

    // Project moved
    assert!(!project.exists());
    assert!(target.exists());
    assert!(target.join(".claude/settings.json").exists());
    assert!(target.join(".mcp.json").exists());

    // Settings updated
    let settings = fs::read_to_string(target.join(".claude/settings.json")).unwrap();
    assert!(settings.contains(&new_path));
    assert!(!settings.contains(&old_path));

    // Session files updated
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
    assert!(session.contains(&new_path));
    assert!(!session.contains(&old_path));

    // History updated
    let history = fs::read_to_string(claude_home.join("history.jsonl")).unwrap();
    assert!(history.contains(&new_path));
    assert!(!history.contains(&old_path));
}
