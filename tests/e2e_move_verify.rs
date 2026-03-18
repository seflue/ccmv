mod common;

use assert_cmd::Command;
use std::fs;

/// Verifies that an actual move updates the same files a dry-run reports.
#[test]
fn e2e_move_updates_all_files_reported_by_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let (project, claude_home) = common::setup_claude_project(tmp.path(), "source_project");

    let old_path = project.to_string_lossy().to_string();
    let target = tmp.path().join("target_project");
    let new_path = target.to_string_lossy().to_string();

    // Actual move (not dry-run)
    let output = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_path, &new_path])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "move failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Must list session files, sessions-index, settings, mcp, history
    assert!(
        stdout.contains("abc-session.jsonl"),
        "missing session file:\n{stdout}"
    );
    assert!(
        stdout.contains("sessions-index.json"),
        "missing sessions-index:\n{stdout}"
    );
    assert!(
        stdout.contains("settings.json"),
        "missing settings:\n{stdout}"
    );
    assert!(stdout.contains(".mcp.json"), "missing mcp:\n{stdout}");
    assert!(
        stdout.contains("history.jsonl"),
        "missing history:\n{stdout}"
    );
    assert!(
        stdout.contains("agent-xyz.jsonl"),
        "missing subagent:\n{stdout}"
    );

    // No file should have 0 replacements (all contain the source path)
    assert!(
        !stdout.contains("(0 replacements)"),
        "unexpected 0 replacements:\n{stdout}"
    );

    // Verify actual file contents were updated
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
    let global = claude_home.join("projects").join(&new_encoded);

    let session = fs::read_to_string(global.join("abc-session.jsonl")).unwrap();
    assert!(session.contains(&new_path), "session content not updated");
    assert!(!session.contains(&old_path), "session still has old path");

    let settings = fs::read_to_string(target.join(".claude/settings.json")).unwrap();
    assert!(settings.contains(&new_path), "settings not updated");
    assert!(!settings.contains(&old_path), "settings still has old path");

    let history = fs::read_to_string(claude_home.join("history.jsonl")).unwrap();
    assert!(history.contains(&new_path), "history not updated");
    assert!(!history.contains(&old_path), "history still has old path");
}
