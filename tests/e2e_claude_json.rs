mod common;

use assert_cmd::Command;
use std::fs;

#[test]
fn e2e_move_updates_claude_json() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, _) = common::setup_claude_project(tmp.path(), "old-project");

    let old_path = project.to_string_lossy().to_string();
    let new_project = tmp.path().join("new-project");
    let new_path = new_project.to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &new_path])
        .assert()
        .success();

    let claude_json = fs::read_to_string(tmp.path().join(".claude.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).unwrap();

    assert!(
        parsed.get(&new_path).is_some(),
        "new path key should exist in .claude.json"
    );
    assert!(
        parsed.get(&old_path).is_none(),
        "old path key should be gone from .claude.json"
    );
    assert_eq!(
        parsed[&new_path]["hasTrustDialogAccepted"],
        serde_json::Value::Bool(true),
        "trust state should be preserved"
    );
}

#[test]
fn e2e_move_preserves_other_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let (project_a, _) = common::setup_claude_project(tmp.path(), "project-a");
    let (_, _) = common::setup_claude_project(tmp.path(), "project-b");

    let old_path = project_a.to_string_lossy().to_string();
    let new_project = tmp.path().join("project-a-moved");
    let new_path = new_project.to_string_lossy().to_string();
    let other_path = tmp.path().join("project-b").to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &new_path])
        .assert()
        .success();

    let claude_json = fs::read_to_string(tmp.path().join(".claude.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).unwrap();

    assert!(parsed.get(&new_path).is_some(), "moved project should have new key");
    assert!(parsed.get(&old_path).is_none(), "old key should be gone");
    assert!(
        parsed.get(&other_path).is_some(),
        "other project should be untouched"
    );
}

#[test]
fn e2e_dry_run_does_not_modify_claude_json() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, _) = common::setup_claude_project(tmp.path(), "myproject");

    let old_path = project.to_string_lossy().to_string();
    let new_path = tmp.path().join("newproject").to_string_lossy().to_string();

    let before = fs::read_to_string(tmp.path().join(".claude.json")).unwrap();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--dry-run", "--no-backup", &old_path, &new_path])
        .assert()
        .success();

    let after = fs::read_to_string(tmp.path().join(".claude.json")).unwrap();
    assert_eq!(before, after, ".claude.json should not change on dry-run");
}
