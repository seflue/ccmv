mod common;

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn e2e_nonexistent_source_aborts() {
    let tmp = tempfile::tempdir().unwrap();
    // No project setup — source doesn't exist at all
    let source_path = tmp
        .path()
        .join("does-not-exist")
        .to_string_lossy()
        .to_string();
    let target_path = tmp.path().join("target").to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &source_path, &target_path])
        .assert()
        .failure()
        .stderr(predicate::str::contains("source not found"));
}

#[test]
fn e2e_conflict_aborts() {
    let tmp = tempfile::tempdir().unwrap();
    // Same project name in different parent dirs — mv-semantics will make
    // target = dst-dir/myproject (appending source name to existing dir)
    let (source, _) = common::setup_claude_project(tmp.path(), "src-dir/myproject");
    let (_, _) = common::setup_claude_project(tmp.path(), "dst-dir/myproject");

    let source_path = source.to_string_lossy().to_string();
    // Pass parent dir as target — mv-semantics appends "myproject"
    // resulting in dst-dir/myproject which already has Claude data
    let target_path = tmp.path().join("dst-dir").to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &source_path, &target_path])
        .assert()
        .failure()
        .stderr(predicate::str::contains("conflict"));
}

/// A rename that cannot work has to be caught before the first write. The
/// same check covers the two failures that need a real filesystem to
/// reproduce (a second mount, an unwritable directory) and so have no
/// automated test of their own.
#[test]
fn e2e_unusable_target_directory_stops_before_any_write() {
    let tmp = tempfile::tempdir().unwrap();
    let (source, claude_home) = common::setup_claude_project(tmp.path(), "myproject");

    let history = claude_home.join("history.jsonl");
    let before = std::fs::read_to_string(&history).unwrap();

    let source_path = source.to_string_lossy().to_string();
    let target_path = tmp.path().join("no/such/dir").to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &source_path, &target_path])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot move"))
        .stderr(predicate::str::contains("target directory does not exist"));

    assert!(source.exists(), "source must still be in place");
    assert_eq!(
        std::fs::read_to_string(&history).unwrap(),
        before,
        "history.jsonl must not have been touched"
    );
}

#[test]
fn e2e_force_merges_into_existing_target() {
    let tmp = tempfile::tempdir().unwrap();
    let (source, claude_home) = common::setup_claude_project(tmp.path(), "src-dir/myproject");
    let (_, _) = common::setup_claude_project(tmp.path(), "dst-dir/myproject");

    let source_path = source.to_string_lossy().to_string();
    let target_path = tmp.path().join("dst-dir").to_string_lossy().to_string();
    let final_target = tmp.path().join("dst-dir/myproject");
    let final_target_path = final_target.to_string_lossy().to_string();

    // --force should merge, not fail
    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--force", "--no-backup", &source_path, &target_path])
        .assert()
        .success();

    // Source gone, target exists
    assert!(!source.exists(), "source should be gone");
    assert!(final_target.exists(), "target should exist");

    // Session files reference new path
    let new_encoded: String = final_target_path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let session = std::fs::read_to_string(
        claude_home
            .join("projects")
            .join(&new_encoded)
            .join("abc-session.jsonl"),
    )
    .unwrap();
    assert!(
        session.contains(&final_target_path),
        "session should reference new path"
    );
}
