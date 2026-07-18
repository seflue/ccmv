// Ref updater: updates path references in Claude Code files

use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;

use crate::fs::Fs;

/// Report for a single file update.
#[derive(Debug)]
pub struct UpdateReport {
    pub file: PathBuf,
    pub replacements: usize,
    /// `true` if the file didn't exist or had no matches.
    pub skipped: bool,
}

/// `true` if `rest` (the text following a path match) starts a new path
/// segment rather than continuing the matched one.
///
/// Without this check, moving `/x/proj` would also rewrite `/x/proj_other`.
fn is_segment_boundary(rest: &str) -> bool {
    match rest.chars().next() {
        None | Some('/') => true,
        Some(c) => !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')),
    }
}

/// Replaces occurrences of `old` with `new`, but only where `old` ends on a
/// path-segment boundary. Returns the replacement count and the new content.
fn replace_paths(content: &str, old: &str, new: &str) -> (usize, String) {
    let mut out = String::with_capacity(content.len());
    let mut count = 0;
    let mut rest = content;

    while let Some(idx) = rest.find(old) {
        let after = &rest[idx + old.len()..];
        out.push_str(&rest[..idx]);
        if is_segment_boundary(after) {
            out.push_str(new);
            count += 1;
        } else {
            out.push_str(old);
        }
        rest = after;
    }
    out.push_str(rest);

    (count, out)
}

/// Updates path references in a single file.
///
/// Reads the file, replaces all occurrences of `old_path` with `new_path`,
/// and writes back atomically only if changes were made. Matches that merely
/// share a prefix with a sibling path (`/x/proj` vs `/x/proj_other`) are left
/// alone.
/// When `dry_run` is `true`, counts replacements without writing.
pub fn update_file(
    fs: &dyn Fs,
    file: &Path,
    old_path: &str,
    new_path: &str,
    dry_run: bool,
) -> Result<UpdateReport> {
    if !fs.exists(file) {
        return Ok(UpdateReport {
            file: file.to_path_buf(),
            replacements: 0,
            skipped: true,
        });
    }

    let content = fs.read_to_string(file)?;
    let (count, new_content) = replace_paths(&content, old_path, new_path);

    if count == 0 {
        return Ok(UpdateReport {
            file: file.to_path_buf(),
            replacements: 0,
            skipped: true,
        });
    }

    if !dry_run {
        fs.write_atomically(file, &new_content)?;
    }

    Ok(UpdateReport {
        file: file.to_path_buf(),
        replacements: count,
        skipped: false,
    })
}

/// Updates path references in multiple files in parallel using rayon.
/// When `dry_run` is `true`, counts replacements without writing.
pub fn update_files_parallel(
    fs: &dyn Fs,
    files: &[PathBuf],
    old_path: &str,
    new_path: &str,
    dry_run: bool,
) -> Result<Vec<UpdateReport>> {
    files
        .par_iter()
        .map(|f| update_file(fs, f, old_path, new_path, dry_run))
        .collect()
}

/// Renames the JSON object key `old_prefix` and its path descendants to use
/// `new_prefix`. Sibling keys sharing only a textual prefix are untouched.
///
/// Used for `~/.claude.json` where project paths are top-level keys.
/// Only keys are modified — values remain untouched.
pub fn rename_json_keys(
    fs: &dyn Fs,
    path: &Path,
    old_prefix: &str,
    new_prefix: &str,
    dry_run: bool,
) -> Result<UpdateReport> {
    if !fs.exists(path) {
        return Ok(UpdateReport {
            file: path.to_path_buf(),
            replacements: 0,
            skipped: true,
        });
    }

    let content = fs.read_to_string(path)?;
    let mut root: serde_json::Value = serde_json::from_str(&content)?;

    // Project keys live under "projects" in ~/.claude.json
    let obj = if let Some(projects) = root.get_mut("projects").and_then(|v| v.as_object_mut()) {
        projects
    } else if let Some(top) = root.as_object_mut() {
        // Fallback: top-level keys (for tests / simpler formats)
        top
    } else {
        return Ok(UpdateReport {
            file: path.to_path_buf(),
            replacements: 0,
            skipped: true,
        });
    };

    // Segment boundary: `/old` must not match the sibling key `/old_other`.
    let sub_prefix = format!("{old_prefix}/");
    let renames: Vec<(String, String)> = obj
        .keys()
        .filter(|k| k.as_str() == old_prefix || k.starts_with(&sub_prefix))
        .map(|k| {
            let new_key = format!("{new_prefix}{}", &k[old_prefix.len()..]);
            (k.clone(), new_key)
        })
        .collect();

    let count = renames.len();

    if count == 0 {
        return Ok(UpdateReport {
            file: path.to_path_buf(),
            replacements: 0,
            skipped: true,
        });
    }

    if !dry_run {
        for (old_key, new_key) in &renames {
            if let Some(value) = obj.remove(old_key) {
                obj.insert(new_key.clone(), value);
            }
        }
        let new_content = serde_json::to_string_pretty(&root)?;
        fs.write_atomically(path, &new_content)?;
    }

    Ok(UpdateReport {
        file: path.to_path_buf(),
        replacements: count,
        skipped: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MockFs;
    use std::path::Path;

    #[test]
    fn update_replaces_cwd_in_jsonl() {
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/data/session.jsonl"),
            r#"{"type":"user","cwd":"/old/path","msg":"hello"}
{"type":"assistant","cwd":"/old/path","msg":"hi"}"#,
        );

        let report = update_file(
            &fs,
            Path::new("/data/session.jsonl"),
            "/old/path",
            "/new/path",
            false,
        )
        .unwrap();

        assert_eq!(report.replacements, 2);
        assert!(!report.skipped);

        let content = fs.read_to_string(Path::new("/data/session.jsonl")).unwrap();
        assert!(content.contains("/new/path"));
        assert!(!content.contains("/old/path"));
    }

    #[test]
    fn update_skips_file_without_matches() {
        let fs = MockFs::new();
        fs.add_file(Path::new("/data/other.json"), r#"{"key":"value"}"#);

        let report = update_file(
            &fs,
            Path::new("/data/other.json"),
            "/old/path",
            "/new/path",
            false,
        )
        .unwrap();

        assert!(report.skipped);
        assert_eq!(report.replacements, 0);
        // File should be unchanged (not rewritten)
        assert_eq!(
            fs.read_to_string(Path::new("/data/other.json")).unwrap(),
            r#"{"key":"value"}"#
        );
    }

    #[test]
    fn update_skips_nonexistent_file() {
        let fs = MockFs::new();

        let report = update_file(
            &fs,
            Path::new("/nonexistent"),
            "/old/path",
            "/new/path",
            false,
        )
        .unwrap();

        assert!(report.skipped);
        assert_eq!(report.replacements, 0);
    }

    #[test]
    fn update_handles_settings_json() {
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/project/.claude/settings.json"),
            r#"{"permissions":{"allow":["Bash(/home/user/old-project/.claude/hooks/hook.py:*)"]},"hooks":{"PreToolUse":[{"hooks":[{"command":"python /home/user/old-project/.claude/hooks/redirect.py"}]}]}}"#,
        );

        let report = update_file(
            &fs,
            Path::new("/project/.claude/settings.json"),
            "/home/user/old-project",
            "/home/user/new-project",
            false,
        )
        .unwrap();

        assert_eq!(report.replacements, 2);
        let content = fs
            .read_to_string(Path::new("/project/.claude/settings.json"))
            .unwrap();
        assert!(content.contains("/home/user/new-project"));
        assert!(!content.contains("/home/user/old-project"));
    }

    #[test]
    fn update_files_parallel_processes_multiple() {
        let fs = MockFs::new();
        fs.add_file(Path::new("/a.jsonl"), r#"{"cwd":"/old"}"#);
        fs.add_file(Path::new("/b.jsonl"), r#"{"cwd":"/old"}"#);
        fs.add_file(Path::new("/c.jsonl"), r#"{"key":"value"}"#);

        let files = vec![
            PathBuf::from("/a.jsonl"),
            PathBuf::from("/b.jsonl"),
            PathBuf::from("/c.jsonl"),
        ];

        let reports = update_files_parallel(&fs, &files, "/old", "/new", false).unwrap();

        assert_eq!(reports.len(), 3);
        let updated: Vec<_> = reports.iter().filter(|r| !r.skipped).collect();
        let skipped: Vec<_> = reports.iter().filter(|r| r.skipped).collect();
        assert_eq!(updated.len(), 2);
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn update_sessions_index() {
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/data/sessions-index.json"),
            r#"{"version":1,"entries":[{"fullPath":"/home/user/.claude/projects/-home-user-old/abc.jsonl","projectPath":"/home/user/old"}]}"#,
        );

        let report = update_file(
            &fs,
            Path::new("/data/sessions-index.json"),
            "/home/user/old",
            "/home/user/new",
            false,
        )
        .unwrap();

        assert_eq!(report.replacements, 1); // only projectPath (fullPath uses encoded hyphens)
        let content = fs
            .read_to_string(Path::new("/data/sessions-index.json"))
            .unwrap();
        assert!(content.contains("/home/user/new"));
        assert!(!content.contains("/home/user/old"));
    }

    #[test]
    fn dry_run_counts_but_does_not_write() {
        let fs = MockFs::new();
        let original = r#"{"cwd":"/old/path","data":"test"}
{"cwd":"/old/path","data":"more"}"#;
        fs.add_file(Path::new("/data/session.jsonl"), original);

        let report = update_file(
            &fs,
            Path::new("/data/session.jsonl"),
            "/old/path",
            "/new/path",
            true, // dry_run
        )
        .unwrap();

        // Same count as real run
        assert_eq!(report.replacements, 2);
        assert!(!report.skipped);

        // File content unchanged
        let content = fs.read_to_string(Path::new("/data/session.jsonl")).unwrap();
        assert_eq!(content, original, "dry_run must not modify the file");
    }

    #[test]
    fn rename_json_keys_nested_projects() {
        // Real ~/.claude.json structure: keys are under "projects", not top-level
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/home/.claude.json"),
            r#"{"numStartups":5,"projects":{"/home/user/old-project":{"hasTrustDialogAccepted":true},"/home/user/other":{"hasTrustDialogAccepted":true}}}"#,
        );

        let report = rename_json_keys(
            &fs,
            Path::new("/home/.claude.json"),
            "/home/user/old-project",
            "/home/user/new-project",
            false,
        )
        .unwrap();

        assert_eq!(report.replacements, 1);
        let content = fs.read_to_string(Path::new("/home/.claude.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed["projects"].get("/home/user/new-project").is_some());
        assert!(parsed["projects"].get("/home/user/old-project").is_none());
        assert!(parsed["projects"].get("/home/user/other").is_some());
        // Top-level keys preserved
        assert_eq!(parsed["numStartups"], 5);
    }

    #[test]
    fn rename_json_keys_basic() {
        // Fallback: top-level keys (simpler test format)
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/home/.claude.json"),
            r#"{"/home/user/old-project":{"hasTrustDialogAccepted":true,"allowedTools":["Bash"]},"/home/user/other":{"hasTrustDialogAccepted":true}}"#,
        );

        let report = rename_json_keys(
            &fs,
            Path::new("/home/.claude.json"),
            "/home/user/old-project",
            "/home/user/new-project",
            false,
        )
        .unwrap();

        assert_eq!(report.replacements, 1);
        assert!(!report.skipped);

        let content = fs.read_to_string(Path::new("/home/.claude.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.get("/home/user/new-project").is_some());
        assert!(parsed.get("/home/user/old-project").is_none());
        // Other project untouched
        assert!(parsed.get("/home/user/other").is_some());
        // Value preserved
        assert_eq!(
            parsed["/home/user/new-project"]["hasTrustDialogAccepted"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn rename_json_keys_subprojects() {
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/home/.claude.json"),
            r#"{"/old":{"trust":true},"/old/sub":{"trust":true},"/other":{"trust":true}}"#,
        );

        let report =
            rename_json_keys(&fs, Path::new("/home/.claude.json"), "/old", "/new", false).unwrap();

        assert_eq!(report.replacements, 2);

        let content = fs.read_to_string(Path::new("/home/.claude.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.get("/new").is_some());
        assert!(parsed.get("/new/sub").is_some());
        assert!(parsed.get("/other").is_some());
        assert!(parsed.get("/old").is_none());
        assert!(parsed.get("/old/sub").is_none());
    }

    #[test]
    fn update_does_not_touch_sibling_paths() {
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/data/history.jsonl"),
            r#"{"project":"/x/hyprland"}
{"project":"/x/hyprland_kde-apps"}
{"project":"/x/hyprland_config_plugin_neovim"}
{"project":"/x/hyprland-old"}
{"project":"/x/hyprland.bak"}
{"project":"/x/hyprland/sub"}"#,
        );

        let report = update_file(
            &fs,
            Path::new("/data/history.jsonl"),
            "/x/hyprland",
            "/y/hypr",
            false,
        )
        .unwrap();

        assert_eq!(report.replacements, 2, "only exact path and its descendant");

        let content = fs.read_to_string(Path::new("/data/history.jsonl")).unwrap();
        assert!(content.contains(r#"{"project":"/y/hypr"}"#));
        assert!(content.contains(r#"{"project":"/y/hypr/sub"}"#));
        assert!(content.contains("/x/hyprland_kde-apps"));
        assert!(content.contains("/x/hyprland_config_plugin_neovim"));
        assert!(content.contains("/x/hyprland-old"));
        assert!(content.contains("/x/hyprland.bak"));
    }

    #[test]
    fn update_skips_file_with_only_sibling_matches() {
        let fs = MockFs::new();
        let original = r#"{"project":"/x/hyprland_kde-apps"}"#;
        fs.add_file(Path::new("/data/history.jsonl"), original);

        let report = update_file(
            &fs,
            Path::new("/data/history.jsonl"),
            "/x/hyprland",
            "/y/hypr",
            false,
        )
        .unwrap();

        assert!(report.skipped);
        assert_eq!(report.replacements, 0);
        assert_eq!(
            fs.read_to_string(Path::new("/data/history.jsonl")).unwrap(),
            original
        );
    }

    #[test]
    fn rename_json_keys_ignores_sibling_keys() {
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/home/.claude.json"),
            r#"{"/x/hyprland":{"trust":true},"/x/hyprland/sub":{"trust":true},"/x/hyprland_kde-apps":{"trust":true},"/x/hyprland-old":{"trust":true}}"#,
        );

        let report = rename_json_keys(
            &fs,
            Path::new("/home/.claude.json"),
            "/x/hyprland",
            "/y/hypr",
            false,
        )
        .unwrap();

        assert_eq!(report.replacements, 2);

        let content = fs.read_to_string(Path::new("/home/.claude.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.get("/y/hypr").is_some());
        assert!(parsed.get("/y/hypr/sub").is_some());
        assert!(parsed.get("/x/hyprland").is_none());
        // Siblings survive under their original keys
        assert!(parsed.get("/x/hyprland_kde-apps").is_some());
        assert!(parsed.get("/x/hyprland-old").is_some());
    }

    #[test]
    fn rename_json_keys_no_match() {
        let fs = MockFs::new();
        let original = r#"{"/unrelated":{"trust":true}}"#;
        fs.add_file(Path::new("/home/.claude.json"), original);

        let report =
            rename_json_keys(&fs, Path::new("/home/.claude.json"), "/old", "/new", false).unwrap();

        assert_eq!(report.replacements, 0);
        assert!(report.skipped);
    }

    #[test]
    fn rename_json_keys_dry_run() {
        let fs = MockFs::new();
        let original = r#"{"/old":{"trust":true}}"#;
        fs.add_file(Path::new("/home/.claude.json"), original);

        let report =
            rename_json_keys(&fs, Path::new("/home/.claude.json"), "/old", "/new", true).unwrap();

        assert_eq!(report.replacements, 1);
        assert!(!report.skipped);

        // File unchanged
        let content = fs.read_to_string(Path::new("/home/.claude.json")).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn dry_run_and_real_run_report_same_counts() {
        let fs = MockFs::new();
        let content = r#"{"permissions":{"allow":["Bash(/home/user/proj/.claude/hook:*)"]},"hooks":{"cmd":"/home/user/proj/.claude/redirect.py"}}"#;

        // Dry run
        fs.add_file(Path::new("/settings.json"), content);
        let dry = update_file(
            &fs,
            Path::new("/settings.json"),
            "/home/user/proj",
            "/new",
            true,
        )
        .unwrap();

        // Real run (same file, same content)
        fs.add_file(Path::new("/settings.json"), content);
        let real = update_file(
            &fs,
            Path::new("/settings.json"),
            "/home/user/proj",
            "/new",
            false,
        )
        .unwrap();

        assert_eq!(
            dry.replacements, real.replacements,
            "dry and real must report same count"
        );
        assert_eq!(dry.skipped, real.skipped);
    }
}
