// Migration: core orchestration with idempotent migration logic

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;

use crate::backup;
use crate::batch;
use crate::encoder;
use crate::fs::Fs;
use crate::scanner;
use crate::updater::{self, Substitutions, UpdateReport};

/// Where a project's global session directory moves, as `(old, new)`.
pub type GlobalRename = (PathBuf, PathBuf);

#[allow(clippy::struct_excessive_bools)]
pub struct MigrateOpts {
    pub dry_run: bool,
    pub force: bool,
    pub no_backup: bool,
    pub session_only: bool,
}

/// What became of one project. A run reports one of these per move, so a
/// batch names every project instead of collapsing to a count.
#[derive(Debug)]
pub struct MoveReport {
    pub source: PathBuf,
    pub target: PathBuf,
    pub global_dir_rename: Option<GlobalRename>,
}

#[derive(Debug)]
pub struct MigrationReport {
    pub action: String,
    pub moves: Vec<MoveReport>,
    pub files_updated: Vec<UpdateReport>,
    pub backup_paths: Vec<PathBuf>,
    pub dry_run: bool,
    pub nothing_to_do: bool,
}

pub enum Command {
    Move {
        source: PathBuf,
        target: PathBuf,
        opts: MigrateOpts,
    },
    Batch {
        moves: Vec<batch::Move>,
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

        let opts = MigrateOpts {
            dry_run: cli.dry_run,
            force: cli.force,
            no_backup: cli.no_backup,
            session_only: cli.session_only,
        };

        if let Some(ref batch_file) = cli.batch {
            let input = read_batch_input(batch_file)?;
            let moves = batch::parse(&input)?
                .into_iter()
                .map(|raw| {
                    Ok(batch::Move {
                        source: resolve_source(Path::new(&raw.source))?,
                        // No mv-semantics here: a batch line states the target
                        // outright, so appending the source name would be wrong.
                        target: normalize_path(&std::path::absolute(&raw.target)?),
                        line: Some(raw.line),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(Self::Batch { moves, opts });
        }

        match cli.paths.as_slice() {
            [] => bail!(
                "missing <SOURCE> and <TARGET> arguments\n\nUsage: ccmv <SOURCE>... <TARGET>\n       ccmv --batch <FILE>"
            ),
            [_] => bail!(
                "missing <TARGET> argument\n\nUsage: ccmv <SOURCE>... <TARGET>\n       ccmv --batch <FILE>"
            ),
            [source_arg, target_arg] => {
                let source = resolve_source(source_arg)?;
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
                    opts,
                })
            }
            // Several sources leave no room for the rename reading of `mv`, so
            // the target has to be a directory each source lands in. That also
            // makes `--session-only` append the name here while the two-path
            // form treats the target as literal — the same split `mv` makes
            // between renaming and moving into a directory.
            [source_args @ .., target_arg] => {
                let dir = normalize_path(&std::path::absolute(target_arg)?);
                if !dir.is_dir() {
                    bail!("target is not a directory: {}", dir.display());
                }
                let moves = source_args
                    .iter()
                    .map(|source_arg| {
                        let source = resolve_source(source_arg)?;
                        let name = source
                            .file_name()
                            .with_context(|| format!("source has no name: {}", source.display()))?
                            .to_owned();
                        Ok(batch::Move {
                            target: dir.join(name),
                            source,
                            line: None,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Self::Batch { moves, opts })
            }
        }
    }

    /// Execute this command, producing a `MigrationReport`.
    pub fn execute(self, fs: &dyn Fs, claude_home: &Path) -> Result<MigrationReport> {
        match self {
            // Session-only has preconditions of its own, and they belong with
            // the other plan rules rather than halfway through the run. A lone
            // move is a one-line plan.
            Self::Move {
                source,
                target,
                opts,
            } if opts.session_only => execute_batch(
                fs,
                vec![batch::Move {
                    source,
                    target,
                    line: None,
                }],
                claude_home,
                &opts,
            ),
            Self::Move {
                source,
                target,
                opts,
            } => Migration::new(fs, &[(source, target)], claude_home, &opts)?.run(),
            Self::Batch { moves, opts } => execute_batch(fs, moves, claude_home, &opts),
            Self::Backup { path } => execute_backup(fs, &path, claude_home),
            Self::Restore { backup_file } => execute_restore(&backup_file),
        }
    }
}

/// One project on the move, with everything the run needs scanned before the
/// first write.
struct Unit {
    source: PathBuf,
    target: PathBuf,
    state: scanner::ProjectState,
    /// Projects nested under `source`, as `(scanned state, new path)`. Empty
    /// until `run` discovers them; session-only moves never populate it.
    subprojects: Vec<(scanner::ProjectState, PathBuf)>,
    /// Already where it belongs, with consistent references. `active()`
    /// filters it out of every step that walks the units. It contributes no
    /// substitution either, but for a separate reason: `subs` is built before
    /// this flag exists and drops moves onto themselves.
    nothing_to_do: bool,
}

impl Unit {
    /// Takes a state that has already been scanned, as the batch path does:
    /// its plan validation walks every source anyway.
    fn from_state(source: &Path, target: &Path, state: scanner::ProjectState) -> Result<Self> {
        require_absolute(source, target)?;

        Ok(Self {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            state,
            subprojects: Vec::new(),
            nothing_to_do: false,
        })
    }

    fn scan(
        fs: &dyn Fs,
        source: &Path,
        target: &Path,
        claude_home: &Path,
        opts: &MigrateOpts,
    ) -> Result<Self> {
        require_absolute(source, target)?;

        let source_state = scanner::scan(fs, source, claude_home)?;
        // Source must exist as a directory OR have global Claude data. If
        // neither does, the migration may already have completed — fall back
        // to the target's state so an idempotent re-run still finds the
        // remaining work (e.g. `.claude.json` keys).
        let state = if source_state.has_claude_data() {
            // Conflict check — skipped in session-only mode (target global may
            // already exist; rename_global_dir will merge into it).
            if source != target && !opts.session_only {
                let target_state = scanner::scan(fs, target, claude_home)?;
                if target_state.global_project_dir.is_some() && !opts.force {
                    bail!("{}", scanner::occupied_target_error(target));
                }
            }
            source_state
        } else {
            // Session-only mode: that fallback is unsafe here. With nothing to
            // move on the source side, falling through would silently no-op
            // AND, if backup is enabled, archive target's local `.claude/` —
            // including any broken symlinks that live there.
            if source == target || opts.session_only {
                bail!("{}", scanner::missing_source_error(source));
            }
            let target_state = scanner::scan(fs, target, claude_home)?;
            if !target_state.has_claude_data() {
                bail!("{}", scanner::missing_source_error(source));
            }
            target_state
        };

        Self::from_state(source, target, state)
    }
}

fn require_absolute(source: &Path, target: &Path) -> Result<()> {
    if !source.is_absolute() {
        bail!("source path must be absolute, got: {}", source.display());
    }
    if !target.is_absolute() {
        bail!("target path must be absolute, got: {}", target.display());
    }
    Ok(())
}

/// One run over any number of moves.
///
/// Each unit owns disjoint work — its global session directory, the session
/// files inside it, its local config, its project directory. `history.jsonl`
/// and `~/.claude.json` belong to no unit: they are rewritten once for the
/// whole run, which is what `subs` covering every unit at once buys.
struct Migration<'a> {
    fs: &'a dyn Fs,
    claude_home: &'a Path,
    units: Vec<Unit>,
    /// Applied to every file this run rewrites, in one pass each.
    subs: Substitutions,
    /// The two files every project shares. Both derive from `claude_home`, so
    /// every unit's scan yields the same pair.
    history_jsonl: Option<PathBuf>,
    claude_json: Option<PathBuf>,
    dry_run: bool,
    no_backup: bool,
    session_only: bool,
}

impl<'a> Migration<'a> {
    fn new(
        fs: &'a dyn Fs,
        moves: &[(PathBuf, PathBuf)],
        claude_home: &'a Path,
        opts: &MigrateOpts,
    ) -> Result<Self> {
        let units = moves
            .iter()
            .map(|(source, target)| Unit::scan(fs, source, target, claude_home, opts))
            .collect::<Result<Vec<_>>>()?;
        Self::from_units(fs, units, claude_home, opts)
    }

    fn from_units(
        fs: &'a dyn Fs,
        units: Vec<Unit>,
        claude_home: &'a Path,
        opts: &MigrateOpts,
    ) -> Result<Self> {
        let first = units.first().context("nothing to migrate")?;

        // A move onto itself contributes no substitution: applying one would
        // rewrite every matching file to its own contents.
        let subs = Substitutions::new(
            units
                .iter()
                .filter(|unit| unit.source != unit.target)
                .map(|unit| {
                    (
                        unit.source.to_string_lossy().into_owned(),
                        unit.target.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        );

        Ok(Self {
            fs,
            claude_home,
            history_jsonl: first.state.history_jsonl.clone(),
            claude_json: first.state.claude_json.clone(),
            units,
            subs,
            dry_run: opts.dry_run,
            no_backup: opts.no_backup,
            session_only: opts.session_only,
        })
    }

    /// The units that still have work left.
    fn active(&self) -> impl Iterator<Item = &Unit> {
        self.units.iter().filter(|unit| !unit.nothing_to_do)
    }

    /// The shape of every run: settle, prepare, do the per-unit work, rewrite
    /// the shared files, verify, report. Only the per-unit middle differs
    /// between a full move and a session-only one.
    fn run(mut self) -> Result<MigrationReport> {
        self.mark_settled();
        if self.units.iter().all(|unit| unit.nothing_to_do) {
            let action = if self.session_only {
                "session-only-noop"
            } else {
                "move"
            };
            return Ok(self.settled_report(action));
        }

        if !self.session_only {
            self.discover_all_subprojects()?;
        }
        self.preflight()?;

        let backup_paths = self.create_backups_if_needed()?;

        let (renames, mut all_reports) = if self.session_only {
            self.move_sessions()?
        } else {
            self.move_projects()?
        };

        // Shared files, once each: a path-prefix match covers every unit and
        // every sub-project underneath them in the same pass.
        all_reports.extend(self.update_history());
        all_reports.extend(self.update_claude_json());

        self.verify_all()?;

        let action = if self.session_only {
            "session-only"
        } else {
            "move"
        };
        Ok(self.report(action, all_reports, backup_paths, renames))
    }

    /// Flags the units already where they belong. Session-only leaves both
    /// project directories alone, so there the paths decide on their own.
    fn mark_settled(&mut self) {
        let session_only = self.session_only;
        for unit in &mut self.units {
            unit.nothing_to_do = unit.source == unit.target
                && (session_only || (unit.state.paths_consistent && unit.state.project_dir_exists));
        }
    }

    /// Discovers before anything is rewritten — the keys still name the old
    /// paths at this point.
    fn discover_all_subprojects(&mut self) -> Result<()> {
        let project_keys = self.project_keys()?;
        let discovered = self
            .units
            .iter()
            .map(|unit| {
                if unit.nothing_to_do {
                    Ok(Vec::new())
                } else {
                    self.discover_subprojects(unit, &project_keys)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        for (unit, subprojects) in self.units.iter_mut().zip(discovered) {
            unit.subprojects = subprojects;
        }
        Ok(())
    }

    /// Every unit here touches only its own paths: its global session
    /// directory, its local config, its project directory. Distinct targets
    /// encode to distinct global directory names, and validation rejects any
    /// two moves whose paths nest, so no two units meet.
    fn move_projects(&self) -> Result<(Vec<Option<GlobalRename>>, Vec<UpdateReport>)> {
        let per_unit: Vec<(Option<GlobalRename>, Vec<UpdateReport>)> = self
            .units
            .par_iter()
            .filter(|unit| !unit.nothing_to_do)
            .map(|unit| {
                let (rename, mut reports) = self.migrate_project(&unit.state, &unit.target)?;
                reports.extend(self.migrate_subprojects(unit)?);
                self.move_project_dir(unit)?;
                Ok((rename, reports))
            })
            .collect::<Result<_>>()?;
        let (renames, per_unit_reports): (Vec<_>, Vec<_>) = per_unit.into_iter().unzip();
        Ok((renames, per_unit_reports.into_iter().flatten().collect()))
    }

    /// Moves the global session data and leaves both project directories
    /// untouched.
    fn move_sessions(&self) -> Result<(Vec<Option<GlobalRename>>, Vec<UpdateReport>)> {
        let mut renames = Vec::new();
        let mut reports = Vec::new();
        for unit in self.active() {
            let rename = self.global_rename_for(&unit.state, &unit.target)?;
            self.rename_global_dir(rename.as_ref())?;
            reports.extend(self.update_session_files_for(&unit.state, rename.as_ref())?);
            renames.push(rename);
        }
        Ok((renames, reports))
    }

    /// Session-only has nothing to verify: the source project directory may
    /// not exist and the target one is untouched by design.
    fn verify_all(&self) -> Result<()> {
        if self.session_only {
            if !self.dry_run {
                eprintln!("session-only: skipping full verify; source project dir untouched");
            }
            return Ok(());
        }
        for unit in self.active() {
            self.verify(unit)?;
        }
        Ok(())
    }

    /// A report over the whole run. `renames` lines up with the units that
    /// had work, in the order the work ran.
    fn report(
        &self,
        action: &str,
        files_updated: Vec<UpdateReport>,
        backup_paths: Vec<PathBuf>,
        renames: Vec<Option<GlobalRename>>,
    ) -> MigrationReport {
        let moves = self
            .active()
            .zip(renames)
            .map(|(unit, global_dir_rename)| MoveReport {
                source: unit.source.clone(),
                target: unit.target.clone(),
                global_dir_rename,
            })
            .collect();
        MigrationReport {
            action: action.to_owned(),
            moves,
            files_updated,
            backup_paths,
            dry_run: self.dry_run,
            nothing_to_do: false,
        }
    }

    /// Nothing left to do — every unit is already where it belongs.
    fn settled_report(&self, action: &str) -> MigrationReport {
        MigrationReport {
            action: action.to_owned(),
            moves: Vec::new(),
            files_updated: Vec::new(),
            backup_paths: Vec::new(),
            dry_run: self.dry_run,
            nothing_to_do: true,
        }
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
        if let Some(ref history) = self.history_jsonl
            && let Ok(report) = updater::update_file(self.fs, history, &self.subs, self.dry_run)
        {
            reports.push(report);
        }
        reports
    }

    fn update_claude_json(&self) -> Vec<UpdateReport> {
        let mut reports = Vec::new();
        if let Some(ref cj) = self.claude_json
            && let Ok(report) = updater::rename_json_keys(self.fs, cj, &self.subs, self.dry_run)
        {
            reports.push(report);
        }
        reports
    }

    /// Every project path `~/.claude.json` knows about. Read once per run:
    /// the file holds every project on the machine, and a batch would
    /// otherwise re-read and re-parse it for each of its moves.
    fn project_keys(&self) -> Result<Vec<String>> {
        let Some(ref cj_path) = self.claude_json else {
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
        Ok(obj.keys().cloned().collect())
    }

    /// Projects registered under `unit.source`, scanned and paired with where
    /// they land once the parent moves. Scanning here keeps the pre-flight and
    /// the migration itself on one shared view.
    fn discover_subprojects(
        &self,
        unit: &Unit,
        project_keys: &[String],
    ) -> Result<Vec<(scanner::ProjectState, PathBuf)>> {
        let old_str = unit.source.to_string_lossy();
        let new_str = unit.target.to_string_lossy();
        let prefix = format!("{old_str}/");
        project_keys
            .iter()
            .filter(|k| k.starts_with(&prefix))
            .map(|k| {
                let sub_target = format!("{new_str}{}", &k[old_str.len()..]);
                let state = scanner::scan(self.fs, Path::new(k), self.claude_home)?;
                Ok((state, PathBuf::from(sub_target)))
            })
            .collect()
    }

    /// Reads every file this run will rewrite, before the first write.
    ///
    /// Failing halfway through leaves some global directories already renamed
    /// while the project directory and the `~/.claude.json` keys still name
    /// the old path, so the run has to be stopped while nothing has happened
    /// yet. Every unreadable file is reported at once — fixing them one run at
    /// a time is unusable on a tree with many sub-projects.
    ///
    /// This is a full read, not an open-and-close: `update_file` needs the
    /// contents as UTF-8, so anything short of reading them would miss cases.
    fn preflight(&self) -> Result<()> {
        let mut to_read: Vec<&Path> = Vec::new();

        for state in self.active().flat_map(|unit| {
            std::iter::once(&unit.state).chain(unit.subprojects.iter().map(|(state, _)| state))
        }) {
            to_read.extend(state.session_files.iter().map(PathBuf::as_path));
            to_read.extend(
                [&state.sessions_index, &state.settings_json, &state.mcp_json]
                    .into_iter()
                    .flatten()
                    .map(PathBuf::as_path),
            );
        }
        to_read.extend(
            [&self.history_jsonl, &self.claude_json]
                .into_iter()
                .flatten()
                .map(PathBuf::as_path),
        );

        let unreadable: Vec<&Path> = to_read
            .into_iter()
            .filter(|path| self.fs.read_to_string(path).is_err())
            .collect();

        if !unreadable.is_empty() {
            let mut list = String::new();
            for path in &unreadable {
                let _ = write!(list, "\n  {}", path.display());
            }
            bail!(
                "cannot read {} file(s); nothing was changed:{list}",
                unreadable.len()
            );
        }

        // Session-only leaves the project directories alone, so there is no
        // rename to vet there.
        if self.session_only {
            return Ok(());
        }
        for unit in self.active() {
            if unit.source != unit.target && self.fs.is_dir(&unit.source) {
                self.fs
                    .can_rename(&unit.source, &unit.target)
                    .with_context(|| {
                        format!("cannot move {}; nothing was changed", unit.source.display())
                    })?;
            }
        }
        Ok(())
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
    fn migrate_subprojects(&self, unit: &Unit) -> Result<Vec<UpdateReport>> {
        let mut reports = Vec::new();

        for (state, sub_target) in &unit.subprojects {
            let (_, sub_reports) = self.migrate_project(state, sub_target)?;
            reports.extend(sub_reports);
        }

        Ok(reports)
    }

    fn move_project_dir(&self, unit: &Unit) -> Result<()> {
        let (source, target) = (unit.source.as_path(), unit.target.as_path());
        if !self.dry_run && source != target && self.fs.is_dir(source) {
            if self.fs.is_dir(target) {
                // Target already exists (e.g. user moved manually, or previous run).
                // Merge source into target, preserving existing files.
                merge_dirs(source, target)
                    .context("merging project directory into existing target")?;
            } else {
                self.fs
                    .rename(source, target)
                    .context("moving project directory")?;
            }
        }
        Ok(())
    }

    fn verify(&self, unit: &Unit) -> Result<()> {
        if !self.dry_run {
            let final_state = scanner::scan(self.fs, &unit.target, self.claude_home)?;
            if !final_state.paths_consistent && final_state.global_project_dir.is_some() {
                bail!(
                    "verification failed: paths are not consistent after migration to {}",
                    unit.target.display()
                );
            }
        }
        Ok(())
    }

    /// One archive per moving project. A single archive for the whole run
    /// would need a manifest format that holds more than one source path, and
    /// `restore` to match — the per-project shape is what both understand.
    fn create_backups_if_needed(&self) -> Result<Vec<PathBuf>> {
        if self.dry_run || self.no_backup {
            return Ok(Vec::new());
        }
        // In parallel: each archive is a gzip pass over one project's session
        // directory, and on a long plan that dominates the run.
        self.units
            .par_iter()
            .filter(|unit| !unit.nothing_to_do)
            .filter_map(|unit| {
                let global = unit.state.global_project_dir.as_ref()?;
                // Session-only mode leaves the source project tree untouched,
                // so backing up `local/.claude/` and `.mcp.json` is wrong (and
                // unsafe: local `.claude/` may contain broken symlinks). Only
                // global/ and `.claude.json` (which IS rewritten) are archived.
                let (local, mcp) = if self.session_only {
                    (None, None)
                } else {
                    (
                        unit.state.local_claude_dir.as_deref(),
                        unit.state.mcp_json.as_deref(),
                    )
                };
                Some(backup::create_backup(
                    &unit.source,
                    self.claude_home,
                    global,
                    local,
                    mcp,
                    self.claude_json.as_deref(),
                ))
            })
            .collect()
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

/// Batch input, read from a file or, for `-`, from standard input.
fn read_batch_input(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        return Ok(input);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// Absolute form of a source argument. Falls back to lexical normalization so
/// that an already-moved source (gone from disk) still resolves on a re-run.
fn resolve_source(arg: &Path) -> Result<PathBuf> {
    match std::fs::canonicalize(arg) {
        Ok(path) => Ok(path),
        Err(_) => Ok(normalize_path(&std::path::absolute(arg)?)),
    }
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
        moves: Vec::new(),
        files_updated: Vec::new(),
        backup_paths: vec![backup_path],
        dry_run: false,
        nothing_to_do: false,
    })
}

/// Runs a whole plan as one `Migration`, after validating it as a whole.
///
/// Validation rules out the interference the moves could otherwise cause: no
/// move's target is another move's source, and no source nests inside another.
/// What it does not give is atomicity — a failure partway through leaves the
/// earlier work applied, and the report is lost with the error.
fn execute_batch(
    fs: &dyn Fs,
    moves: Vec<batch::Move>,
    claude_home: &Path,
    opts: &MigrateOpts,
) -> Result<MigrationReport> {
    let plan = batch::BatchPlan { moves };
    let states = plan.validate(
        fs,
        claude_home,
        &batch::PlanRules {
            force: opts.force,
            session_only: opts.session_only,
        },
    )?;

    let units = plan
        .moves
        .iter()
        .zip(states)
        .map(|(mv, state)| Unit::from_state(&mv.source, &mv.target, state))
        .collect::<Result<Vec<_>>>()?;
    Migration::from_units(fs, units, claude_home, opts)?.run()
}

fn execute_restore(backup_file: &Path) -> Result<MigrationReport> {
    backup::restore_backup(backup_file)?;

    Ok(MigrationReport {
        action: "restore".to_owned(),
        moves: Vec::new(),
        files_updated: Vec::new(),
        backup_paths: vec![backup_file.to_path_buf()],
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
            force: false,
            no_backup: true,
            session_only: false,
        }
    }

    fn dry_run_opts() -> MigrateOpts {
        MigrateOpts {
            dry_run: true,
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

    /// An unreadable file anywhere in the tree must stop the run before the
    /// first write. Discovering it mid-flight would leave some global
    /// directories already renamed while the project directory and the
    /// `.claude.json` keys still name the old path.
    #[test]
    fn unreadable_session_file_stops_the_run_before_any_write() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_parent_with_subprojects(&fs);

        // Listable but not readable — stands in for a broken symlink or a
        // file the user cannot read.
        let encoded = crate::encoder::encode(Path::new("/home/user/parent/b")).unwrap();
        let broken = claude_home
            .join("projects")
            .join(&encoded)
            .join("broken.jsonl");
        fs.add_dir(&broken);

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/parent"),
            target: PathBuf::from("/home/user/moved"),
            opts: default_opts(),
        };
        let err = cmd.execute(&fs, claude_home).unwrap_err().to_string();

        assert!(err.contains("broken.jsonl"), "{err}");
        assert_eq!(
            fs.write_count(Path::new("/home/user/.claude/history.jsonl")),
            0,
            "history.jsonl must not be touched"
        );
        assert_eq!(
            fs.write_count(Path::new("/home/user/.claude.json")),
            0,
            ".claude.json must not be touched"
        );
        assert!(
            fs.is_dir(&claude_home.join("projects").join(&encoded)),
            "no global directory may have been renamed"
        );
        assert!(
            fs.is_dir(Path::new("/home/user/parent")),
            "the project directory must still be in place"
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

    /// Three independent moves in one run: the per-project work happens three
    /// times, the two files every project shares exactly once.
    #[test]
    fn batch_writes_shared_files_once() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        for name in ["a", "b", "c"] {
            setup_project(&fs, &format!("/home/user/{name}"), "/home/user/.claude");
        }
        fs.add_dir(Path::new("/home/user/dest"));
        fs.add_file(
            Path::new("/home/user/.claude/history.jsonl"),
            concat!(
                r#"{"project":"/home/user/a"}"#,
                "\n",
                r#"{"project":"/home/user/b"}"#,
                "\n",
                r#"{"project":"/home/user/c"}"#,
            ),
        );
        fs.add_file(
            Path::new("/home/user/.claude.json"),
            concat!(
                r#"{"/home/user/a":{"trust":true},"#,
                r#""/home/user/b":{"trust":true},"#,
                r#""/home/user/c":{"trust":true}}"#,
            ),
        );

        let moves = ["a", "b", "c"]
            .into_iter()
            .map(|name| batch::Move {
                source: PathBuf::from(format!("/home/user/{name}")),
                target: PathBuf::from(format!("/home/user/dest/{name}")),
                line: None,
            })
            .collect();

        Command::Batch {
            moves,
            opts: default_opts(),
        }
        .execute(&fs, claude_home)
        .unwrap();

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

        // A single write is only right if it carries all three moves.
        let history = fs
            .read_to_string(&claude_home.join("history.jsonl"))
            .unwrap();
        let cj = fs
            .read_to_string(Path::new("/home/user/.claude.json"))
            .unwrap();
        for name in ["a", "b", "c"] {
            let landed = format!("/home/user/dest/{name}");
            assert!(history.contains(&landed), "{history}");
            assert!(cj.contains(&landed), "{cj}");
        }
    }

    fn session_only_opts() -> MigrateOpts {
        MigrateOpts {
            dry_run: false,
            force: false,
            no_backup: true,
            session_only: true,
        }
    }

    fn session_only_dry_opts() -> MigrateOpts {
        MigrateOpts {
            dry_run: true,
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
        assert!(report.backup_paths.is_empty());
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

        let mut cli = cli_with_paths(vec![source.clone(), target_dir.clone()]);
        cli.session_only = true;

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

        let cli = cli_with_paths(vec![source.clone(), target_dir.clone()]);

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

    fn cli_with_paths(paths: Vec<PathBuf>) -> crate::cli::Cli {
        crate::cli::Cli {
            command: None,
            paths,
            batch: None,
            dry_run: false,
            verbose: false,
            force: false,
            no_backup: false,
            session_only: false,
        }
    }

    #[test]
    fn from_cli_moves_every_source_into_the_target_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for dir in [&dest, &a, &b] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let cmd = Command::from_cli(&cli_with_paths(vec![a, b, dest.clone()])).unwrap();
        let Command::Batch { moves, .. } = cmd else {
            panic!("expected Batch")
        };
        let canon_dest = std::fs::canonicalize(&dest).unwrap();
        let targets: Vec<_> = moves.iter().map(|m| m.target.clone()).collect();
        assert_eq!(targets, [canon_dest.join("a"), canon_dest.join("b")]);
    }

    #[test]
    fn from_cli_reads_moves_from_a_batch_file() {
        let tmp = tempfile::tempdir().unwrap();
        let list = tmp.path().join("moves.tsv");
        std::fs::write(&list, "# header\n\n/a\t/x/a\n/b\t/x/b\n").unwrap();

        let mut cli = cli_with_paths(Vec::new());
        cli.batch = Some(list);

        let cmd = Command::from_cli(&cli).unwrap();
        let Command::Batch { moves, .. } = cmd else {
            panic!("expected Batch")
        };
        let seen: Vec<_> = moves
            .iter()
            .map(|m| (m.source.clone(), m.target.clone(), m.line))
            .collect();
        assert_eq!(
            seen,
            [
                (PathBuf::from("/a"), PathBuf::from("/x/a"), Some(3)),
                (PathBuf::from("/b"), PathBuf::from("/x/b"), Some(4)),
            ]
        );
    }

    #[test]
    fn from_cli_rejects_multi_source_when_target_not_a_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let dest = tmp.path().join("dest.txt");
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&dest, "not a directory").unwrap();

        let Err(err) = Command::from_cli(&cli_with_paths(vec![a, b, dest])) else {
            panic!("expected an error")
        };
        assert!(
            err.to_string().contains("target is not a directory"),
            "{err}"
        );
    }
}
