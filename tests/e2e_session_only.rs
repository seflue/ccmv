mod common;

use assert_cmd::Command;
use flate2::read::GzDecoder;
use std::fs;
use std::path::Path;
use tar::Archive;

/// Helper: encode a path the same way ccmv does (alphanumeric + '-' kept,
/// everything else replaced with '-').
fn encode(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Reads a directory tree into a sorted Vec of (`relative_path`, content).
/// Used to assert directory contents are unchanged.
fn snapshot_dir(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    collect(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            let content = fs::read(&path).unwrap();
            out.push((rel, content));
        }
    }
}

/// Worktree → Main scenario: only session data moves, both project
/// directories remain on disk and unchanged.
#[test]
fn session_only_worktree_to_main() {
    let tmp = tempfile::tempdir().unwrap();

    // Source = worktree-style path with full Claude data
    let (worktree, claude_home) =
        common::setup_claude_project(tmp.path(), ".claude/worktrees/branch1/myproject");
    let src = worktree.to_string_lossy().to_string();

    // Target = independent project dir with its own unrelated content
    let main_proj = tmp.path().join("myproject");
    fs::create_dir_all(main_proj.join("src")).unwrap();
    fs::write(main_proj.join("README.md"), b"main repo readme").unwrap();
    fs::write(main_proj.join("src/main.rs"), b"fn main() {}").unwrap();
    let tgt = main_proj.to_string_lossy().to_string();

    // Snapshots before the run
    let worktree_before = snapshot_dir(&worktree);
    let main_before = snapshot_dir(&main_proj);

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", "--session-only", &src, &tgt])
        .assert()
        .success();

    // Source global gone, target global present
    let src_global = claude_home.join("projects").join(encode(&worktree));
    let tgt_global = claude_home.join("projects").join(encode(&main_proj));
    assert!(!src_global.exists(), "source global dir should be removed");
    assert!(tgt_global.exists(), "target global dir should exist");
    assert!(tgt_global.join("abc-session.jsonl").exists());
    assert!(tgt_global.join("sessions-index.json").exists());
    assert!(
        tgt_global
            .join("abc-session/subagents/agent-xyz.jsonl")
            .exists()
    );

    // Session jsonl cwd rewritten to target path
    let session = fs::read_to_string(tgt_global.join("abc-session.jsonl")).unwrap();
    assert!(session.contains(&tgt));
    assert!(!session.contains(&src));

    // Sessions index points to target
    let index = fs::read_to_string(tgt_global.join("sessions-index.json")).unwrap();
    assert!(index.contains(&tgt));
    assert!(!index.contains(&src));

    // Subagent jsonl cwd rewritten too
    let sub = fs::read_to_string(tgt_global.join("abc-session/subagents/agent-xyz.jsonl")).unwrap();
    assert!(sub.contains(&tgt));
    assert!(!sub.contains(&src));

    // History updated
    let history = fs::read_to_string(claude_home.join("history.jsonl")).unwrap();
    assert!(history.contains(&tgt));
    assert!(!history.contains(&src));

    // .claude.json: trust key renamed
    let claude_json = fs::read_to_string(tmp.path().join(".claude.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).unwrap();
    assert!(parsed.get(&tgt).is_some(), "target trust key missing");
    assert!(parsed.get(&src).is_none(), "source trust key still present");

    // Source worktree dir untouched
    assert_eq!(
        snapshot_dir(&worktree),
        worktree_before,
        "source worktree dir was modified"
    );

    // Target dir untouched (no foreign files added)
    assert_eq!(
        snapshot_dir(&main_proj),
        main_before,
        "target main dir was modified"
    );
}

/// Dry-run reports the same files+replacement counts as the real run.
#[test]
fn session_only_dry_run_matches_real() {
    // Run 1: dry-run
    let tmp1 = tempfile::tempdir().unwrap();
    let (src1, _) = common::setup_claude_project(tmp1.path(), ".claude/worktrees/wt/myapp");
    let tgt1 = tmp1.path().join("myapp");
    fs::create_dir_all(&tgt1).unwrap();

    let dry_out = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp1.path())
        .args([
            "--dry-run",
            "--no-backup",
            "--session-only",
            &src1.to_string_lossy(),
            &tgt1.to_string_lossy(),
        ])
        .output()
        .unwrap();
    assert!(
        dry_out.status.success(),
        "dry run failed: {}",
        String::from_utf8_lossy(&dry_out.stderr)
    );
    let dry_stdout = String::from_utf8_lossy(&dry_out.stdout);

    // Run 2: real run with identical setup
    let tmp2 = tempfile::tempdir().unwrap();
    let (src2, _) = common::setup_claude_project(tmp2.path(), ".claude/worktrees/wt/myapp");
    let tgt2 = tmp2.path().join("myapp");
    fs::create_dir_all(&tgt2).unwrap();

    let real_out = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp2.path())
        .args([
            "--no-backup",
            "--session-only",
            &src2.to_string_lossy(),
            &tgt2.to_string_lossy(),
        ])
        .output()
        .unwrap();
    assert!(
        real_out.status.success(),
        "real run failed: {}",
        String::from_utf8_lossy(&real_out.stderr)
    );
    let real_stdout = String::from_utf8_lossy(&real_out.stdout);

    // Both must report session-only as the action
    assert!(dry_stdout.contains("Action: session-only"));
    assert!(real_stdout.contains("Action: session-only"));

    let dry_files: Vec<&str> = dry_stdout
        .lines()
        .filter(|l| l.contains("replacements)"))
        .collect();
    let real_files: Vec<&str> = real_stdout
        .lines()
        .filter(|l| l.contains("replacements)"))
        .collect();

    assert_eq!(
        dry_files.len(),
        real_files.len(),
        "dry-run reported {} files but real run reported {}.\nDry:\n{}\nReal:\n{}",
        dry_files.len(),
        real_files.len(),
        dry_stdout,
        real_stdout
    );

    for (i, (dry_line, real_line)) in dry_files.iter().zip(real_files.iter()).enumerate() {
        let dry_count = extract_replacement_count(dry_line);
        let real_count = extract_replacement_count(real_line);
        assert_eq!(
            dry_count, real_count,
            "file {i} replacement counts differ: dry={dry_count}, real={real_count}"
        );
    }
}

fn extract_replacement_count(line: &str) -> &str {
    let start = line.rfind('(').unwrap() + 1;
    let end = line.rfind(' ').unwrap();
    &line[start..end]
}

/// Bug 2: in session-only mode, the backup must only archive `global/` —
/// not `local/.claude/`, `local/.mcp.json`, or anything from the source
/// project tree. Spec: the source tree is left untouched, so backing it up
/// is wrong (and dangerous: local `.claude/` can hold broken symlinks).
#[test]
fn session_only_backup_excludes_local_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (worktree, _claude_home) =
        common::setup_claude_project(tmp.path(), ".claude/worktrees/wt/myapp");
    let main_proj = tmp.path().join("myapp");
    fs::create_dir_all(&main_proj).unwrap();

    let src = worktree.to_string_lossy().to_string();
    let tgt = main_proj.to_string_lossy().to_string();

    let out = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--session-only", &src, &tgt])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "session-only run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let backup_line = stdout
        .lines()
        .find(|l| l.starts_with("Backup:"))
        .expect("expected 'Backup:' line");
    let backup_path = backup_line.trim_start_matches("Backup:").trim();

    let file = fs::File::open(backup_path).unwrap();
    let dec = GzDecoder::new(file);
    let mut archive = Archive::new(dec);
    let entries: Vec<String> = archive
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect();

    assert!(
        entries.iter().any(|p| p == "manifest.json"),
        "manifest missing: {entries:?}"
    );
    assert!(
        entries.iter().any(|p| p.starts_with("global/")),
        "global/ payload missing: {entries:?}"
    );
    assert!(
        !entries.iter().any(|p| p.starts_with("local/")),
        "session-only backup must not contain local/ entries: {entries:?}"
    );
}

/// Backup created during session-only run can be restored to recover the
/// global session data.
#[test]
fn session_only_backup_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let (worktree, claude_home) =
        common::setup_claude_project(tmp.path(), ".claude/worktrees/wt/myapp");
    let main_proj = tmp.path().join("myapp");
    fs::create_dir_all(&main_proj).unwrap();

    let src = worktree.to_string_lossy().to_string();
    let tgt = main_proj.to_string_lossy().to_string();

    let src_global = claude_home.join("projects").join(encode(&worktree));
    let tgt_global = claude_home.join("projects").join(encode(&main_proj));

    // Run session-only (with backup enabled)
    let out = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--session-only", &src, &tgt])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "session-only run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Backup line printed in report
    let backup_line = stdout
        .lines()
        .find(|l| l.starts_with("Backup:"))
        .expect("expected 'Backup:' line in output");
    let backup_path = backup_line.trim_start_matches("Backup:").trim().to_owned();
    assert!(
        Path::new(&backup_path).exists(),
        "backup file should exist on disk"
    );

    // After move: source global is gone, target global present
    assert!(!src_global.exists());
    assert!(tgt_global.exists());

    // Wipe BOTH globals to prove restore actually re-creates source-global
    fs::remove_dir_all(&tgt_global).unwrap();
    assert!(!tgt_global.exists());

    // Restore the backup
    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["restore", &backup_path])
        .assert()
        .success();

    // Backup was created BEFORE rename, so it restores to the source-encoded path
    assert!(
        src_global.join("abc-session.jsonl").exists(),
        "source global session file not restored"
    );
    assert!(
        src_global.join("sessions-index.json").exists(),
        "sessions-index not restored"
    );
    assert!(
        src_global
            .join("abc-session/subagents/agent-xyz.jsonl")
            .exists(),
        "subagent file not restored"
    );

    // Restored session content has original (pre-move) cwd reference
    let restored = fs::read_to_string(src_global.join("abc-session.jsonl")).unwrap();
    assert!(restored.contains(&src));
}

/// Target already has its own global Claude session data; session-only must
/// merge the source globals into the existing target global without
/// clobbering target-only files. Deferred from Phase 2 (`MockFs` limitation).
#[test]
fn session_only_merges_into_existing_target_global() {
    let tmp = tempfile::tempdir().unwrap();

    // Source: worktree with one session
    let (worktree, claude_home) =
        common::setup_claude_project(tmp.path(), ".claude/worktrees/wt/myapp");

    // Target: separate project that already has its own global session data
    let (main_proj, _) = common::setup_claude_project(tmp.path(), "myapp");

    let src = worktree.to_string_lossy().to_string();
    let tgt = main_proj.to_string_lossy().to_string();

    let src_global = claude_home.join("projects").join(encode(&worktree));
    let tgt_global = claude_home.join("projects").join(encode(&main_proj));

    // Both globals exist, each with their own abc-session.jsonl. Rename the
    // source's session file so the merge does not overwrite target's file
    // by name collision (we want to assert both end up under target).
    fs::rename(
        src_global.join("abc-session.jsonl"),
        src_global.join("worktree-session.jsonl"),
    )
    .unwrap();
    // Move the subagents subdirectory likewise to a unique name
    fs::rename(
        src_global.join("abc-session"),
        src_global.join("worktree-session"),
    )
    .unwrap();

    // Pre-existing target session file content (sentinel for "not clobbered")
    let target_session_before = fs::read_to_string(tgt_global.join("abc-session.jsonl")).unwrap();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", "--session-only", &src, &tgt])
        .assert()
        .success();

    // Source global removed, target global has BOTH sessions
    assert!(!src_global.exists(), "source global should be gone");
    assert!(tgt_global.join("worktree-session.jsonl").exists());
    assert!(tgt_global.join("abc-session.jsonl").exists());
    assert!(
        tgt_global
            .join("worktree-session/subagents/agent-xyz.jsonl")
            .exists()
    );

    // Pre-existing target session file untouched
    let target_session_after = fs::read_to_string(tgt_global.join("abc-session.jsonl")).unwrap();
    assert_eq!(
        target_session_before, target_session_after,
        "pre-existing target session should not be touched"
    );

    // Migrated worktree session has cwd rewritten to target
    let migrated = fs::read_to_string(tgt_global.join("worktree-session.jsonl")).unwrap();
    assert!(
        migrated.contains(&tgt),
        "migrated session cwd should point to target"
    );
    assert!(
        !migrated.contains(&src),
        "migrated session cwd should not still point to source"
    );
}

/// When both globals contain a sessions-index.json, merging must preserve
/// entries from both. Currently the source index overwrites the target
/// index via `std::fs::rename`, so target sessions vanish from the index
/// (files remain on disk — UI inconsistency only, no data loss).
#[test]
fn session_only_merges_sessions_index_preserves_target_entries() {
    let tmp = tempfile::tempdir().unwrap();

    let (worktree, claude_home) =
        common::setup_claude_project(tmp.path(), ".claude/worktrees/wt/myapp");
    let (main_proj, _) = common::setup_claude_project(tmp.path(), "myapp");

    let src = worktree.to_string_lossy().to_string();
    let tgt = main_proj.to_string_lossy().to_string();

    let src_global = claude_home.join("projects").join(encode(&worktree));
    let tgt_global = claude_home.join("projects").join(encode(&main_proj));

    // Give source and target distinct sessionIds so we can detect loss.
    let src_index = format!(
        r#"{{"version":1,"entries":[{{"sessionId":"src-session-uuid","projectPath":"{src}"}}]}}"#
    );
    let tgt_index = format!(
        r#"{{"version":1,"entries":[{{"sessionId":"tgt-session-uuid","projectPath":"{tgt}"}}]}}"#
    );
    fs::write(src_global.join("sessions-index.json"), &src_index).unwrap();
    fs::write(tgt_global.join("sessions-index.json"), &tgt_index).unwrap();

    // Rename source's session-files so file-level merge doesn't collide.
    fs::rename(
        src_global.join("abc-session.jsonl"),
        src_global.join("worktree-session.jsonl"),
    )
    .unwrap();
    fs::rename(
        src_global.join("abc-session"),
        src_global.join("worktree-session"),
    )
    .unwrap();

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["--no-backup", "--session-only", &src, &tgt])
        .assert()
        .success();

    let merged_text = fs::read_to_string(tgt_global.join("sessions-index.json")).unwrap();
    let merged: serde_json::Value =
        serde_json::from_str(&merged_text).expect("merged sessions-index.json must be valid JSON");
    let entries = merged
        .get("entries")
        .and_then(|v| v.as_array())
        .expect("merged index must have `entries` array");

    let ids: Vec<&str> = entries
        .iter()
        .filter_map(|e| e.get("sessionId").and_then(|v| v.as_str()))
        .collect();
    assert!(
        ids.contains(&"tgt-session-uuid"),
        "target sessionId lost during merge; ids = {ids:?}"
    );
    assert!(
        ids.contains(&"src-session-uuid"),
        "source sessionId missing after merge; ids = {ids:?}"
    );

    // Source path in source-entry must have been rewritten to target path.
    assert!(
        !merged_text.contains(&src),
        "merged index still contains old source path"
    );
}
