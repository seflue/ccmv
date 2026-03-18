// Migration: core orchestration with idempotent migration logic

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::backup;
use crate::encoder;
use crate::fs::Fs;
use crate::scanner;
use crate::updater::{self, UpdateReport};

#[allow(clippy::struct_excessive_bools)]
pub struct MigrateOpts {
    pub dry_run: bool,
    #[allow(dead_code)]
    pub verbose: bool,
    pub force: bool,
    pub no_backup: bool,
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
        if target.is_dir()
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
    old_str: String,
    new_str: String,
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
        if !source_state.project_dir_exists && source_state.global_project_dir.is_none() {
            if source == target {
                bail!(
                    "source not found: {} does not exist and has no Claude Code project data",
                    source.display()
                );
            }
            let target_state = scanner::scan(fs, target, claude_home)?;
            if target_state.project_dir_exists || target_state.global_project_dir.is_some() {
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
                    old_str,
                    new_str,
                });
            }
            bail!(
                "source not found: {} does not exist and has no Claude Code project data",
                source.display()
            );
        }

        // Conflict check
        if source != target {
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
            old_str,
            new_str,
        })
    }

    fn run(self) -> Result<MigrationReport> {
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
        let global_dir_rename = self.compute_global_rename()?;
        self.rename_global_dir(global_dir_rename.as_ref())?;

        let mut all_reports = Vec::new();
        all_reports.extend(self.update_session_files(global_dir_rename.as_ref())?);
        all_reports.extend(self.update_local_config());
        all_reports.extend(self.update_history());

        // Migrate sub-projects (discovered from ~/.claude.json keys)
        all_reports.extend(self.migrate_subprojects()?);

        // Rename project keys in ~/.claude.json (covers main + sub-projects)
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

    fn compute_global_rename(&self) -> Result<Option<(PathBuf, PathBuf)>> {
        if let Some(ref old_global) = self.source_state.global_project_dir {
            let new_encoded = encoder::encode(self.target)?;
            let new_global = self.claude_home.join("projects").join(&new_encoded);
            if *old_global == new_global {
                Ok(None)
            } else {
                Ok(Some((old_global.clone(), new_global)))
            }
        } else {
            Ok(None)
        }
    }

    fn rename_global_dir(&self, global_dir_rename: Option<&(PathBuf, PathBuf)>) -> Result<()> {
        if !self.dry_run
            && let Some((old_global, new_global)) = global_dir_rename
        {
            if self.fs.is_dir(new_global) {
                // Target global dir already exists — merge source into it.
                // Move each entry from source to target (overwrite on conflict),
                // preserving any new sessions created at the target path.
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
        let Some(ref old_global) = self.source_state.global_project_dir else {
            return Ok(Vec::new());
        };

        let new_global = if let Some((_, ng)) = global_dir_rename {
            ng.clone()
        } else {
            old_global.clone()
        };

        let session_file_paths: Vec<PathBuf> = if self.dry_run {
            self.source_state.session_files.clone()
        } else {
            self.source_state
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

        let mut reports = updater::update_files_parallel(
            self.fs,
            &session_file_paths,
            &self.old_str,
            &self.new_str,
            self.dry_run,
        )?;

        if let Some(ref old_idx) = self.source_state.sessions_index {
            let idx_path = if self.dry_run {
                old_idx.clone()
            } else {
                let relative = old_idx
                    .strip_prefix(old_global)
                    .context("sessions-index not under expected global dir")?;
                new_global.join(relative)
            };
            let report = updater::update_file(
                self.fs,
                &idx_path,
                &self.old_str,
                &self.new_str,
                self.dry_run,
            )?;
            reports.push(report);
        }

        Ok(reports)
    }

    fn update_local_config(&self) -> Vec<UpdateReport> {
        let mut reports = Vec::new();
        if let Some(ref settings) = self.source_state.settings_json
            && let Ok(report) = updater::update_file(
                self.fs,
                settings,
                &self.old_str,
                &self.new_str,
                self.dry_run,
            )
        {
            reports.push(report);
        }
        if let Some(ref mcp) = self.source_state.mcp_json
            && let Ok(report) =
                updater::update_file(self.fs, mcp, &self.old_str, &self.new_str, self.dry_run)
        {
            reports.push(report);
        }
        reports
    }

    fn update_history(&self) -> Vec<UpdateReport> {
        let mut reports = Vec::new();
        if let Some(ref history) = self.source_state.history_jsonl
            && let Ok(report) =
                updater::update_file(self.fs, history, &self.old_str, &self.new_str, self.dry_run)
        {
            reports.push(report);
        }
        reports
    }

    fn update_claude_json(&self) -> Vec<UpdateReport> {
        let mut reports = Vec::new();
        if let Some(ref cj) = self.source_state.claude_json
            && let Ok(report) = updater::rename_json_keys(
                self.fs,
                cj,
                &self.old_str,
                &self.new_str,
                self.dry_run,
            )
        {
            reports.push(report);
        }
        reports
    }

    fn migrate_subprojects(&self) -> Result<Vec<UpdateReport>> {
        let Some(ref cj_path) = self.source_state.claude_json else {
            return Ok(Vec::new());
        };

        let content = self.fs.read_to_string(cj_path)?;
        let root: serde_json::Value = serde_json::from_str(&content)?;
        let Some(obj) = root.as_object() else {
            return Ok(Vec::new());
        };

        let prefix = format!("{}/", self.old_str);
        let subprojects: Vec<(PathBuf, PathBuf)> = obj
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .map(|k| {
                let sub_target = format!("{}{}", self.new_str, &k[self.old_str.len()..]);
                (PathBuf::from(k), PathBuf::from(sub_target))
            })
            .collect();

        let mut all_reports = Vec::new();
        let sub_opts = MigrateOpts {
            dry_run: self.dry_run,
            verbose: false,
            force: true,   // sub-project target may already exist
            no_backup: true, // parent backup covers everything
        };

        for (sub_source, sub_target) in &subprojects {
            let sub = Migration::new(self.fs, sub_source, sub_target, self.claude_home, &sub_opts);
            match sub {
                Ok(migration) => match migration.run() {
                    Ok(report) => all_reports.extend(report.files_updated),
                    Err(e) => eprintln!("warning: sub-project {}: {e:#}", sub_source.display()),
                },
                Err(e) => eprintln!("warning: sub-project {}: {e:#}", sub_source.display()),
            }
        }

        Ok(all_reports)
    }

    fn move_project_dir(&self) -> Result<()> {
        if !self.dry_run
            && self.source != self.target
            && self.fs.is_dir(self.source)
        {
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
            return Ok(Some(backup::create_backup(
                self.source,
                self.claude_home,
                global,
                self.source_state.local_claude_dir.as_deref(),
                self.source_state.mcp_json.as_deref(),
                self.source_state.claude_json.as_deref(),
            )?));
        }
        Ok(None)
    }
}

/// Merge contents of `src` directory into `dst`, recursively.
/// Files in `src` overwrite same-named files in `dst`.
/// Files in `dst` that don't exist in `src` are preserved.
/// After merging, `src` is removed.
fn merge_dirs(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)
        .with_context(|| format!("reading {}", src.display()))?
    {
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
    std::fs::remove_dir(src)
        .with_context(|| format!("removing empty {}", src.display()))?;
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
        }
    }

    fn dry_run_opts() -> MigrateOpts {
        MigrateOpts {
            dry_run: true,
            verbose: false,
            force: false,
            no_backup: true,
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

}
