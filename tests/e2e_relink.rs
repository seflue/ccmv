mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

/// Everything the test needs to know about the fixture once it is built, so
/// the test body itself only asserts.
struct Fixture {
    project_b: PathBuf,
    claude_home: PathBuf,
    self_link: PathBuf,
    absolute_link: PathBuf,
    relative_link: PathBuf,
    dead_link: PathBuf,
    dead_link_target_before: PathBuf,
    textref_file: PathBuf,
    textref_file_before: String,
    target_b: PathBuf,
}

/// Two projects, `a` and `b`, both registered in a `~/.claude.json` stand-in.
/// `a` holds an absolute and a relative Inbound Link into `b`, plus a link
/// that was already dead before the Move. `b` holds a self-referential
/// absolute link, and `a` holds a Text Reference to `b`'s path.
fn build_fixture(tmp: &Path) -> Fixture {
    let (project_a, claude_home) = common::setup_claude_project(tmp, "a");
    let (project_b, _) = common::setup_claude_project(tmp, "b");

    // The self-referential link's real target, inside b itself.
    fs::create_dir_all(project_b.join(".claude/lib")).unwrap();
    fs::write(project_b.join(".claude/lib/real-tool"), "tool content").unwrap();
    let self_link = project_b.join(".claude/bin-tool");
    symlink(project_b.join(".claude/lib/real-tool"), &self_link).unwrap();

    // a's Inbound Links into b, both under a's Local Claude Directory —
    // only .claude/ trees are Scan Roots by default.
    fs::create_dir_all(project_a.join(".claude/links")).unwrap();
    let absolute_link = project_a.join(".claude/links/absolute-link");
    symlink(project_b.join(".claude/lib/real-tool"), &absolute_link).unwrap();

    let relative_link = project_a.join(".claude/links/relative-link");
    // a/.claude/links -> up three (links, .claude, a) to tmp, then into b.
    symlink("../../../b/.claude/lib/real-tool", &relative_link).unwrap();

    // A link into b that was already dead before the Move — its target was
    // never created.
    let dead_link = project_a.join(".claude/links/dead-link");
    symlink(project_b.join(".claude/lib/missing-tool"), &dead_link).unwrap();
    let dead_link_target_before = fs::read_link(&dead_link).unwrap();

    // a's Text Reference to b's path — a file that is not itself part of a
    // moving Project's source tree.
    let textref_file = project_a.join(".claude/notes.txt");
    let b_str = project_b.to_string_lossy().to_string();
    fs::write(&textref_file, format!("see {b_str}/README for details")).unwrap();
    let textref_file_before = fs::read_to_string(&textref_file).unwrap();

    let target_b = tmp.join("b-moved");

    Fixture {
        project_b,
        claude_home,
        self_link,
        absolute_link,
        relative_link,
        dead_link,
        dead_link_target_before,
        textref_file,
        textref_file_before,
        target_b,
    }
}

/// The absolute and relative Inbound Links, and the self-referential link
/// inside b, must all end up pointing at b's new location.
fn assert_live_links_repointed(fx: &Fixture) {
    assert_eq!(
        fs::read_link(&fx.absolute_link).unwrap(),
        fx.target_b.join(".claude/lib/real-tool")
    );

    let relative_raw = fs::read_link(&fx.relative_link).unwrap();
    assert!(
        relative_raw.is_relative(),
        "expected a relative target, got {}",
        relative_raw.display()
    );
    let relative_resolved = fs::canonicalize(&fx.relative_link).unwrap();
    assert_eq!(
        relative_resolved,
        fs::canonicalize(fx.target_b.join(".claude/lib/real-tool")).unwrap()
    );

    // The self-referential link moved with b and was repointed at its new
    // path; the old path is gone along with the rest of the project dir.
    assert!(!fx.self_link.exists());
    let self_link_new = fx.target_b.join(".claude/bin-tool");
    assert_eq!(
        fs::read_link(&self_link_new).unwrap(),
        fx.target_b.join(".claude/lib/real-tool")
    );
}

/// A link that was dead before the Move, and a file holding a Text
/// Reference, must both come out byte-identical — neither is rewritten.
fn assert_dead_link_and_textref_untouched(fx: &Fixture) {
    assert_eq!(
        fs::read_link(&fx.dead_link).unwrap(),
        fx.dead_link_target_before
    );
    assert_eq!(
        fs::read_to_string(&fx.textref_file).unwrap(),
        fx.textref_file_before
    );
}

/// The Relink Log and the Text Reference report both land next to the backup
/// archive, the Relink Log sharing its `{encoded}-{timestamp}` name — see
/// the plan's "derselbe Name und Zeitstempel wie das Archiv". One Relink Log
/// row per repointed link: the absolute and relative links in a, and the
/// self-referential link in b — the dead link is excluded.
fn assert_artifacts_written(fx: &Fixture) {
    let backups_dir = fx.claude_home.join("backups/ccmv");
    let entries: Vec<_> = fs::read_dir(&backups_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();

    let archive = entries
        .iter()
        .find(|p| p.to_string_lossy().ends_with(".tar.gz"))
        .expect("backup archive must exist");
    let archive_stem = archive
        .file_name()
        .unwrap()
        .to_string_lossy()
        .strip_suffix(".tar.gz")
        .unwrap()
        .to_string();

    let relink_log = entries
        .iter()
        .find(|p| p.to_string_lossy().ends_with(".relink.tsv"))
        .expect("relink log must exist");
    let relink_log_stem = relink_log
        .file_name()
        .unwrap()
        .to_string_lossy()
        .strip_suffix(".relink.tsv")
        .unwrap()
        .to_string();
    assert_eq!(
        relink_log_stem, archive_stem,
        "relink log must share the backup archive's {{encoded}}-{{timestamp}} name"
    );

    let relink_log_content = fs::read_to_string(relink_log).unwrap();
    let relink_rows: Vec<&str> = relink_log_content.lines().collect();
    assert_eq!(
        relink_rows.len(),
        3,
        "expected one row per repointed link: {relink_log_content}"
    );

    let textrefs_log = entries
        .iter()
        .find(|p| p.to_string_lossy().ends_with(".textrefs.tsv"))
        .expect("text references report must exist");
    let textrefs_content = fs::read_to_string(textrefs_log).unwrap();
    assert!(
        textrefs_content.contains(&fx.project_b.to_string_lossy().to_string()),
        "the Text Reference report must record b's old path: {textrefs_content}"
    );
    assert!(
        textrefs_content.contains(&fx.target_b.to_string_lossy().to_string()),
        "the Text Reference report must record b's new path: {textrefs_content}"
    );
}

/// Moving b with `--relink` — backups on, closing the gap every relink unit
/// test's hard-coded `no_backup: true` left open — must repoint every live
/// Inbound Link, leave the dead one and the Text Reference's file untouched,
/// and write both the Relink Log and the Text Reference report.
#[test]
fn e2e_relink_repoints_inbound_links_across_a_real_filesystem() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = build_fixture(tmp.path());

    Command::cargo_bin("ccmv")
        .unwrap()
        .env("HOME", tmp.path())
        .args([
            "--relink",
            &fx.project_b.to_string_lossy(),
            &fx.target_b.to_string_lossy(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Text references: 1"));

    assert_live_links_repointed(&fx);
    assert_dead_link_and_textref_untouched(&fx);
    assert_artifacts_written(&fx);
}
