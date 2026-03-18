mod common;

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;

#[test]
fn e2e_backup_creates_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, claude_home) = common::setup_claude_project(tmp.path(), "myproject");
    let path = project.to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["backup", &path])
        .assert()
        .success()
        .stdout(predicate::str::contains("Backup:"));

    // Check backup file exists
    let backup_dir = claude_home.join("backups/ccmv");
    assert!(backup_dir.exists());
    let entries: Vec<_> = fs::read_dir(&backup_dir).unwrap().collect();
    assert_eq!(entries.len(), 1);
    let backup_file = entries[0].as_ref().unwrap().path();
    assert!(backup_file.to_string_lossy().ends_with(".tar.gz"));
}

#[test]
fn e2e_backup_restore_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, claude_home) = common::setup_claude_project(tmp.path(), "myproject");
    let path = project.to_string_lossy().to_string();

    let encoded: String = path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let global_dir = claude_home.join("projects").join(&encoded);

    // Create backup
    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["backup", &path])
        .assert()
        .success();

    let backup_dir = claude_home.join("backups/ccmv");
    let backup_file = fs::read_dir(&backup_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();

    // Delete Claude data
    fs::remove_dir_all(&global_dir).unwrap();
    fs::remove_dir_all(project.join(".claude")).unwrap();
    fs::remove_file(project.join(".mcp.json")).unwrap();
    assert!(!global_dir.exists());

    // Restore
    let backup_str = backup_file.to_string_lossy().to_string();
    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["restore", &backup_str])
        .assert()
        .success();

    // Verify restored files
    assert!(
        global_dir.join("abc-session.jsonl").exists(),
        "session file not restored"
    );
    assert!(
        global_dir.join("sessions-index.json").exists(),
        "sessions-index not restored"
    );
    assert!(
        global_dir
            .join("abc-session/subagents/agent-xyz.jsonl")
            .exists(),
        "subagent file not restored"
    );
    assert!(
        project.join(".claude/settings.json").exists(),
        "settings not restored"
    );
    assert!(project.join(".mcp.json").exists(), ".mcp.json not restored");

    // Content should be intact
    let session = fs::read_to_string(global_dir.join("abc-session.jsonl")).unwrap();
    assert!(
        session.contains(&path),
        "restored session should contain project path"
    );
}
