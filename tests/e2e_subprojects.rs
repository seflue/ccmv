mod common;

use assert_cmd::Command;
use std::fs;

#[test]
fn e2e_move_migrates_subproject() {
    let tmp = tempfile::tempdir().unwrap();

    // Parent project
    let (parent, claude_home) = common::setup_claude_project(tmp.path(), "parent");
    // Sub-project inside the parent
    let (child, _) = common::setup_claude_project(tmp.path(), "parent/child");

    let old_parent = parent.to_string_lossy().to_string();
    let new_parent_dir = tmp.path().join("moved-parent");
    let new_parent = new_parent_dir.to_string_lossy().to_string();
    let old_child = child.to_string_lossy().to_string();
    let new_child = new_parent_dir.join("child").to_string_lossy().to_string();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", &old_parent, &new_parent])
        .assert()
        .success();

    // Parent project moved
    assert!(!parent.exists(), "old parent should be gone");
    assert!(new_parent_dir.exists(), "new parent should exist");

    // Sub-project session files updated
    let child_encoded: String = new_child
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let child_global = claude_home.join("projects").join(&child_encoded);
    assert!(
        child_global.exists(),
        "sub-project global dir should be renamed to new path"
    );

    let session = fs::read_to_string(child_global.join("abc-session.jsonl")).unwrap();
    assert!(
        session.contains(&new_child),
        "sub-project session should reference new path"
    );
    assert!(
        !session.contains(&old_child),
        "sub-project session should not reference old path"
    );

    // Both keys updated in .claude.json
    let claude_json = fs::read_to_string(tmp.path().join(".claude.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).unwrap();

    assert!(
        parsed.get(&new_parent).is_some(),
        "parent key should be updated"
    );
    assert!(
        parsed.get(&new_child).is_some(),
        "child key should be updated"
    );
    assert!(
        parsed.get(&old_parent).is_none(),
        "old parent key should be gone"
    );
    assert!(
        parsed.get(&old_child).is_none(),
        "old child key should be gone"
    );
}
