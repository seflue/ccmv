mod common;

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn e2e_dry_run_no_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, _) = common::setup_claude_project(tmp.path(), "myproject");

    let old_path = project.to_string_lossy().to_string();
    let new_path = tmp.path().join("newproject").to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--dry-run", "--no-backup", &old_path, &new_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dry run"));

    // Original unchanged
    assert!(project.exists());
    assert!(!tmp.path().join("newproject").exists());
}

#[test]
fn e2e_dry_run_reports_all_files_and_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, _) = common::setup_claude_project(tmp.path(), "myproject");

    let old_path = project.to_string_lossy().to_string();
    let new_path = tmp.path().join("newproject").to_string_lossy().to_string();

    let output = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--dry-run", "--no-backup", &old_path, &new_path])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Must list session file, sessions-index, settings, mcp, history
    assert!(
        stdout.contains("abc-session.jsonl"),
        "missing session file in dry-run output:\n{stdout}"
    );
    assert!(
        stdout.contains("sessions-index.json"),
        "missing sessions-index in dry-run output:\n{stdout}"
    );
    assert!(
        stdout.contains("settings.json"),
        "missing settings.json in dry-run output:\n{stdout}"
    );
    assert!(
        stdout.contains(".mcp.json"),
        "missing .mcp.json in dry-run output:\n{stdout}"
    );
    assert!(
        stdout.contains("history.jsonl"),
        "missing history.jsonl in dry-run output:\n{stdout}"
    );

    // Subagent file
    assert!(
        stdout.contains("agent-xyz.jsonl"),
        "missing subagent file in dry-run output:\n{stdout}"
    );

    // Replacement counts must be > 0 for files that contain the path
    assert!(
        !stdout.contains("(0 replacements)"),
        "dry-run should not report 0 replacements for files containing the path:\n{stdout}"
    );
}

#[test]
fn e2e_dry_run_with_underscores_in_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, _) = common::setup_claude_project(tmp.path(), "my_project_name");

    let old_path = project.to_string_lossy().to_string();
    let new_path = tmp
        .path()
        .join("new_location")
        .to_string_lossy()
        .to_string();

    let output = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--dry-run", "--no-backup", &old_path, &new_path])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Must find session files even with underscores in path
    assert!(
        stdout.contains("abc-session.jsonl"),
        "failed to find session files for underscore path:\n{stdout}"
    );
    assert!(
        !stdout.contains("(0 replacements)"),
        "0 replacements for underscore path:\n{stdout}"
    );
}

#[test]
fn e2e_dry_run_does_not_create_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, claude_home) = common::setup_claude_project(tmp.path(), "myproject");

    let old_path = project.to_string_lossy().to_string();
    let new_path = tmp.path().join("newproject").to_string_lossy().to_string();

    // Note: NO --no-backup flag — backup would normally be created
    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--dry-run", &old_path, &new_path])
        .assert()
        .success();

    // No backup directory should exist
    let backup_dir = claude_home.join("backups/ccmv");
    assert!(
        !backup_dir.exists(),
        "dry-run should not create backup, but found: {}",
        backup_dir.display()
    );
}
