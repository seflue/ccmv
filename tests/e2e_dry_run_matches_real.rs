mod common;

use assert_cmd::Command;

/// Proves dry-run and real-run report the same files and replacement counts.
/// This test prevents future regressions where someone might introduce
/// a separate dry-run code path that diverges from the real execution.
#[test]
fn e2e_dry_run_matches_real_run() {
    // Run 1: dry-run — capture report
    let tmp1 = tempfile::tempdir().unwrap();
    let (project1, _) = common::setup_claude_project(tmp1.path(), "test_project");
    let old_path1 = project1.to_string_lossy().to_string();
    let new_path1 = tmp1
        .path()
        .join("moved_project")
        .to_string_lossy()
        .to_string();

    let dry_output = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp1.path())
        .args(["--dry-run", "--no-backup", &old_path1, &new_path1])
        .output()
        .unwrap();
    let dry_stdout = String::from_utf8_lossy(&dry_output.stdout);

    // Run 2: real run with identical setup — capture report
    let tmp2 = tempfile::tempdir().unwrap();
    let (project2, _) = common::setup_claude_project(tmp2.path(), "test_project");
    let old_path2 = project2.to_string_lossy().to_string();
    let new_path2 = tmp2
        .path()
        .join("moved_project")
        .to_string_lossy()
        .to_string();

    let real_output = Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp2.path())
        .args(["--no-backup", &old_path2, &new_path2])
        .output()
        .unwrap();
    assert!(
        real_output.status.success(),
        "real run failed: {}",
        String::from_utf8_lossy(&real_output.stderr)
    );
    let real_stdout = String::from_utf8_lossy(&real_output.stdout);

    // Extract "Files updated: N" and all "(X replacements)" lines from both outputs
    // They should report the same number of files and same replacement counts
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

    // Compare replacement counts (file paths will differ due to different tmpdirs,
    // but counts should be identical for each position)
    for (i, (dry_line, real_line)) in dry_files.iter().zip(real_files.iter()).enumerate() {
        let dry_count = extract_replacement_count(dry_line);
        let real_count = extract_replacement_count(real_line);
        assert_eq!(
            dry_count, real_count,
            "File {i} has different replacement counts: dry={dry_count}, real={real_count}"
        );
    }
}

fn extract_replacement_count(line: &str) -> &str {
    // Line format: "  /path/to/file (N replacements)"
    let start = line.rfind('(').unwrap() + 1;
    let end = line.rfind(' ').unwrap();
    &line[start..end]
}
