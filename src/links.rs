// Relink scan: finds Relink Candidates under the Scan Roots. Read-only —
// nothing in this module writes to disk.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::fs::Fs;
use crate::updater::{self, Substitutions};

/// Every project path registered under `"projects"` in `claude_json`. Falls
/// back to the file's top level for older/simpler files — the same two
/// shapes `updater::rename_json_keys` accepts.
///
/// A missing `claude_json` — `None`, or a path that does not exist — is a
/// legitimate empty state and yields an empty `Vec`, not an error. An
/// unreadable or malformed file is an error: silently scanning zero roots
/// would make a Relink report success while relinking nothing.
fn project_paths(fs: &dyn Fs, claude_json: Option<&Path>) -> Result<Vec<PathBuf>> {
    let Some(path) = claude_json else {
        return Ok(Vec::new());
    };
    if !fs.exists(path) {
        return Ok(Vec::new());
    }
    let content = fs
        .read_to_string(path)
        .with_context(|| format!("reading project list from {}", path.display()))?;
    let root: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("reading project list from {}", path.display()))?;
    let obj = root
        .get("projects")
        .and_then(|v| v.as_object())
        .or_else(|| root.as_object());
    Ok(obj
        .map(|obj| obj.keys().map(PathBuf::from).collect())
        .unwrap_or_default())
}

/// Makes a symlink's raw `target` absolute, resolved lexically against
/// `link_path`'s directory — never against the filesystem. `canonicalize`
/// requires an existing target, and after a Move the target the link
/// pointed at is exactly what may be gone.
fn resolve_link_target(link_path: &Path, target: &Path) -> PathBuf {
    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        let dir = link_path.parent().unwrap_or(link_path);
        dir.join(target)
    };
    lexically_normalize(&joined)
}

/// Collapses `.` and `..` components without touching the filesystem —
/// `Path::join` alone leaves them in place. A `..` that would climb above
/// the root is dropped: there is nowhere higher to go, so the path stays at
/// the root, the same way a shell's `cd ..` at `/` is a no-op.
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut stack: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match stack.last() {
                Some(Component::Normal(_)) => {
                    stack.pop();
                }
                Some(Component::RootDir) => {}
                _ => stack.push(component),
            },
            other => stack.push(other),
        }
    }
    stack.iter().collect()
}

/// The inverse of `resolve_link_target`: `target`, expressed as a relative
/// path from `link_path`'s directory. Written back into a Relink Candidate
/// that was relative, so it stays relative at its new location instead of
/// coming back absolute — see CONTEXT.md's *Relink* and the plan's
/// "Relative Links" note. Lexical, like `resolve_link_target`: no
/// filesystem access, since after a Move the old location may no longer
/// exist to canonicalize against.
pub fn relative_target(link_path: &Path, target: &Path) -> PathBuf {
    let link_dir = link_path.parent().unwrap_or(link_path);
    let link_components: Vec<_> = link_dir.components().collect();
    let target_components: Vec<_> = target.components().collect();

    let common = link_components
        .iter()
        .zip(target_components.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut result = PathBuf::new();
    for _ in common..link_components.len() {
        result.push("..");
    }
    for component in &target_components[common..] {
        result.push(component);
    }
    result
}

/// The Scan Roots for a Run: the Local Claude Directory of every Project
/// registered in `claude_json`, plus every explicit `--scan-root` path,
/// taken as-is rather than narrowed to a `.claude/` subdirectory. A root
/// that does not exist on disk is skipped silently — a registered project
/// can long since be deleted.
///
/// An explicit `--scan-root` can name a directory that is also, or nests
/// inside, one of the default `.claude/` roots — `--scan-root
/// ~/work/vendor/cef` on a path that is itself a registered project yields
/// both `<cef>/.claude` and `<cef>`, whose trees overlap. The invariant this
/// enforces: every returned root's tree is disjoint from every other's, so
/// `walk_scan_roots` never visits the same path twice. A root nested inside
/// another surviving root is dropped, keeping the outermost — `--scan-root`
/// stays additive per the plan's "additiv"; only the redundant overlap is
/// collapsed, not the set of trees actually covered.
pub fn scan_roots(
    fs: &dyn Fs,
    claude_json: Option<&Path>,
    explicit_roots: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut roots: Vec<PathBuf> = project_paths(fs, claude_json)?
        .into_iter()
        .map(|project| project.join(".claude"))
        .chain(explicit_roots.iter().cloned())
        .filter(|root| fs.exists(root))
        .collect();
    roots.sort();
    roots.dedup();

    let mut deduped = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let nested_in_another = roots
            .iter()
            .enumerate()
            .any(|(j, other)| j != i && root.starts_with(other));
        if !nested_in_another {
            deduped.push(root.clone());
        }
    }
    Ok(deduped)
}

/// A symlink the scan found whose resolved target falls under the
/// Substitution Set — see CONTEXT.md's *Relink Candidate*.
pub struct LinkCandidate {
    pub path: PathBuf,
    /// The link's resolved (always-absolute) target *before* the Move — see
    /// `resolve_link_target`. This is the Relink Log's `old` column
    /// (`RelinkLogEntry`, `src/migration.rs`), not the raw target string as
    /// read from disk: see `RelinkLogEntry`'s doc comment for why.
    pub resolved_target: PathBuf,
    /// `resolved_target` with its matching Substitution applied — where the
    /// link should point once relinked. Computed at scan time so Phase 3
    /// does not have to re-run the match.
    pub new_target: PathBuf,
    pub is_relative: bool,
    pub alive: bool,
}

/// Builds a `LinkCandidate` for the symlink at `link_path`, or `None` when
/// its resolved target matches no entry in `subs`. Matching goes through
/// `Substitutions::match_path` — a whole path, not a text fragment — the
/// same rule `replace_paths` applies to JSON keys, so an entry under both
/// `/x` and `/x/sub` resolves to the more specific one.
pub fn relink_candidate(
    fs: &dyn Fs,
    link_path: &Path,
    subs: &Substitutions,
) -> Option<LinkCandidate> {
    let raw_target = fs.read_link(link_path).ok()?;
    let is_relative = raw_target.is_relative();
    let resolved_target = resolve_link_target(link_path, &raw_target);

    let (target_old, _) = subs.match_path(&resolved_target)?;
    let new_target = subs.rebase(&resolved_target)?;

    // Self-referential Link exception: a relative link whose own path moves
    // under the very same entry as its target has both endpoints moving
    // together, so the correct relative target does not change. A link
    // whose own path moves under a *different* entry — or does not move at
    // all — is a candidate regardless.
    if is_relative
        && let Some((link_old, _)) = subs.match_path(link_path)
        && link_old == target_old
    {
        return None;
    }

    let alive = fs.exists(&resolved_target);

    Some(LinkCandidate {
        path: link_path.to_path_buf(),
        resolved_target,
        new_target,
        is_relative,
        alive,
    })
}

/// Every path found under the Scan Roots' trees, one `Fs::list_dir_recursive`
/// call per root run in parallel — 189 project trees per run are not free to
/// walk serially, the same reasoning behind `move_projects`'s `par_iter`.
pub fn walk_scan_roots(fs: &dyn Fs, roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .par_iter()
        .filter_map(|root| fs.list_dir_recursive(root).ok())
        .flatten()
        .collect()
}

/// A Text Reference the scan found — see CONTEXT.md's *Text Reference*: an
/// absolute path to a moving Project in a file's contents, not as a symlink
/// target. Read-only: nothing in this module rewrites `file`.
#[derive(Debug)]
pub struct TextReference {
    pub file: PathBuf,
    /// 1-based, so a report line matches what an editor would jump to.
    pub line: usize,
    pub old: String,
    pub new: String,
}

/// Crude but honest binary detection: a NUL byte anywhere in the content
/// marks the file binary. It misses binary formats that never happen to
/// contain a NUL, and it would call a text file "binary" if that file itself
/// embeds one — real source, config and doc files don't. Genuinely
/// non-UTF-8 content never reaches this check: `fs.read_to_string` already
/// rejects it in `text_references`.
fn looks_binary(content: &str) -> bool {
    content.contains('\0')
}

/// Every Text Reference to one of `subs`'s old paths found in `content`,
/// with 1-based line numbers. Matching goes through `updater::scan_matches`
/// — the same scan `replace_paths` runs — so this scan and a real rewrite
/// always agree on what counts as a match; only the line counting below is
/// specific to this module.
fn text_references_in_content(content: &str, subs: &Substitutions) -> Vec<(usize, String, String)> {
    let mut findings = Vec::new();
    let mut line = 1;
    let mut last = 0;
    for (offset, old, new) in updater::scan_matches(content, subs) {
        line += content[last..offset].matches('\n').count();
        findings.push((line, old.to_string(), new.to_string()));
        last = offset + old.len();
    }
    findings
}

/// Scans one file for Text References — see CONTEXT.md's *Text Reference*.
/// Read-only: nothing here writes to `path`.
///
/// Skips a `.git/` path, a file inside a moving Project's own source tree
/// (its `settings.json` and `.mcp.json` are rewritten by
/// `update_local_config_for` — reporting a Text Reference there would flag
/// the run's own write as a finding), and
/// anything that fails to read as UTF-8 or that `looks_binary` flags.
/// Unreadable and binary files yield an empty `Vec`, not an error: a
/// read-only pass over hundreds of foreign trees must not abort on one odd
/// file.
///
/// Called per path from `Migration::scan_relink_candidates`
/// (`src/migration.rs`), over the same `walk_scan_roots` output
/// `relink_candidate` scans — a Relink and a Text Reference finding always
/// agree on where they looked.
pub fn text_references(fs: &dyn Fs, path: &Path, subs: &Substitutions) -> Vec<TextReference> {
    if path.components().any(|c| c.as_os_str() == ".git") {
        return Vec::new();
    }
    if subs.match_path(path).is_some() {
        return Vec::new();
    }
    let Ok(content) = fs.read_to_string(path) else {
        return Vec::new();
    };
    if looks_binary(&content) {
        return Vec::new();
    }
    text_references_in_content(&content, subs)
        .into_iter()
        .map(|(line, old, new)| TextReference {
            file: path.to_path_buf(),
            line,
            old,
            new,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MockFs;

    #[test]
    fn resolve_absolute_target_unchanged() {
        let resolved = resolve_link_target(
            Path::new("/home/user/a/.claude/link"),
            Path::new("/home/user/b/target"),
        );

        assert_eq!(resolved, PathBuf::from("/home/user/b/target"));
    }

    #[test]
    fn resolve_relative_target_against_link_dir() {
        let resolved =
            resolve_link_target(Path::new("/home/user/a/.claude/link"), Path::new("target"));

        assert_eq!(resolved, PathBuf::from("/home/user/a/.claude/target"));
    }

    #[test]
    fn resolve_handles_parent_segments() {
        let resolved = resolve_link_target(
            Path::new("/home/user/a/.claude/bin/link"),
            Path::new("../../x/y"),
        );

        assert_eq!(resolved, PathBuf::from("/home/user/a/x/y"));
    }

    /// `lexically_normalize` claims a `..` that would climb past the root is
    /// a no-op, the way a shell's `cd ..` at `/` is. Nothing checked that: a
    /// link with more `..` than it has depth is exactly what a hand-written
    /// relative target looks like when someone miscounts.
    #[test]
    fn resolve_stops_at_the_root_instead_of_climbing_past_it() {
        let resolved = resolve_link_target(Path::new("/a/link"), Path::new("../../../x"));

        assert_eq!(resolved, PathBuf::from("/x"));
    }

    #[test]
    fn resolve_works_for_dangling_target() {
        let resolved = resolve_link_target(
            Path::new("/home/user/a/.claude/link"),
            Path::new("../missing/gone"),
        );

        assert_eq!(resolved, PathBuf::from("/home/user/a/missing/gone"));
    }

    #[test]
    fn scan_roots_from_claude_json() {
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/home/.claude.json"),
            r#"{"projects":{"/home/user/a":{},"/home/user/b":{},"/home/user/c":{}}}"#,
        );
        fs.add_dir(Path::new("/home/user/a/.claude"));
        fs.add_dir(Path::new("/home/user/b/.claude"));
        // /home/user/c/.claude never existed — the project is long gone.

        let mut roots = scan_roots(&fs, Some(Path::new("/home/.claude.json")), &[]).unwrap();
        roots.sort();

        assert_eq!(
            roots,
            vec![
                PathBuf::from("/home/user/a/.claude"),
                PathBuf::from("/home/user/b/.claude"),
            ]
        );
    }

    #[test]
    fn scan_roots_include_explicit_roots() {
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/home/.claude.json"),
            r#"{"projects":{"/home/user/a":{}}}"#,
        );
        fs.add_dir(Path::new("/home/user/a/.claude"));
        fs.add_dir(Path::new("/work/vendor/cef"));

        let mut roots = scan_roots(
            &fs,
            Some(Path::new("/home/.claude.json")),
            &[PathBuf::from("/work/vendor/cef")],
        )
        .unwrap();
        roots.sort();

        assert_eq!(
            roots,
            vec![
                PathBuf::from("/home/user/a/.claude"),
                PathBuf::from("/work/vendor/cef"),
            ]
        );
    }

    /// A malformed `~/.claude.json` must not silently scan zero roots —
    /// that would make a Relink report success while relinking nothing.
    #[test]
    fn scan_roots_errors_on_malformed_claude_json() {
        let fs = MockFs::new();
        fs.add_file(Path::new("/home/.claude.json"), "not json");

        let err = scan_roots(&fs, Some(Path::new("/home/.claude.json")), &[]).unwrap_err();

        assert!(err.to_string().contains("/home/.claude.json"));
    }

    /// Unlike a malformed file, a missing `~/.claude.json` is a legitimate
    /// empty state — a project can long since be deleted.
    #[test]
    fn scan_roots_treats_missing_claude_json_as_empty() {
        let fs = MockFs::new();

        let roots = scan_roots(&fs, Some(Path::new("/home/.claude.json")), &[]).unwrap();

        assert!(roots.is_empty());
    }

    #[test]
    fn explicit_scan_root_is_walked_in_full() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/work/vendor/cef/global"));
        fs.add_file(Path::new("/work/vendor/cef/global/settings.json"), "{}");

        let roots = scan_roots(&fs, None, &[PathBuf::from("/work/vendor/cef")]).unwrap();
        let found = walk_scan_roots(&fs, &roots);

        assert_eq!(
            found,
            vec![PathBuf::from("/work/vendor/cef/global/settings.json")]
        );
    }

    /// `--scan-root ~/work/vendor/cef` on a directory that is *also* a
    /// registered project must not yield both `<cef>/.claude` (from
    /// `claude.json`) and `<cef>` (explicit) — their trees overlap, so
    /// walking both would visit every symlink under `<cef>/.claude` twice
    /// and log it twice.
    #[test]
    fn scan_roots_does_not_walk_overlapping_roots_twice() {
        let fs = MockFs::new();
        fs.add_file(
            Path::new("/home/.claude.json"),
            r#"{"projects":{"/work/vendor/cef":{}}}"#,
        );
        fs.add_dir(Path::new("/work/vendor/cef/.claude"));
        fs.add_file(Path::new("/work/vendor/cef/.claude/link"), "content");

        let roots = scan_roots(
            &fs,
            Some(Path::new("/home/.claude.json")),
            &[PathBuf::from("/work/vendor/cef")],
        )
        .unwrap();

        assert_eq!(
            roots,
            vec![PathBuf::from("/work/vendor/cef")],
            "the nested /work/vendor/cef/.claude must be dropped, not kept alongside it"
        );

        let found = walk_scan_roots(&fs, &roots);
        assert_eq!(
            found,
            vec![PathBuf::from("/work/vendor/cef/.claude/link")],
            "one root, one visit — a duplicate root would report this path twice"
        );
    }

    #[test]
    fn candidate_matches_longest_prefix() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![
            ("/x".to_string(), "/x-new".to_string()),
            ("/x/sub".to_string(), "/x/sub-new".to_string()),
        ]);
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/x/sub/deep/target.txt"),
        );

        let candidate = relink_candidate(&fs, Path::new("/outside/link"), &subs).unwrap();

        // `/x/sub` is the longer, more specific entry; if `/x` had won
        // instead, the new target would read `/x-new/sub/deep/target.txt`.
        assert_eq!(
            candidate.new_target,
            PathBuf::from("/x/sub-new/deep/target.txt")
        );
    }

    #[test]
    fn candidate_skips_link_outside_all_sources() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![("/x".to_string(), "/x-new".to_string())]);
        fs.add_symlink(Path::new("/outside/link"), Path::new("/y/other/target.txt"));

        let candidate = relink_candidate(&fs, Path::new("/outside/link"), &subs);

        assert!(candidate.is_none());
    }

    #[test]
    fn candidate_records_dead_link_as_not_alive() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![("/x".to_string(), "/x-new".to_string())]);
        // /x/gone.txt was never registered as a file or directory — dead
        // before the Move.
        fs.add_symlink(Path::new("/outside/link"), Path::new("/x/gone.txt"));

        let candidate = relink_candidate(&fs, Path::new("/outside/link"), &subs).unwrap();

        assert!(!candidate.alive);
    }

    #[test]
    fn candidate_does_not_cascade() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![
            ("/a".to_string(), "/b".to_string()),
            ("/b".to_string(), "/c".to_string()),
        ]);
        fs.add_symlink(Path::new("/outside/link"), Path::new("/a/file.txt"));

        let candidate = relink_candidate(&fs, Path::new("/outside/link"), &subs).unwrap();

        // A single pass applies only the `a -> b` entry; if the result were
        // rescanned against `b -> c`, this would read `/c/file.txt` instead.
        assert_eq!(candidate.new_target, PathBuf::from("/b/file.txt"));
    }

    #[test]
    fn absolute_self_link_is_a_candidate() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![("/x".to_string(), "/x-new".to_string())]);
        fs.add_dir(Path::new("/x/.claude/lib"));
        fs.add_file(Path::new("/x/.claude/lib/real-tool"), "content");
        fs.add_symlink(
            Path::new("/x/.claude/bin/tool"),
            Path::new("/x/.claude/lib/real-tool"),
        );

        let candidate = relink_candidate(&fs, Path::new("/x/.claude/bin/tool"), &subs).unwrap();

        assert!(!candidate.is_relative);
        assert!(candidate.alive);
        assert_eq!(
            candidate.resolved_target,
            PathBuf::from("/x/.claude/lib/real-tool")
        );
    }

    #[test]
    fn textscan_finds_absolute_path_in_file() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![("/x/proj".to_string(), "/y/proj".to_string())]);
        fs.add_file(
            Path::new("/outside/config.toml"),
            "line one\npath = \"/x/proj/data\"\nline three",
        );

        let findings = text_references(&fs, Path::new("/outside/config.toml"), &subs);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, 2);
        assert_eq!(findings[0].old, "/x/proj");
        assert_eq!(findings[0].new, "/y/proj");
    }

    /// One reference per file leaves the cursor that carries line counting
    /// from one match to the next completely unexercised — and a config file
    /// mentioning the same project twice is the ordinary case, not the odd
    /// one. The second line number is the assertion that matters here.
    #[test]
    fn textscan_numbers_every_reference_in_a_file() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![("/x/proj".to_string(), "/y/proj".to_string())]);
        fs.add_file(
            Path::new("/outside/config.toml"),
            "a = \"/x/proj/one\"\nfiller\nfiller\nb = \"/x/proj/two\"",
        );

        let findings = text_references(&fs, Path::new("/outside/config.toml"), &subs);

        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].line, 1);
        assert_eq!(findings[1].line, 4);
    }

    #[test]
    fn textscan_skips_binary_and_git() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![("/x/proj".to_string(), "/y/proj".to_string())]);
        // A NUL byte in the first chunk is this scan's whole binary test —
        // crude, but it will never mistake a real config or source file for
        // binary content.
        fs.add_file(Path::new("/outside/binary.dat"), "prefix\0/x/proj/data");
        fs.add_file(Path::new("/outside/.git/config"), "/x/proj/data");

        assert!(text_references(&fs, Path::new("/outside/binary.dat"), &subs).is_empty());
        assert!(text_references(&fs, Path::new("/outside/.git/config"), &subs).is_empty());
    }

    #[test]
    fn textscan_skips_moving_source_trees() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![("/x/proj".to_string(), "/y/proj".to_string())]);
        // ccmv rewrites this file itself (update_local_config_for); a Text
        // Reference finding here would just report the run's own write.
        fs.add_file(
            Path::new("/x/proj/.claude/settings.json"),
            r#"{"cwd":"/x/proj"}"#,
        );

        assert!(text_references(&fs, Path::new("/x/proj/.claude/settings.json"), &subs).is_empty());
    }

    #[test]
    fn textscan_leaves_files_untouched() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![("/x/proj".to_string(), "/y/proj".to_string())]);
        fs.add_file(Path::new("/outside/config.toml"), "path = \"/x/proj/data\"");

        let findings = text_references(&fs, Path::new("/outside/config.toml"), &subs);

        assert_eq!(findings.len(), 1, "sanity: the scan did read this file");
        assert_eq!(fs.write_count(Path::new("/outside/config.toml")), 0);
    }

    #[test]
    fn relative_self_link_is_not_a_candidate() {
        let fs = MockFs::new();
        let subs = Substitutions::new(vec![("/x".to_string(), "/x-new".to_string())]);
        fs.add_dir(Path::new("/x/.claude/lib"));
        fs.add_file(Path::new("/x/.claude/lib/real-tool"), "content");
        fs.add_symlink(
            Path::new("/x/.claude/bin/tool"),
            Path::new("../lib/real-tool"),
        );

        let candidate = relink_candidate(&fs, Path::new("/x/.claude/bin/tool"), &subs);

        assert!(candidate.is_none());
    }
}
