// Migration: core orchestration with idempotent migration logic

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::backup;
use crate::encoder;
use crate::fs::Fs;
use crate::scanner;
use crate::updater::{self, Substitutions, UpdateReport};

/// Where a project's global session directory moves, as `(old, new)`.
type GlobalRename = (PathBuf, PathBuf);

#[allow(clippy::struct_excessive_bools)]
pub struct MigrateOpts {
    pub dry_run: bool,
    #[allow(dead_code)]
    pub verbose: bool,
    pub force: bool,
    pub no_backup: bool,
    pub session_only: bool,
}

#[derive(Debug)]
pub struct MigrationReport {
    pub action: String,
    pub source: Option<PathBuf>,
    pub target: Option<PathBuf>,
    pub global_dir_rename: Option<(PathBuf, PathBuf)>,
    pub files_updated: Vec<UpdateReport>,
    pub backup_path: Option<PathBuf>,
    pub dry_run: bool,
    pub nothing_to_do: bool,
}

pub enum Command {
    Move {
        source: PathBuf,
        target: PathBuf,
        opts: MigrateOpts,
    },
    Backup {
        path: PathBuf,
    },
    Restore {
        backup_file: PathBuf,
    },
}

impl Command {
    /// Build a `Command` from parsed CLI arguments, resolving paths.
    pub fn from_cli(cli: &crate::cli::Cli) -> Result<Self> {
        if let Some(ref subcmd) = cli.command {
            return match subcmd {
                crate::cli::Commands::Backup { path } => {
                    let path = std::fs::canonicalize(path)?;
                    Ok(Self::Backup { path })
                }
                crate::cli::Commands::Restore { backup_file } => {
                    let backup_file = std::fs::canonicalize(backup_file)?;
                    Ok(Self::Restore { backup_file })
                }
            };
        }

        let Some(ref source_arg) = cli.source else {
            bail!("missing <SOURCE> and <TARGET> arguments\n\nUsage: ccmv <SOURCE> <TARGET>");
        };
        let Some(ref target_arg) = cli.target else {
            bail!("missing <TARGET> argument\n\nUsage: ccmv <SOURCE> <TARGET>");
        };

        let source = std::fs::canonicalize(source_arg)
            .or_else(|_| std::path::absolute(source_arg).map(|p| normalize_path(&p)))?;
        let mut target = normalize_path(&std::path::absolute(target_arg)?);
        // mv-semantics: if target is existing dir (and not source itself),
        // move source INTO it, just like `mv` does.
        // For idempotent re-runs (source gone): only apply mv-semantics
        // if target/name exists (i.e. a previous move used mv-semantics).
        // Skipped for --session-only: target is the literal new project path.
        if !cli.session_only
            && target.is_dir()
            && target != source
            && let Some(name) = source.file_name()
            && (source.is_dir() || target.join(name).exists())
        {
            target = target.join(name);
        }
        Ok(Self::Move {
            source,
            target,
            opts: MigrateOpts {
                dry_run: cli.dry_run,
                verbose: cli.verbose,
                force: cli.force,
                no_backup: cli.no_backup,
                session_only: cli.session_only,
            },
        })
    }

    /// Execute this command, producing a `MigrationReport`.
    pub fn execute(self, fs: &dyn Fs, claude_home: &Path) -> Result<MigrationReport> {
        match self {
            Self::Move {
                source,
                target,
                opts,
            } => Migration::new(fs, &source, &target, claude_home, &opts)?.run(),
            Self::Backup { path } => execute_backup(fs, &path, claude_home),
            Self::Restore { backup_file } => execute_restore(&backup_file),
        }
    }
}

struct Migration<'a> {
    fs: &'a dyn Fs,
    source: &'a Path,
    target: &'a Path,
    source_state: scanner::ProjectState,
    claude_home: &'a Path,
    dry_run: bool,
    no_backup: bool,
    session_only: bool,
    old_str: String,
    new_str: String,
    /// Applied to every file this migration rewrites, in one pass each.
    subs: Substitutions,
    /// Projects nested under `source`, as `(old, new)` pairs. Empty until
    /// `run` discovers them; session-only moves never populate it.
    subprojects: Vec<(PathBuf, PathBuf)>,
}

impl<'a> Migration<'a> {
    fn new(
        fs: &'a dyn Fs,
        source: &'a Path,
        target: &'a Path,
        claude_home: &'a Path,
        opts: &MigrateOpts,
    ) -> Result<Self> {
        // Validate: both paths must be absolute
        if !source.is_absolute() {
            bail!("source path must be absolute, got: {}", source.display());
        }
        if !target.is_absolute() {
            bail!("target path must be absolute, got: {}", target.display());
        }

        let old_str = source.to_string_lossy().to_string();
        let new_str = target.to_string_lossy().to_string();

        // Scan source state
        let source_state = scanner::scan(fs, source, claude_home)?;

        // Validate: source must exist as directory OR have global Claude data.
        // If neither exists, check if the migration was already completed
        // (target has the project) — support idempotent re-runs.
        if !source_state.has_claude_data() {
            // Session-only mode: the idempotency fallback (reuse target state
            // as source state) is unsafe here. With nothing to move on the
            // source side, falling through would silently no-op AND, if backup
            // is enabled, archive target's local `.claude/` — including any
            // broken symlinks that live there. Bail clearly instead.
            if source == target || opts.session_only {
                bail!(
                    "source not found: {} does not exist and has no Claude Code project data",
                    source.display()
                );
            }
            let target_state = scanner::scan(fs, target, claude_home)?;
            if target_state.has_claude_data() {
                // Migration already completed — use target state to check
                // if any remaining work is needed (e.g. .claude.json keys)
                return Ok(Self {
                    fs,
                    source,
                    target,
                    source_state: target_state,
                    claude_home,
                    dry_run: opts.dry_run,
                    no_backup: opts.no_backup,
                    session_only: opts.session_only,
                    subs: Substitutions::one(&old_str, &new_str),
                    subprojects: Vec::new(),
                    old_str,
                    new_str,
                });
            }
            bail!(
                "source not found: {} does not exist and has no Claude Code project data",
                source.display()
            );
        }

        // Conflict check — skipped in session-only mode (target global may
        // already exist; rename_global_dir will merge into it).
        if source != target && !opts.session_only {
            let target_state = scanner::scan(fs, target, claude_home)?;
            if target_state.global_project_dir.is_some() && !opts.force {
                bail!(
                    "conflict: target {} already has Claude Code project data; use --force to overwrite",
                    target.display()
                );
            }
        }

        Ok(Self {
            fs,
            source,
            target,
            source_state,
            claude_home,
            dry_run: opts.dry_run,
            no_backup: opts.no_backup,
            session_only: opts.session_only,
            subs: Substitutions::one(&old_str, &new_str),
            subprojects: Vec::new(),
            old_str,
            new_str,
        })
    }

    fn run(mut self) -> Result<MigrationReport> {
        if self.session_only {
            return self.run_session_only();
        }
        // Idempotency check
        if self.source_state.paths_consistent
            && self.source_state.project_dir_exists
            && self.source == self.target
        {
            return Ok(MigrationReport {
                action: "move".to_owned(),
                source: Some(self.source.to_path_buf()),
                target: Some(self.target.to_path_buf()),
                global_dir_rename: None,
                files_updated: Vec::new(),
                backup_path: None,
                dry_run: self.dry_run,
                nothing_to_do: true,
            });
        }

        let backup_path = self.create_backup_if_needed()?;
        // Discover before anything is rewritten — the keys still name the
        // old paths at this point.
        self.subprojects = self.discover_subprojects()?;

        let (global_dir_rename, mut all_reports) =
            self.migrate_project(&self.source_state, self.target)?;
        all_reports.extend(self.migrate_subprojects()?);

        // Shared files, once each: a path-prefix match covers the parent and
        // every sub-project underneath it in the same pass.
        all_reports.extend(self.update_history());
        all_reports.extend(self.update_claude_json());

        self.move_project_dir()?;
        self.verify()?;

        Ok(MigrationReport {
            action: "move".to_owned(),
            source: Some(self.source.to_path_buf()),
            target: Some(self.target.to_path_buf()),
            global_dir_rename,
            files_updated: all_reports,
            backup_path,
            dry_run: self.dry_run,
            nothing_to_do: false,
        })
    }

    /// Session-only migration: moves global Claude Code session data
    /// from source to target, leaving both project directories untouched.
    fn run_session_only(self) -> Result<MigrationReport> {
        // Pre-check 1: source must have global session data
        if self.source_state.global_project_dir.is_none() {
            bail!(
                "nothing to move: no global session data for {}",
                self.source.display()
            );
        }
        // Pre-check 2: target must exist as directory (otherwise sessions would
        // be orphaned). Only enforced when source != target.
        if self.source != self.target && !self.fs.is_dir(self.target) {
            bail!(
                "target path does not exist; sessions would be orphaned: {}",
                self.target.display()
            );
        }
        // Noop: source == target
        if self.source == self.target {
            return Ok(MigrationReport {
                action: "session-only-noop".to_owned(),
                source: Some(self.source.to_path_buf()),
                target: Some(self.target.to_path_buf()),
                global_dir_rename: None,
                files_updated: Vec::new(),
                backup_path: None,
                dry_run: self.dry_run,
                nothing_to_do: true,
            });
        }

        let backup_path = self.create_backup_if_needed()?;
        let global_dir_rename = self.compute_global_rename()?;
        self.rename_global_dir(global_dir_rename.as_ref())?;

        let mut all_reports = Vec::new();
        all_reports.extend(self.update_session_files(global_dir_rename.as_ref())?);
        all_reports.extend(self.update_history());
        all_reports.extend(self.update_claude_json());

        // verify() is intentionally skipped — source project dir may not exist
        // and target project dir is untouched by design.
        if !self.dry_run {
            eprintln!("session-only: skipping full verify; source project dir untouched");
        }

        Ok(MigrationReport {
            action: "session-only".to_owned(),
            source: Some(self.source.to_path_buf()),
            target: Some(self.target.to_path_buf()),
            global_dir_rename,
            files_updated: all_reports,
            backup_path,
            dry_run: self.dry_run,
            nothing_to_do: false,
        })
    }

    fn compute_global_rename(&self) -> Result<Option<(PathBuf, PathBuf)>> {
        self.global_rename_for(&self.source_state, self.target)
    }

    /// Where `state`'s global directory has to move for a project landing at
    /// `target`. `None` when it is already in the right place.
    fn global_rename_for(
        &self,
        state: &scanner::ProjectState,
        target: &Path,
    ) -> Result<Option<(PathBuf, PathBuf)>> {
        let Some(ref old_global) = state.global_project_dir else {
            return Ok(None);
        };
        let new_encoded = encoder::encode(target)?;
        let new_global = self.claude_home.join("projects").join(&new_encoded);
        if *old_global == new_global {
            Ok(None)
        } else {
            Ok(Some((old_global.clone(), new_global)))
        }
    }

    fn rename_global_dir(&self, global_dir_rename: Option<&(PathBuf, PathBuf)>) -> Result<()> {
        if !self.dry_run
            && let Some((old_global, new_global)) = global_dir_rename
        {
            if self.fs.is_dir(new_global) {
                // Target global dir already exists — merge source into it.
                // `sessions-index.json` is JSON-merged first so target's
                // session entries aren't clobbered by source's index;
                // remaining files (UUID-named) are moved by `merge_dirs`.
                merge_sessions_index(old_global, new_global)?;
                merge_dirs(old_global, new_global)?;
            } else {
                self.fs
                    .rename(old_global, new_global)
                    .context("renaming global project directory")?;
            }
        }
        Ok(())
    }

    fn update_session_files(
        &self,
        global_dir_rename: Option<&(PathBuf, PathBuf)>,
    ) -> Result<Vec<UpdateReport>> {
        self.update_session_files_for(&self.source_state, global_dir_rename)
    }

    /// Rewrites the session files and session index belonging to `state`,
    /// following them into their new global directory when one was renamed.
    fn update_session_files_for(
        &self,
        state: &scanner::ProjectState,
        global_dir_rename: Option<&(PathBuf, PathBuf)>,
    ) -> Result<Vec<UpdateReport>> {
        let Some(ref old_global) = state.global_project_dir else {
            return Ok(Vec::new());
        };

        let new_global = if let Some((_, ng)) = global_dir_rename {
            ng.clone()
        } else {
            old_global.clone()
        };

        let session_file_paths: Vec<PathBuf> = if self.dry_run {
            state.session_files.clone()
        } else {
            state
                .session_files
                .iter()
                .map(|f| {
                    let relative = f
                        .strip_prefix(old_global)
                        .context("session file not under expected global dir")?;
                    Ok(new_global.join(relative))
                })
                .collect::<Result<_>>()?
        };

        let mut reports =
            updater::update_files_parallel(self.fs, &session_file_paths, &self.subs, self.dry_run)?;

        if let Some(ref old_idx) = state.sessions_index {
            let idx_path = if self.dry_run {
                old_idx.clone()
            } else {
                let relative = old_idx
                    .strip_prefix(old_global)
                    .context("sessions-index not under expected global dir")?;
                new_global.join(relative)
            };
            let report = updater::update_file(self.fs, &idx_path, &self.subs, self.dry_run)?;
            reports.push(report);
        }

        Ok(reports)
    }

    fn update_local_config_for(&self, state: &scanner::ProjectState) -> Vec<UpdateReport> {
        let mut reports = Vec::new();
        if let Some(ref settings) = state.settings_json
            && let Ok(report) = updater::update_file(self.fs, settings, &self.subs, self.dry_run)
        {
            reports.push(report);
        }
        if let Some(ref mcp) = state.mcp_json
            && let Ok(report) = updater::update_file(self.fs, mcp, &self.subs, self.dry_run)
        {
            reports.push(report);
        }
        reports
    }

    fn update_history(&self) -> Vec<UpdateReport> {
        let mut reports = Vec::new();
        if let Some(ref history) = self.source_state.history_jsonl
            && let Ok(report) = updater::update_file(self.fs, history, &self.subs, self.dry_run)
        {
            reports.push(report);
        }
        reports
    }

    fn update_claude_json(&self) -> Vec<UpdateReport> {
        let mut reports = Vec::new();
        if let Some(ref cj) = self.source_state.claude_json
            && let Ok(report) = updater::rename_json_keys(self.fs, cj, &self.subs, self.dry_run)
        {
            reports.push(report);
        }
        reports
    }

    /// Projects registered under `source` in `~/.claude.json`, paired with
    /// where they land once the parent moves.
    fn discover_subprojects(&self) -> Result<Vec<(PathBuf, PathBuf)>> {
        let Some(ref cj_path) = self.source_state.claude_json else {
            return Ok(Vec::new());
        };

        let content = self.fs.read_to_string(cj_path)?;
        let root: serde_json::Value = serde_json::from_str(&content)?;
        // Project keys live under "projects"; older/simpler files put them at
        // the top level. Same two shapes `rename_json_keys` accepts.
        let Some(obj) = root
            .get("projects")
            .and_then(|v| v.as_object())
            .or_else(|| root.as_object())
        else {
            return Ok(Vec::new());
        };

        let prefix = format!("{}/", self.old_str);
        Ok(obj
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .map(|k| {
                let sub_target = format!("{}{}", self.new_str, &k[self.old_str.len()..]);
                (PathBuf::from(k), PathBuf::from(sub_target))
            })
            .collect())
    }

    /// Everything a single project owns: its global session directory, the
    /// session files inside it, and its local `.claude/settings.json` and
    /// `.mcp.json`. The parent and every sub-project go through here, so
    /// "what migrating one project means" is defined in one place.
    fn migrate_project(
        &self,
        state: &scanner::ProjectState,
        target: &Path,
    ) -> Result<(Option<GlobalRename>, Vec<UpdateReport>)> {
        let rename = self.global_rename_for(state, target)?;
        self.rename_global_dir(rename.as_ref())?;
        let mut reports = self.update_session_files_for(state, rename.as_ref())?;
        reports.extend(self.update_local_config_for(state));
        Ok((rename, reports))
    }

    /// A sub-project's global directory is encoded from its own path, so the
    /// parent's rename does not cover it. Its project directory travels with
    /// the parent's move, and the shared files (`history.jsonl`,
    /// `~/.claude.json`) are rewritten once by the parent — a path prefix
    /// match reaches every descendant there.
    fn migrate_subprojects(&self) -> Result<Vec<UpdateReport>> {
        let mut reports = Vec::new();

        for (sub_source, sub_target) in &self.subprojects {
            let state = scanner::scan(self.fs, sub_source, self.claude_home)?;
            let (_, sub_reports) = self.migrate_project(&state, sub_target)?;
            reports.extend(sub_reports);
        }

        Ok(reports)
    }

    fn move_project_dir(&self) -> Result<()> {
        if !self.dry_run && self.source != self.target && self.fs.is_dir(self.source) {
            if self.fs.is_dir(self.target) {
                // Target already exists (e.g. user moved manually, or previous run).
                // Merge source into target, preserving existing files.
                merge_dirs(self.source, self.target)
                    .context("merging project directory into existing target")?;
            } else {
                self.fs
                    .rename(self.source, self.target)
                    .context("moving project directory")?;
            }
        }
        Ok(())
    }

    fn verify(&self) -> Result<()> {
        if !self.dry_run {
            let final_state = scanner::scan(self.fs, self.target, self.claude_home)?;
            if !final_state.paths_consistent && final_state.global_project_dir.is_some() {
                bail!(
                    "verification failed: paths are not consistent after migration to {}",
                    self.target.display()
                );
            }
        }
        Ok(())
    }

    fn create_backup_if_needed(&self) -> Result<Option<PathBuf>> {
        if self.dry_run || self.no_backup {
            return Ok(None);
        }
        if let Some(ref global) = self.source_state.global_project_dir {
            // Session-only mode leaves the source project tree untouched,
            // so backing up `local/.claude/` and `.mcp.json` is wrong (and
            // unsafe: local `.claude/` may contain broken symlinks). Only
            // global/ and `.claude.json` (which IS rewritten) are archived.
            let (local, mcp) = if self.session_only {
                (None, None)
            } else {
                (
                    self.source_state.local_claude_dir.as_deref(),
                    self.source_state.mcp_json.as_deref(),
                )
            };
            return Ok(Some(backup::create_backup(
                self.source,
                self.claude_home,
                global,
                local,
                mcp,
                self.source_state.claude_json.as_deref(),
            )?));
        }
        Ok(None)
    }
}

/// JSON-merge the `sessions-index.json` files at the top of two global dirs.
///
/// When both source and target global dirs contain a `sessions-index.json`,
/// a plain file-level rename (as done by `merge_dirs`) would let source's
/// index overwrite target's, dropping target sessions from the index even
/// though their jsonl files survive on disk.
///
/// Strategy: concatenate target's `entries` array with source's entries.
/// Session IDs are UUIDs, so duplicates effectively never occur; if they
/// ever did, source entries would appear after target's (i.e. last-write
/// wins, same end state as the previous rename-based behavior). Other
/// top-level keys are taken from target (it already exists and is the
/// "winning" location after the move).
///
/// After merging, the source-side file is removed so the subsequent
/// `merge_dirs` call doesn't see it.
fn merge_sessions_index(src_dir: &Path, dst_dir: &Path) -> Result<()> {
    let src = src_dir.join("sessions-index.json");
    let dst = dst_dir.join("sessions-index.json");
    if !src.exists() {
        return Ok(());
    }
    if !dst.exists() {
        // No conflict — let `merge_dirs` handle the move.
        return Ok(());
    }

    let src_text =
        std::fs::read_to_string(&src).with_context(|| format!("reading {}", src.display()))?;
    let dst_text =
        std::fs::read_to_string(&dst).with_context(|| format!("reading {}", dst.display()))?;
    let src_v: serde_json::Value =
        serde_json::from_str(&src_text).with_context(|| format!("parsing {}", src.display()))?;
    let mut dst_v: serde_json::Value =
        serde_json::from_str(&dst_text).with_context(|| format!("parsing {}", dst.display()))?;

    let src_entries = src_v
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    match dst_v.get_mut("entries").and_then(|v| v.as_array_mut()) {
        Some(dst_entries) => dst_entries.extend(src_entries),
        None => {
            dst_v["entries"] = serde_json::Value::Array(src_entries);
        }
    }

    let merged = serde_json::to_string(&dst_v).context("serializing merged sessions-index.json")?;
    std::fs::write(&dst, merged).with_context(|| format!("writing merged {}", dst.display()))?;
    std::fs::remove_file(&src).with_context(|| format!("removing source {}", src.display()))?;
    Ok(())
}

/// Merge contents of `src` directory into `dst`, recursively.
/// Files in `src` overwrite same-named files in `dst`.
/// Files in `dst` that don't exist in `src` are preserved.
/// After merging, `src` is removed.
fn merge_dirs(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            if dst_path.is_dir() {
                merge_dirs(&src_path, &dst_path)?;
            } else {
                std::fs::rename(&src_path, &dst_path)
                    .with_context(|| format!("moving {}", src_path.display()))?;
            }
        } else {
            std::fs::rename(&src_path, &dst_path)
                .with_context(|| format!("moving {}", src_path.display()))?;
        }
    }
    std::fs::remove_dir(src).with_context(|| format!("removing empty {}", src.display()))?;
    Ok(())
}

/// Normalize a path by resolving `.` and `..` components without filesystem access.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other),
        }
    }
    result
}

fn execute_backup(fs: &dyn Fs, path: &Path, claude_home: &Path) -> Result<MigrationReport> {
    let state = scanner::scan(fs, path, claude_home)?;
    let global = state
        .global_project_dir
        .context("no global project directory found; nothing to back up")?;

    let backup_path = backup::create_backup(
        path,
        claude_home,
        &global,
        state.local_claude_dir.as_deref(),
        state.mcp_json.as_deref(),
        state.claude_json.as_deref(),
    )?;

    Ok(MigrationReport {
        action: "backup".to_owned(),
        source: None,
        target: None,
        global_dir_rename: None,
        files_updated: Vec::new(),
        backup_path: Some(backup_path),
        dry_run: false,
        nothing_to_do: false,
    })
}

fn execute_restore(backup_file: &Path) -> Result<MigrationReport> {
    backup::restore_backup(backup_file)?;

    Ok(MigrationReport {
        action: "restore".to_owned(),
        source: None,
        target: None,
        global_dir_rename: None,
        files_updated: Vec::new(),
        backup_path: Some(backup_file.to_path_buf()),
        dry_run: false,
        nothing_to_do: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MockFs;
    use std::path::Path;

    fn default_opts() -> MigrateOpts {
        MigrateOpts {
            dry_run: false,
            verbose: false,
            force: false,
            no_backup: true,
            session_only: false,
        }
    }

    fn dry_run_opts() -> MigrateOpts {
        MigrateOpts {
            dry_run: true,
            verbose: false,
            force: false,
            no_backup: true,
            session_only: false,
        }
    }

    fn setup_project(fs: &MockFs, project_path: &str, claude_home: &str) {
        let project = Path::new(project_path);
        let home = Path::new(claude_home);
        let encoded = crate::encoder::encode(project).unwrap();

        // Project dir
        fs.add_dir(project);
        fs.add_dir(&project.join(".claude"));
        fs.add_file(
            &project.join(".claude/settings.json"),
            &format!(
                r#"{{"permissions":{{"allow":["Bash({project_path}/.claude/hooks/hook.py:*)"]}}}}"#
            ),
        );

        // Global dir
        let global = home.join("projects").join(&encoded);
        fs.add_dir(&global);
        fs.add_file(
            &global.join("session.jsonl"),
            &format!(r#"{{"cwd":"{project_path}"}}"#),
        );
        fs.add_file(
            &global.join("sessions-index.json"),
            &format!(r#"{{"entries":[{{"projectPath":"{project_path}"}}]}}"#),
        );

        // History
        fs.add_file(
            &home.join("history.jsonl"),
            &format!(r#"{{"project":"{project_path}"}}"#),
        );
    }

    #[test]
    fn move_fresh_project() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: default_opts(),
        };

        let report = cmd.execute(&fs, claude_home).unwrap();

        assert!(!report.nothing_to_do);
        assert!(!report.dry_run);

        // Project dir should be moved
        assert!(!fs.exists(Path::new("/home/user/old-project")));
        assert!(fs.is_dir(Path::new("/home/user/new-project")));

        // Global dir should be renamed
        let old_encoded = crate::encoder::encode(Path::new("/home/user/old-project")).unwrap();
        let new_encoded = crate::encoder::encode(Path::new("/home/user/new-project")).unwrap();
        assert!(!fs.exists(&claude_home.join("projects").join(&old_encoded)));
        assert!(
            fs.exists(
                &claude_home
                    .join("projects")
                    .join(&new_encoded)
                    .join("session.jsonl")
            )
        );

        // Session content should reference new path
        let session = fs
            .read_to_string(
                &claude_home
                    .join("projects")
                    .join(&new_encoded)
                    .join("session.jsonl"),
            )
            .unwrap();
        assert!(session.contains("/home/user/new-project"));
        assert!(!session.contains("/home/user/old-project"));
    }

    #[test]
    fn move_already_done_nothing_to_do() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        // Project already at target with consistent paths
        setup_project(&fs, "/home/user/project", "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/project"),
            target: PathBuf::from("/home/user/project"),
            opts: default_opts(),
        };

        let report = cmd.execute(&fs, claude_home).unwrap();
        assert!(report.nothing_to_do);
    }

    #[test]
    fn move_conflict_aborts() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/source", "/home/user/.claude");
        setup_project(&fs, "/home/user/target", "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/source"),
            target: PathBuf::from("/home/user/target"),
            opts: default_opts(), // no --force
        };

        let result = cmd.execute(&fs, claude_home);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("conflict"));
    }

    #[test]
    fn move_dry_run_no_changes() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old", "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old"),
            target: PathBuf::from("/home/user/new"),
            opts: dry_run_opts(),
        };

        let report = cmd.execute(&fs, claude_home).unwrap();
        assert!(report.dry_run);

        // Nothing should have changed
        assert!(fs.exists(Path::new("/home/user/old")));
        assert!(!fs.exists(Path::new("/home/user/new")));
    }

    #[test]
    fn move_partial_migration_dir_already_moved() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");

        // Project dir already at new location
        fs.add_dir(Path::new("/home/user/new-project"));
        fs.add_dir(&Path::new("/home/user/new-project").join(".claude"));
        fs.add_file(
            &Path::new("/home/user/new-project").join(".claude/settings.json"),
            r#"{"permissions":{"allow":["Bash(/home/user/old-project/.claude/hooks/hook.py:*)"]}}"#,
        );

        // But global refs still point to old
        let old_encoded = crate::encoder::encode(Path::new("/home/user/old-project")).unwrap();
        let global = claude_home.join("projects").join(&old_encoded);
        fs.add_dir(&global);
        fs.add_file(
            &global.join("session.jsonl"),
            r#"{"cwd":"/home/user/old-project"}"#,
        );
        fs.add_file(
            &global.join("sessions-index.json"),
            r#"{"entries":[{"projectPath":"/home/user/old-project"}]}"#,
        );
        fs.add_file(
            &claude_home.join("history.jsonl"),
            r#"{"project":"/home/user/old-project"}"#,
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: default_opts(),
        };

        let report = cmd.execute(&fs, claude_home).unwrap();
        assert!(!report.nothing_to_do);

        // Global dir renamed
        let new_encoded = crate::encoder::encode(Path::new("/home/user/new-project")).unwrap();
        assert!(
            fs.exists(
                &claude_home
                    .join("projects")
                    .join(&new_encoded)
                    .join("session.jsonl")
            )
        );

        // Refs updated
        let session = fs
            .read_to_string(
                &claude_home
                    .join("projects")
                    .join(&new_encoded)
                    .join("session.jsonl"),
            )
            .unwrap();
        assert!(session.contains("/home/user/new-project"));
    }

    /// Parent with three sub-projects, all present in `~/.claude.json` and
    /// `history.jsonl`.
    fn setup_parent_with_subprojects(fs: &MockFs) {
        let home = "/home/user/.claude";
        for path in [
            "/home/user/parent",
            "/home/user/parent/a",
            "/home/user/parent/b",
            "/home/user/parent/c",
        ] {
            setup_project(fs, path, home);
        }
        // setup_project overwrites these per call; seed the real multi-project
        // shape once, afterwards.
        fs.add_file(
            Path::new("/home/user/.claude/history.jsonl"),
            concat!(
                r#"{"project":"/home/user/parent"}"#,
                "\n",
                r#"{"project":"/home/user/parent/a"}"#,
                "\n",
                r#"{"project":"/home/user/parent/b"}"#,
                "\n",
                r#"{"project":"/home/user/parent/c"}"#,
            ),
        );
        fs.add_file(
            Path::new("/home/user/.claude.json"),
            concat!(
                r#"{"/home/user/parent":{"trust":true},"#,
                r#""/home/user/parent/a":{"trust":true},"#,
                r#""/home/user/parent/b":{"trust":true},"#,
                r#""/home/user/parent/c":{"trust":true}}"#,
            ),
        );
    }

    /// The shared global files are rewritten once for the whole tree, not
    /// once per sub-project.
    #[test]
    fn subproject_move_writes_shared_files_once() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_parent_with_subprojects(&fs);

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/parent"),
            target: PathBuf::from("/home/user/moved"),
            opts: default_opts(),
        };
        cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(
            fs.write_count(Path::new("/home/user/.claude/history.jsonl")),
            1,
            "history.jsonl"
        );
        assert_eq!(
            fs.write_count(Path::new("/home/user/.claude.json")),
            1,
            ".claude.json"
        );
    }

    /// `.claude/settings.json` and `.mcp.json` are per-project, not shared,
    /// so the parent's single rewrite does not reach them. Hook commands and
    /// MCP server paths inside a sub-project would otherwise keep pointing at
    /// the pre-move directory.
    #[test]
    fn subproject_local_config_is_rewritten() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_parent_with_subprojects(&fs);

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/parent"),
            target: PathBuf::from("/home/user/moved"),
            opts: default_opts(),
        };
        cmd.execute(&fs, claude_home).unwrap();

        let settings = fs
            .get_file(Path::new("/home/user/moved/a/.claude/settings.json"))
            .expect("sub-project settings travelled with the parent move");
        assert!(
            settings.contains("/home/user/moved/a/.claude/hooks/hook.py"),
            "{settings}"
        );
        assert!(!settings.contains("/home/user/parent"), "{settings}");
    }

    /// Behaviour the E2E suite pins, asserted here too so the restructure
    /// cannot pass by simply skipping sub-projects.
    #[test]
    fn subproject_globals_and_keys_are_migrated() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_parent_with_subprojects(&fs);

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/parent"),
            target: PathBuf::from("/home/user/moved"),
            opts: default_opts(),
        };
        cmd.execute(&fs, claude_home).unwrap();

        for name in ["a", "b", "c"] {
            let new_sub = format!("/home/user/moved/{name}");
            let encoded = crate::encoder::encode(Path::new(&new_sub)).unwrap();
            let global = claude_home.join("projects").join(&encoded);
            assert!(fs.is_dir(&global), "global dir for {new_sub}");

            let session = fs.read_to_string(&global.join("session.jsonl")).unwrap();
            assert!(session.contains(&new_sub), "session cwd for {new_sub}");
        }

        let cj = fs
            .read_to_string(Path::new("/home/user/.claude.json"))
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&cj).unwrap();
        assert!(parsed.get("/home/user/moved").is_some());
        assert!(parsed.get("/home/user/moved/a").is_some());
        assert!(parsed.get("/home/user/parent/a").is_none());
    }

    /// Real `~/.claude.json` nests project keys under `"projects"`. Discovery
    /// used to look at top-level keys only, so in a real config no
    /// sub-project was ever found: `rename_json_keys` moved their keys to the
    /// new paths while their global session directories stayed behind under
    /// the old encoded name, orphaning the sessions.
    #[test]
    fn subprojects_are_found_in_nested_claude_json() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/parent", "/home/user/.claude");
        setup_project(&fs, "/home/user/parent/a", "/home/user/.claude");
        fs.add_file(
            Path::new("/home/user/.claude.json"),
            concat!(
                r#"{"numStartups":5,"projects":{"#,
                r#""/home/user/parent":{"trust":true},"#,
                r#""/home/user/parent/a":{"trust":true}}}"#,
            ),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/parent"),
            target: PathBuf::from("/home/user/moved"),
            opts: default_opts(),
        };
        cmd.execute(&fs, claude_home).unwrap();

        let encoded = crate::encoder::encode(Path::new("/home/user/moved/a")).unwrap();
        assert!(
            fs.is_dir(&claude_home.join("projects").join(&encoded)),
            "sub-project global dir must follow the key rename"
        );
    }

    fn session_only_opts() -> MigrateOpts {
        MigrateOpts {
            dry_run: false,
            verbose: false,
            force: false,
            no_backup: true,
            session_only: true,
        }
    }

    fn session_only_dry_opts() -> MigrateOpts {
        MigrateOpts {
            dry_run: true,
            verbose: false,
            force: false,
            no_backup: true,
            session_only: true,
        }
    }

    /// Sets up only the global session data + history + .claude.json for `source`.
    /// Does not create the project directory itself (caller decides).
    fn setup_session_data(fs: &MockFs, source: &str, claude_home: &str) {
        let project = Path::new(source);
        let home = Path::new(claude_home);
        let encoded = crate::encoder::encode(project).unwrap();
        let global = home.join("projects").join(&encoded);
        fs.add_dir(&global);
        fs.add_file(
            &global.join("session.jsonl"),
            &format!(r#"{{"cwd":"{source}"}}"#),
        );
        fs.add_file(
            &global.join("sessions-index.json"),
            &format!(r#"{{"entries":[{{"projectPath":"{source}"}}]}}"#),
        );
        fs.add_file(
            &home.join("history.jsonl"),
            &format!(r#"{{"project":"{source}"}}"#),
        );
        if let Some(parent) = home.parent() {
            fs.add_file(
                &parent.join(".claude.json"),
                &format!(r#"{{"{source}":{{"trust":true}}}}"#),
            );
        }
    }

    #[test]
    fn session_only_renames_global_dir() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/.claude/worktrees/branch1/proj";
        let target = "/home/user/proj";
        fs.add_dir(Path::new(source));
        fs.add_dir(Path::new(target));
        setup_session_data(&fs, source, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();
        assert!(!report.nothing_to_do);

        let old_encoded = crate::encoder::encode(Path::new(source)).unwrap();
        let new_encoded = crate::encoder::encode(Path::new(target)).unwrap();
        assert!(!fs.exists(&claude_home.join("projects").join(&old_encoded)));
        assert!(fs.is_dir(&claude_home.join("projects").join(&new_encoded)));
    }

    #[test]
    fn session_only_updates_session_jsonl_cwd() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/.claude/worktrees/branch1/proj";
        let target = "/home/user/proj";
        fs.add_dir(Path::new(source));
        fs.add_dir(Path::new(target));
        setup_session_data(&fs, source, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(),
        };
        cmd.execute(&fs, claude_home).unwrap();

        let new_encoded = crate::encoder::encode(Path::new(target)).unwrap();
        let session = fs
            .read_to_string(
                &claude_home
                    .join("projects")
                    .join(&new_encoded)
                    .join("session.jsonl"),
            )
            .unwrap();
        assert!(session.contains(target));
        assert!(!session.contains(source));
    }

    #[test]
    fn session_only_updates_sessions_index() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/.claude/worktrees/branch1/proj";
        let target = "/home/user/proj";
        fs.add_dir(Path::new(source));
        fs.add_dir(Path::new(target));
        setup_session_data(&fs, source, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(),
        };
        cmd.execute(&fs, claude_home).unwrap();

        let new_encoded = crate::encoder::encode(Path::new(target)).unwrap();
        let idx = fs
            .read_to_string(
                &claude_home
                    .join("projects")
                    .join(&new_encoded)
                    .join("sessions-index.json"),
            )
            .unwrap();
        assert!(idx.contains(target));
        assert!(!idx.contains(source));
    }

    #[test]
    fn session_only_updates_history_jsonl() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/.claude/worktrees/branch1/proj";
        let target = "/home/user/proj";
        fs.add_dir(Path::new(source));
        fs.add_dir(Path::new(target));
        setup_session_data(&fs, source, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(),
        };
        cmd.execute(&fs, claude_home).unwrap();

        let history = fs
            .read_to_string(&claude_home.join("history.jsonl"))
            .unwrap();
        assert!(history.contains(target));
        assert!(!history.contains(source));
    }

    #[test]
    fn session_only_does_not_touch_source_project_dir() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/.claude/worktrees/branch1/proj";
        let target = "/home/user/proj";
        fs.add_dir(Path::new(source));
        fs.add_dir(Path::new(target));
        // local files inside source — must remain unchanged
        fs.add_dir(&Path::new(source).join(".claude"));
        let settings_path = Path::new(source).join(".claude/settings.json");
        let settings_content = r#"{"keep":"me"}"#;
        fs.add_file(&settings_path, settings_content);
        setup_session_data(&fs, source, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(),
        };
        cmd.execute(&fs, claude_home).unwrap();

        assert!(fs.is_dir(Path::new(source)));
        assert!(fs.is_dir(&Path::new(source).join(".claude")));
        assert_eq!(fs.read_to_string(&settings_path).unwrap(), settings_content);
    }

    #[test]
    fn session_only_does_not_touch_target_project_dir() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/.claude/worktrees/branch1/proj";
        let target = "/home/user/proj";
        fs.add_dir(Path::new(source));
        fs.add_dir(Path::new(target));
        // existing unrelated file inside target — must remain unchanged
        let existing = Path::new(target).join("README.md");
        let existing_content = r"# target stays";
        fs.add_file(&existing, existing_content);
        setup_session_data(&fs, source, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(),
        };
        cmd.execute(&fs, claude_home).unwrap();

        assert!(fs.is_dir(Path::new(target)));
        assert_eq!(fs.read_to_string(&existing).unwrap(), existing_content);
        // No .claude or settings.json injected into target
        assert!(!fs.exists(&Path::new(target).join(".claude/settings.json")));
    }

    #[test]
    fn session_only_noop_when_source_equals_target() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/proj";
        fs.add_dir(Path::new(source));
        setup_session_data(&fs, source, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(source),
            opts: session_only_opts(),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();
        assert!(report.nothing_to_do);
        assert_eq!(report.action, "session-only-noop");
    }

    #[test]
    fn session_only_errors_when_target_path_missing() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/old";
        let target = "/home/user/missing";
        fs.add_dir(Path::new(source));
        // NOTE: target NOT added
        setup_session_data(&fs, source, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(),
        };
        let err = cmd.execute(&fs, claude_home).unwrap_err();
        assert!(err.to_string().contains("target path does not exist"));
    }

    #[test]
    fn session_only_errors_when_source_has_no_sessions() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/empty";
        let target = "/home/user/proj";
        fs.add_dir(Path::new(source));
        fs.add_dir(Path::new(target));
        // NOTE: no setup_session_data — source has no global session data

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(),
        };
        let err = cmd.execute(&fs, claude_home).unwrap_err();
        let msg = err.to_string();
        // Either bail in Migration::new (no project/global at source) or
        // in run_session_only's pre-check; both communicate the same idea.
        assert!(
            msg.contains("nothing to move") || msg.contains("source not found"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn session_only_dry_run_makes_no_changes() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/.claude/worktrees/branch1/proj";
        let target = "/home/user/proj";
        fs.add_dir(Path::new(source));
        fs.add_dir(Path::new(target));
        setup_session_data(&fs, source, "/home/user/.claude");

        let old_encoded = crate::encoder::encode(Path::new(source)).unwrap();
        let old_global = claude_home.join("projects").join(&old_encoded);
        let session_before = fs
            .read_to_string(&old_global.join("session.jsonl"))
            .unwrap();
        let history_before = fs
            .read_to_string(&claude_home.join("history.jsonl"))
            .unwrap();

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_dry_opts(),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();
        assert!(report.dry_run);

        // Old global still in place
        assert!(fs.is_dir(&old_global));
        let new_encoded = crate::encoder::encode(Path::new(target)).unwrap();
        assert!(!fs.exists(&claude_home.join("projects").join(&new_encoded)));
        // Files unchanged
        assert_eq!(
            fs.read_to_string(&old_global.join("session.jsonl"))
                .unwrap(),
            session_before
        );
        assert_eq!(
            fs.read_to_string(&claude_home.join("history.jsonl"))
                .unwrap(),
            history_before
        );
    }

    // NOTE: `session_only_merges_into_existing_target_global` is intentionally
    // covered as an E2E test (Phase 3), because `merge_dirs` is implemented
    // against real `std::fs` (not the `Fs` trait) and cannot be exercised
    // via `MockFs`.

    /// Bug 1: in session-only mode, when neither source dir nor source
    /// global data exist, `Migration::new` must NOT silently fall back to
    /// scanning the target and reusing its state as `source_state`. That
    /// would cause the run to "succeed" while actually doing nothing — and
    /// worse, would mark target's local `.claude/` for backup (Bug 2).
    /// The correct behavior is to bail with "source not found".
    #[test]
    fn session_only_bails_when_source_has_no_data_even_if_target_does() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/.claude/worktrees/gone/proj";
        let target = "/home/user/proj";
        // Source has NO project dir and NO global dir.
        // Target exists AND has its own session data.
        fs.add_dir(Path::new(target));
        setup_session_data(&fs, target, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(),
        };
        let err = cmd.execute(&fs, claude_home).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("source not found") || msg.contains("nothing to move"),
            "expected source-not-found bail, got: {msg}"
        );
    }

    #[test]
    fn session_only_no_backup_skips_backup() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/.claude/worktrees/branch1/proj";
        let target = "/home/user/proj";
        fs.add_dir(Path::new(source));
        fs.add_dir(Path::new(target));
        setup_session_data(&fs, source, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(), // no_backup = true
        };
        let report = cmd.execute(&fs, claude_home).unwrap();
        assert!(report.backup_path.is_none());
    }

    #[test]
    fn session_only_works_when_source_dir_already_deleted() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        let source = "/home/user/.claude/worktrees/branch1/proj";
        let target = "/home/user/proj";
        // NOTE: source dir NOT added — worktree deleted
        fs.add_dir(Path::new(target));
        setup_session_data(&fs, source, "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            opts: session_only_opts(),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();
        assert!(!report.nothing_to_do);

        // .claude.json key renamed
        let cj = fs
            .read_to_string(Path::new("/home/user/.claude.json"))
            .unwrap();
        assert!(cj.contains(target));
        assert!(!cj.contains(source));

        // Global dir moved
        let new_encoded = crate::encoder::encode(Path::new(target)).unwrap();
        assert!(fs.is_dir(&claude_home.join("projects").join(&new_encoded)));
    }

    #[test]
    fn from_cli_propagates_session_only_and_skips_mv_semantics() {
        // tmpdir for an existing target directory
        let tmp = tempfile::tempdir().unwrap();
        let target_dir = tmp.path().to_path_buf();
        let source = tmp.path().join("worktree-src");
        std::fs::create_dir_all(&source).unwrap();

        let cli = crate::cli::Cli {
            command: None,
            source: Some(source.clone()),
            target: Some(target_dir.clone()),
            dry_run: false,
            verbose: false,
            force: false,
            no_backup: false,
            session_only: true,
        };

        let cmd = Command::from_cli(&cli).unwrap();
        match cmd {
            Command::Move {
                source: s,
                target: t,
                opts,
            } => {
                assert!(opts.session_only);
                let canon_source = std::fs::canonicalize(&source).unwrap();
                let canon_target = std::fs::canonicalize(&target_dir).unwrap();
                assert_eq!(s, canon_source);
                // mv-semantics MUST NOT have appended source name to target
                assert_eq!(t, canon_target);
            }
            _ => panic!("expected Move"),
        }
    }

    #[test]
    fn from_cli_without_session_only_still_applies_mv_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let target_dir = tmp.path().to_path_buf();
        let source = tmp.path().join("myproj");
        std::fs::create_dir_all(&source).unwrap();

        let cli = crate::cli::Cli {
            command: None,
            source: Some(source.clone()),
            target: Some(target_dir.clone()),
            dry_run: false,
            verbose: false,
            force: false,
            no_backup: false,
            session_only: false,
        };

        let cmd = Command::from_cli(&cli).unwrap();
        match cmd {
            Command::Move {
                target: t, opts, ..
            } => {
                assert!(!opts.session_only);
                let canon_target = std::fs::canonicalize(&target_dir).unwrap();
                // mv-semantics: target should now be target_dir/myproj
                assert_eq!(t, canon_target.join("myproj"));
            }
            _ => panic!("expected Move"),
        }
    }
}
