mod common;

use std::fmt::Write as _;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

/// Twelve projects and a batch file moving each into `dest/`. Twelve is past
/// the point where the output is cut short, which is what the `-v` test needs.
fn setup_twelve(tmp: &Path) -> String {
    std::fs::create_dir_all(tmp.join("dest")).unwrap();
    let mut lines = String::new();
    for i in 0..12 {
        let (source, _) = common::setup_claude_project(tmp, &format!("proj{i}"));
        let target = tmp.join("dest").join(format!("proj{i}"));
        writeln!(
            lines,
            "{}\t{}",
            source.to_string_lossy(),
            target.to_string_lossy()
        )
        .unwrap();
    }
    let list = tmp.join("moves.tsv");
    std::fs::write(&list, lines).unwrap();
    list.to_string_lossy().to_string()
}

#[test]
fn e2e_batch_dry_run_prints_the_plan_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let list = setup_twelve(tmp.path());
    let history = tmp.path().join(".claude/history.jsonl");
    let before = std::fs::read_to_string(&history).unwrap();

    let output = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--dry-run", "--no-backup", "--batch", &list])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);

    assert!(stdout.contains("move 12 projects"), "{stdout}");
    assert!(stdout.contains("proj0"), "{stdout}");
    assert!(
        stdout.contains("... (2 more, use -v for all)"),
        "the move list is cut short: {stdout}"
    );
    // The file list is far longer than the move list and gets the same
    // treatment; without it the summary above drowns in per-file lines.
    assert_eq!(
        stdout.matches("more, use -v for all").count(),
        2,
        "both lists are cut short: {stdout}"
    );

    assert_eq!(std::fs::read_to_string(&history).unwrap(), before);
    assert!(tmp.path().join("proj0").is_dir(), "source must stay put");
    assert!(!tmp.path().join("dest/proj0").exists());
}

#[test]
fn e2e_batch_verbose_prints_every_move() {
    let tmp = tempfile::tempdir().unwrap();
    let list = setup_twelve(tmp.path());

    let output = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--dry-run", "--no-backup", "--verbose", "--batch", &list])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);

    for i in 0..12 {
        assert!(stdout.contains(&format!("proj{i} ->")), "proj{i}: {stdout}");
    }
    assert!(!stdout.contains("more, use -v"), "{stdout}");
}

/// The whole point of the batch: every project ends up consistent, and the
/// two files they all share are rewritten for the plan as a whole.
#[test]
fn e2e_batch_moves_every_project_and_keeps_references_consistent() {
    let tmp = tempfile::tempdir().unwrap();
    let list = setup_twelve(tmp.path());

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", "--batch", &list])
        .assert()
        .success()
        .stdout(predicate::str::contains("move 12 projects"));

    let history = std::fs::read_to_string(tmp.path().join(".claude/history.jsonl")).unwrap();
    for i in 0..12 {
        let target = tmp.path().join("dest").join(format!("proj{i}"));
        assert!(target.is_dir(), "{} must exist", target.display());
        assert!(!tmp.path().join(format!("proj{i}")).exists());

        let target_str = target.to_string_lossy().to_string();
        assert!(history.contains(&target_str), "history misses {target_str}");

        let encoded: String = target_str
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
            tmp.path()
                .join(".claude/projects")
                .join(&encoded)
                .join("abc-session.jsonl"),
        )
        .unwrap();
        assert!(session.contains(&target_str), "session misses {target_str}");
    }
}
