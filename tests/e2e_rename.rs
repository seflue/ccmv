mod common;

use assert_cmd::Command;
use std::fs;

#[test]
fn e2e_rename_happy_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, claude_home) = common::setup_claude_project(tmp.path(), "old-name");

    let old_path = project.to_string_lossy().to_string();
    let new_project = tmp.path().join("new-name");
    let new_path = new_project.to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &new_path])
        .assert()
        .success();

    assert!(!project.exists());
    assert!(new_project.exists());

    // Session content updated
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
    let global_dir = claude_home.join("projects").join(&new_encoded);
    assert!(global_dir.exists());
    let session = fs::read_to_string(global_dir.join("abc-session.jsonl")).unwrap();
    assert!(session.contains(&new_path));
}
