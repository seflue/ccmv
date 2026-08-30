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

/// A set of `old path -> new path` substitutions applied as one unit.
///
/// Entries are held longest-`old`-first so that nested paths resolve to the
/// most specific entry: with both `/x` and `/x/sub` in the set, `/x/sub`
/// must not be consumed by `/x`.
pub struct Substitutions {
    /// Sorted by descending `old` length — `match_at` and `match_prefix`
    /// depend on this to return the longest match first.
    entries: Vec<(String, String)>,
    /// Byte values that can begin a match. `replace_paths` probes every
    /// position in a multi-megabyte file, and paths all start with `/`, so
    /// this rejects the vast majority of them without scanning `entries`.
    first_bytes: [bool; 256],
}

impl Substitutions {
    pub fn new(mut entries: Vec<(String, String)>) -> Self {
        entries.sort_by_key(|(old, _)| std::cmp::Reverse(old.len()));
        let mut first_bytes = [false; 256];
        for byte in entries.iter().filter_map(|(old, _)| old.as_bytes().first()) {
            first_bytes[*byte as usize] = true;
        }
        Self {
            entries,
            first_bytes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Longest entry matching at the start of `rest` on a segment boundary.
    ///
    /// `pub(crate)` so `links::text_references` can scan file contents for
    /// Text References with the same text-fragment matcher `replace_paths`
    /// uses, instead of a second implementation of the same rule.
    pub(crate) fn match_at(&self, rest: &str) -> Option<(&str, &str)> {
        let first = *rest.as_bytes().first()?;
        if !self.first_bytes[first as usize] {
            return None;
        }
        self.entries
            .iter()
            .find(|(old, _)| {
                rest.starts_with(old.as_str()) && is_segment_boundary(&rest[old.len()..])
            })
            .map(|(old, new)| (old.as_str(), new.as_str()))
    }

    /// Longest entry whose `old` is `path` itself or a path-prefix of it.
    ///
    /// Stricter than `match_at` on purpose: a whole path — a JSON key, or a
    /// resolved symlink target as `links::relink_candidate` uses it — is one
    /// unit, so only an exact match or a `/`-descendant counts. `match_at`
    /// also treats a space as a boundary — needed to rewrite hook command
    /// strings like `cd /old/proj && ...`, but it would wrongly claim the
    /// sibling key `/old/proj stuff`.
    pub(crate) fn match_prefix(&self, path: &str) -> Option<(&str, &str)> {
        self.entries
            .iter()
            .find(|(old, _)| {
                path == old
                    || (path.len() > old.len()
                        && path.as_bytes()[old.len()] == b'/'
                        && path.starts_with(old.as_str()))
            })
            .map(|(old, new)| (old.as_str(), new.as_str()))
    }

    /// `match_prefix` over a filesystem `Path`, wrapping the lossy
    /// `Path`-to-`str` conversion once so callers don't degrade a `PathBuf`
    /// via `to_string_lossy()` themselves at every call site.
    pub(crate) fn match_path(&self, path: &Path) -> Option<(&str, &str)> {
        self.match_prefix(&path.to_string_lossy())
    }

    /// `match_path`, applied: `path`'s matching prefix swapped for its
    /// replacement, `None` when nothing matches. The rebase itself —
    /// `format!("{new}{}", &path[old.len()..])` — used to be written out by
    /// hand at every call site (`links::relink_candidate`,
    /// `Migration::repoint_candidates`, `rename_json_keys`); one copy here is
    /// what all three now share.
    pub(crate) fn rebase(&self, path: &Path) -> Option<PathBuf> {
        let (old, new) = self.match_path(path)?;
        let path_str = path.to_string_lossy();
        Some(PathBuf::from(format!("{new}{}", &path_str[old.len()..])))
    }
}

/// Scans `content` left to right for every Substitution match, yielding one
/// `(byte offset, old, new)` per hit, offsets increasing. Both `replace_paths`
/// (which rebuilds `content` around each hit) and
/// `links::text_references_in_content` (which turns offsets into line
/// numbers) walk this same sequence of matches, instead of each carrying its
/// own copy of the match-and-advance loop — a doc comment claiming the two
/// "always agree" is not the same as them sharing the code that decides it.
pub(crate) fn scan_matches<'a>(
    content: &'a str,
    subs: &'a Substitutions,
) -> impl Iterator<Item = (usize, &'a str, &'a str)> {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < content.len() {
            let rest = &content[i..];
            if let Some((old, new)) = subs.match_at(rest) {
                let offset = i;
                i += old.len();
                return Some((offset, old, new));
            }
            let c = rest
                .chars()
                .next()
                .expect("loop guard keeps rest non-empty");
            i += c.len_utf8();
        }
        None
    })
}

/// Applies every substitution in `subs` in a single left-to-right pass,
/// matching only on path-segment boundaries. Returns the replacement count
/// and the new content.
///
/// The output is never rescanned, so a set containing both `a -> b` and
/// `b -> c` turns an `a` into a `b` and stops there rather than cascading.
fn replace_paths(content: &str, subs: &Substitutions) -> (usize, String) {
    let mut out = String::with_capacity(content.len());
    let mut count = 0;
    let mut last = 0;

    for (offset, old, new) in scan_matches(content, subs) {
        out.push_str(&content[last..offset]);
        out.push_str(new);
        count += 1;
        last = offset + old.len();
    }
    out.push_str(&content[last..]);

    (count, out)
}

/// Updates path references in a single file.
///
/// Reads the file, applies every substitution in `subs` in one pass, and
/// writes back atomically only if changes were made. Matches that merely
/// share a prefix with a sibling path (`/x/proj` vs `/x/proj_other`) are left
/// alone.
/// When `dry_run` is `true`, counts replacements without writing.
pub fn update_file(
    fs: &dyn Fs,
    file: &Path,
    subs: &Substitutions,
    dry_run: bool,
) -> Result<UpdateReport> {
    if subs.is_empty() || !fs.exists(file) {
        return Ok(UpdateReport {
            file: file.to_path_buf(),
            replacements: 0,
            skipped: true,
        });
    }

    let content = fs.read_to_string(file)?;
    let (count, new_content) = replace_paths(&content, subs);

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
    subs: &Substitutions,
    dry_run: bool,
) -> Result<Vec<UpdateReport>> {
    files
        .par_iter()
        .map(|f| update_file(fs, f, subs, dry_run))
        .collect()
}

/// Renames JSON object keys that match a substitution's `old` path or live
/// underneath it. Sibling keys sharing only a textual prefix are untouched.
///
/// Used for `~/.claude.json` where project paths are top-level keys.
/// Only keys are modified — values remain untouched.
pub fn rename_json_keys(
    fs: &dyn Fs,
    path: &Path,
    subs: &Substitutions,
    dry_run: bool,
) -> Result<UpdateReport> {
    if subs.is_empty() || !fs.exists(path) {
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

    // Longest matching prefix wins, so a key under both `/x` and `/x/sub`
    // follows the more specific entry.
    let renames: Vec<(String, String)> = obj
        .keys()
        .filter_map(|k| {
            let new_key = subs.rebase(Path::new(k))?;
            Some((k.clone(), new_key.to_string_lossy().into_owned()))
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

    fn subs(pairs: &[(&str, &str)]) -> Substitutions {
        Substitutions::new(
            pairs
                .iter()
                .map(|(o, n)| ((*o).to_owned(), (*n).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn replace_paths_applies_multiple_substitutions() {
        let content = r#"{"a":"/x/one","b":"/x/two","c":"/x/three"}"#;
        let (count, out) = replace_paths(content, &subs(&[("/x/one", "/y/1"), ("/x/two", "/y/2")]));

        assert_eq!(count, 2);
        assert!(out.contains("/y/1"));
        assert!(out.contains("/y/2"));
        assert!(out.contains("/x/three"), "untouched path must survive");
    }

    /// A single left-to-right pass must not rescan its own output: with
    /// `a→b` and `b→c` in one set, an `a` becomes `b` and stops there.
    /// Without this, chained substitutions would silently cascade.
    #[test]
    fn replace_paths_does_not_cascade() {
        let (count, out) = replace_paths(
            r#"{"p":"/x/a"}"#,
            &subs(&[("/x/a", "/x/b"), ("/x/b", "/x/c")]),
        );

        assert_eq!(count, 1);
        assert!(out.contains("/x/b"));
        assert!(!out.contains("/x/c"), "output must not be rescanned");
    }

    /// Nested sources: the longer prefix must win, otherwise `/x` would
    /// consume `/x/sub` before its own entry is ever considered.
    #[test]
    fn replace_paths_longest_match_wins() {
        let (count, out) = replace_paths(
            r#"{"p":"/x/sub","q":"/x"}"#,
            &subs(&[("/x", "/short"), ("/x/sub", "/long")]),
        );

        assert_eq!(count, 2);
        assert!(out.contains("/long"));
        assert!(out.contains("/short"));
        assert!(!out.contains("/short/sub"));
    }

    /// `match_at` and `match_prefix` must NOT be merged: text content treats
    /// a space as a segment boundary (so `cd /old/proj && ...` in a hook
    /// command gets rewritten), but a JSON key *is* one whole path, so the
    /// same rule would rewrite the unrelated sibling key `/old proj`.
    #[test]
    fn json_keys_are_stricter_about_boundaries_than_text() {
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/home/.claude.json"),
            r#"{"/x/my":{"trust":true},"/x/my stuff":{"trust":true}}"#,
        );

        let report = rename_json_keys(
            &fs,
            Path::new("/home/.claude.json"),
            &subs(&[("/x/my", "/y/mine")]),
            false,
        )
        .unwrap();

        assert_eq!(report.replacements, 1, "only the exact key");
        let content = fs.read_to_string(Path::new("/home/.claude.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.get("/y/mine").is_some());
        assert!(
            parsed.get("/x/my stuff").is_some(),
            "sibling key with a space must survive"
        );
    }

    #[test]
    fn replace_paths_empty_set_is_noop() {
        let content = r#"{"p":"/x/a"}"#;
        let (count, out) = replace_paths(content, &subs(&[]));

        assert_eq!(count, 0);
        assert_eq!(out, content);
    }

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
            &subs(&[("/old/path", "/new/path")]),
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
            &subs(&[("/old/path", "/new/path")]),
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
            &subs(&[("/old/path", "/new/path")]),
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
            &subs(&[("/home/user/old-project", "/home/user/new-project")]),
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

        let reports =
            update_files_parallel(&fs, &files, &subs(&[("/old", "/new")]), false).unwrap();

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
            &subs(&[("/home/user/old", "/home/user/new")]),
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
            &subs(&[("/old/path", "/new/path")]),
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
            &subs(&[("/home/user/old-project", "/home/user/new-project")]),
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
            &subs(&[("/home/user/old-project", "/home/user/new-project")]),
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

        let report = rename_json_keys(
            &fs,
            Path::new("/home/.claude.json"),
            &subs(&[("/old", "/new")]),
            false,
        )
        .unwrap();

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
            &subs(&[("/x/hyprland", "/y/hypr")]),
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
            &subs(&[("/x/hyprland", "/y/hypr")]),
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
            &subs(&[("/x/hyprland", "/y/hypr")]),
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

        let report = rename_json_keys(
            &fs,
            Path::new("/home/.claude.json"),
            &subs(&[("/old", "/new")]),
            false,
        )
        .unwrap();

        assert_eq!(report.replacements, 0);
        assert!(report.skipped);
    }

    #[test]
    fn rename_json_keys_dry_run() {
        let fs = MockFs::new();
        let original = r#"{"/old":{"trust":true}}"#;
        fs.add_file(Path::new("/home/.claude.json"), original);

        let report = rename_json_keys(
            &fs,
            Path::new("/home/.claude.json"),
            &subs(&[("/old", "/new")]),
            true,
        )
        .unwrap();

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
            &subs(&[("/home/user/proj", "/new")]),
            true,
        )
        .unwrap();

        // Real run (same file, same content)
        fs.add_file(Path::new("/settings.json"), content);
        let real = update_file(
            &fs,
            Path::new("/settings.json"),
            &subs(&[("/home/user/proj", "/new")]),
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
