mod common;

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn e2e_idempotent_double_run() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, _) = common::setup_claude_project(tmp.path(), "myproject");
    let path = project.to_string_lossy().to_string();

    // Move to same path = nothing to do
    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &path, &path])
        .assert()
        .success()
        .stdout(predicate::str::contains("Nothing to do"));
}

#[test]
fn e2e_idempotent_rerun_after_completed_move() {
    let tmp = tempfile::tempdir().unwrap();
    // Realistic layout: project in a subdirectory, moved to another parent
    let src_dir = tmp.path().join("old-workspace");
    std::fs::create_dir_all(&src_dir).unwrap();
    let (project, _) = common::setup_claude_project(tmp.path(), "old-workspace/myproject");

    let old_path = project.to_string_lossy().to_string();
    let target_dir = tmp.path().join("new-workspace");
    std::fs::create_dir_all(&target_dir).unwrap();
    let target_dir_str = target_dir.to_string_lossy().to_string();

    // First run: move myproject into new-workspace (mv-semantics)
    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &target_dir_str])
        .assert()
        .success();

    // Verify first run worked
    assert!(!project.exists(), "source should be gone");
    assert!(target_dir.join("myproject").exists(), "target should exist");

    // Second run: same command — source gone, target already migrated
    // mv-semantics resolves target to new-workspace/myproject (same as before)
    // Should succeed with 0 path replacements
    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &target_dir_str])
        .assert()
        .success()
        .stdout(predicate::str::contains("Total path replacements: 0"));
}

#[test]
fn e2e_idempotent_rerun_explicit_target() {
    // User moved manually, then runs ccmv to fix Claude data.
    // Source doesn't exist, target has the project + Claude data from first run.
    let tmp = tempfile::tempdir().unwrap();
    let (project, _) = common::setup_claude_project(tmp.path(), "old-name");

    let old_path = project.to_string_lossy().to_string();
    let new_project = tmp.path().join("new-name");
    let new_path = new_project.to_string_lossy().to_string();

    // First run: migrate
    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &new_path])
        .assert()
        .success();

    assert!(!project.exists());
    assert!(new_project.exists());

    // Second run: explicit paths, source gone, no global dir for source
    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &new_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("Total path replacements: 0"));
}
