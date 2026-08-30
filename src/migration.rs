// Migration: core orchestration with idempotent migration logic

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;

use crate::backup;
use crate::batch;
use crate::encoder;
use crate::fs::{self, Fs};
use crate::links;
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
    /// Opts into the scan `Migration::run` does between `preflight` and
    /// `create_backups_if_needed` — see CONTEXT.md's *Relink*.
    pub relink: bool,
    /// Additional Scan Roots, on top of every project's Local Claude
    /// Directory.
    pub scan_root: Vec<PathBuf>,
}

/// What became of one project. A run reports one of these per move, so a
/// batch names every project instead of collapsing to a count.
#[derive(Debug)]
pub struct MoveReport {
    pub source: PathBuf,
    pub target: PathBuf,
    pub global_dir_rename: Option<GlobalRename>,
}

/// A Relink Candidate whose `replace_symlink` call failed. The rename that
/// made it a candidate already landed, so this is reported, not fatal — see
/// "Teilfehler brechen den Lauf nicht ab" in the plan.
#[derive(Debug)]
pub struct RelinkFailure {
    pub path: PathBuf,
    pub error: String,
}

/// One row of the Relink Log — see CONTEXT.md's *Relink Log*. `new` is the
/// raw target string `replace_symlink` was actually given: exactly what is
/// now on disk at `path`.
///
/// `old` is *not* the matching raw string from before the Move — it is
/// `LinkCandidate::resolved_target`, the link's pre-Move target already
/// resolved to an absolute path. A relative link's raw old target is
/// relative to the link's *old* directory; once the link itself has
/// travelled to a new directory (see the plan's "Kandidaten, die selbst
/// mitwandern"), writing that raw string back at the *new* `path` — the
/// README's rollback, `ln -sfn "$old" "$path"` — would resolve against the
/// wrong depth whenever the Move changes how deep the link's own directory
/// sits. The resolved, absolute target has no directory to get wrong, so the
/// rollback is correct unconditionally — at the cost of turning what may
/// have been a relative link back into an absolute one.
struct RelinkLogEntry {
    path: PathBuf,
    old: PathBuf,
    new: PathBuf,
}

/// One Relink Candidate as shown in a `--dry-run` report — see the plan's
/// "Kandidaten erscheinen unter ihren alten Pfaden": `old` is the candidate's
/// path exactly as scanned, before any rename, since under `--dry-run`
/// nothing moved. `new` is where it would be repointed.
#[derive(Debug)]
pub struct RelinkCandidateReport {
    pub old: PathBuf,
    pub new: PathBuf,
}

/// Everything `run`'s Relink and Text Reference scan produced, handed to
/// `report` as one bundle: the Relink Candidates the scan found, and what
/// happened when `repoint_candidates` acted on them, alongside the Text
/// Reference side of the same walk.
struct ScanOutcome {
    relink_candidates: Vec<links::LinkCandidate>,
    relink_failures: Vec<RelinkFailure>,
    /// How many links this run actually repointed, and where the Relink Log
    /// recording them landed.
    relinked_count: usize,
    relink_log_path: Option<PathBuf>,
    text_reference_count: usize,
    text_refs_report_path: Option<PathBuf>,
    /// The Text References this run found, handed to `report` for its
    /// `--dry-run` listing — see `MigrationReport::text_reference_findings`.
    text_reference_findings: Vec<links::TextReference>,
}

#[derive(Debug)]
pub struct MigrationReport {
    pub action: String,
    pub moves: Vec<MoveReport>,
    pub files_updated: Vec<UpdateReport>,
    pub backup_paths: Vec<PathBuf>,
    pub dry_run: bool,
    pub nothing_to_do: bool,
    pub relink_failures: Vec<RelinkFailure>,
    /// Populated only under `--dry-run`: on a real run these have already
    /// been written, and only failures are worth reporting.
    pub relink_candidates: Vec<RelinkCandidateReport>,
    /// How many links this run repointed. These writes land in *other*
    /// people's projects, which no backup covers, so a run that stays silent
    /// about them leaves the user unaware that anything outside the moved
    /// project changed at all.
    pub relinked_count: usize,
    /// Where the Relink Log recording those writes landed. `None` when
    /// nothing was repointed, or under `--dry-run`, when nothing is written.
    pub relink_log_path: Option<PathBuf>,
    /// How many Text References this run found — see CONTEXT.md's *Text
    /// Reference*. Zero unless `--relink` was given.
    pub text_reference_count: usize,
    /// Where the Text Reference report landed. `None` when there was
    /// nothing to report, or under `--dry-run`, when nothing is written at
    /// all — see `Migration::write_text_refs_log`.
    pub text_refs_report_path: Option<PathBuf>,
    /// Every Text Reference this run found, populated only under
    /// `--dry-run` — the same rule `relink_candidates` follows for
    /// symlinks. On a real run these have already been written to
    /// `text_refs_report_path`, so listing them again here would just
    /// restate that file.
    pub text_reference_findings: Vec<links::TextReference>,
}

impl MigrationReport {
    /// Non-zero when this run left work undone that the caller should notice
    /// on the process exit status — currently, an alive Relink Candidate that
    /// could not be repointed. The Move itself already landed by the time
    /// this is checked, so a non-zero code reports incomplete cleanup, not a
    /// failed run.
    pub fn exit_code(&self) -> i32 {
        i32::from(!self.relink_failures.is_empty())
    }
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
            relink: cli.relink,
            scan_root: cli.scan_root.clone(),
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
#[allow(clippy::struct_excessive_bools)]
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
    /// Whether to scan for and repoint Relink Candidates. `--session-only`
    /// never sets this — the two are mutually exclusive from `src/cli.rs`.
    relink: bool,
    scan_root: Vec<PathBuf>,
    /// Backing store for `run_timestamp` — see its doc comment.
    timestamp: std::sync::OnceLock<String>,
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
            relink: opts.relink,
            scan_root: opts.scan_root.clone(),
            timestamp: std::sync::OnceLock::new(),
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

        // One scan for the whole run, before any unit renames itself: the
        // distinction between "already dead" and "broken by this Move" only
        // holds before the first write. `move_projects` is a `par_iter` —
        // a scan inside that loop would see the victims of a sibling unit's
        // rename as already dead and leave them unrepaired forever.
        let (relink_candidates, text_references) = self.scan_relink_candidates()?;

        let backup_paths = self.create_backups_if_needed()?;

        let (renames, mut all_reports) = if self.session_only {
            self.move_sessions()?
        } else {
            self.move_projects()?
        };

        // Only after the renames landed: a candidate written earlier would
        // bet on the Move succeeding, and a failed Move must relink nothing.
        let (relink_log_entries, mut relink_failures) = self.repoint_candidates(&relink_candidates);
        // A failed log write joins the same collected failures as a failed
        // relink, rather than aborting the run: the renames and the relinks
        // above are already irreversible, and a hard error here would print
        // "Error:" and discard the very report of what was repointed — see
        // "Rückweg: Relink-Log statt Backup-Erweiterung" in the plan.
        let relinked_count = relink_log_entries.len();
        let relink_log_path = match self.write_relink_log(&relink_log_entries, &backup_paths) {
            Ok(path) => path,
            Err(error) => {
                relink_failures.push(RelinkFailure {
                    path: self
                        .relink_log_path(&backup_paths)
                        .unwrap_or_else(|_| PathBuf::from("<relink log>")),
                    error: format!("{error:#}"),
                });
                None
            }
        };

        // The Text Reference report rewrites nothing either way, so it does
        // not have to wait on the Move the way a Relink does — but it is
        // written here regardless, next to the Relink Log, so both land
        // under the same `{encoded}-{timestamp}` pair instead of each
        // picking its own fallback timestamp. A write failure joins
        // `relink_failures` the same way a Relink Log failure does.
        let text_reference_count = text_references.len();
        let text_refs_report_path = match self.write_text_refs_log(&text_references, &backup_paths)
        {
            Ok(path) => path,
            Err(error) => {
                relink_failures.push(RelinkFailure {
                    path: self
                        .text_refs_log_path(&backup_paths)
                        .unwrap_or_else(|_| PathBuf::from("<text references log>")),
                    error: format!("{error:#}"),
                });
                None
            }
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
        Ok(self.report(
            action,
            all_reports,
            backup_paths,
            renames,
            ScanOutcome {
                relink_candidates,
                relink_failures,
                relinked_count,
                relink_log_path,
                text_reference_count,
                text_refs_report_path,
                text_reference_findings: text_references,
            },
        ))
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

    /// One walk over this run's Scan Roots feeding two collectors — Relink
    /// Candidates and Text References, matched against `subs` — see
    /// CONTEXT.md's *Relink Candidate* and *Text Reference*, and the plan's
    /// "derselbe Walk, zwei Sammler". A no-op, without touching `fs` at all,
    /// when `--relink` was not given.
    fn scan_relink_candidates(
        &self,
    ) -> Result<(Vec<links::LinkCandidate>, Vec<links::TextReference>)> {
        if !self.relink {
            return Ok((Vec::new(), Vec::new()));
        }
        let roots = links::scan_roots(self.fs, self.claude_json.as_deref(), &self.scan_root)?;
        let paths = links::walk_scan_roots(self.fs, &roots);
        let candidates = paths
            .par_iter()
            .filter_map(|path| links::relink_candidate(self.fs, path, &self.subs))
            .collect();
        let text_references = paths
            .par_iter()
            .flat_map(|path| links::text_references(self.fs, path, &self.subs))
            .collect();
        Ok((candidates, text_references))
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

    /// Repoints every alive candidate at its new target, after the Move has
    /// landed. A candidate's own path is re-based through `subs.match_path`
    /// first — the same rebasing `update_session_files_for` does for session
    /// file paths — so a link that travelled with its project is rewritten
    /// at its new location, not its by-then-nonexistent old one. Dead
    /// candidates are left alone, and under `--dry-run` nothing is written
    /// at all, the candidates' old paths standing in for a report of what
    /// would change.
    ///
    /// A single failed `replace_symlink` — a foreign tree that refuses
    /// writes, say — does not stop the rest: the Move already landed and is
    /// irreversible at this point, so every other candidate still gets
    /// repointed. Failures are collected and handed back for the report
    /// instead.
    ///
    /// Returns one `RelinkLogEntry` per successful write, for
    /// `write_relink_log`, alongside the failures.
    fn repoint_candidates(
        &self,
        candidates: &[links::LinkCandidate],
    ) -> (Vec<RelinkLogEntry>, Vec<RelinkFailure>) {
        if self.dry_run {
            return (Vec::new(), Vec::new());
        }
        let results: Vec<Result<RelinkLogEntry, RelinkFailure>> = candidates
            .par_iter()
            .filter(|c| c.alive)
            .map(|candidate| {
                let write_path = self
                    .subs
                    .rebase(&candidate.path)
                    .unwrap_or_else(|| candidate.path.clone());
                let raw_target = if candidate.is_relative {
                    links::relative_target(&write_path, &candidate.new_target)
                } else {
                    candidate.new_target.clone()
                };
                match self.fs.replace_symlink(&write_path, &raw_target) {
                    Ok(()) => Ok(RelinkLogEntry {
                        path: write_path,
                        old: candidate.resolved_target.clone(),
                        new: raw_target,
                    }),
                    Err(error) => Err(RelinkFailure {
                        path: write_path,
                        error: format!("{error:#}"),
                    }),
                }
            })
            .collect();

        let mut entries = Vec::new();
        let mut failures = Vec::new();
        for result in results {
            match result {
                Ok(entry) => entries.push(entry),
                Err(failure) => failures.push(failure),
            }
        }
        (entries, failures)
    }

    /// Where a run-scoped artefact lands, named the same way as the run's
    /// backup archive — `{encoded}-{timestamp}.{suffix}` next to
    /// `{encoded}-{timestamp}.tar.gz` (see `backup::create_backup`) — so the
    /// two are visibly a pair without a manifest. `--no-backup` leaves no
    /// archive to share a name with, so the artefact gets its own name
    /// instead: the first active unit's source, encoded the same way, with
    /// `run_timestamp` — shared across every artefact this run writes without
    /// a backup, so a Relink Log and a Text Reference report from the same
    /// run still carry the same timestamp.
    fn artifact_log_path(&self, backup_paths: &[PathBuf], suffix: &str) -> Result<PathBuf> {
        let backup_dir = self.claude_home.join("backups/ccmv");
        if let Some(backup_path) = backup_paths.first() {
            let file_name = backup_path.file_name().with_context(|| {
                format!("backup path has no file name: {}", backup_path.display())
            })?;
            let stem = file_name.to_string_lossy();
            let stem = stem.strip_suffix(".tar.gz").with_context(|| {
                format!(
                    "backup path does not end in .tar.gz: {}",
                    backup_path.display()
                )
            })?;
            return Ok(backup_dir.join(format!("{stem}.{suffix}")));
        }
        let source = &self.active().next().context("nothing to migrate")?.source;
        let encoded = encoder::encode(source)?;
        Ok(backup_dir.join(format!("{encoded}-{}.{suffix}", self.run_timestamp())))
    }

    /// Where the Relink Log lands — see CONTEXT.md's *Relink Log* and
    /// `artifact_log_path`.
    fn relink_log_path(&self, backup_paths: &[PathBuf]) -> Result<PathBuf> {
        self.artifact_log_path(backup_paths, "relink.tsv")
    }

    /// Where the Text Reference report lands — see CONTEXT.md's *Text
    /// Reference* and `artifact_log_path`. `.textrefs.tsv` in place of
    /// `.relink.tsv`, sitting next to it and the backup archive so a run's
    /// three artefacts are visibly a set.
    fn text_refs_log_path(&self, backup_paths: &[PathBuf]) -> Result<PathBuf> {
        self.artifact_log_path(backup_paths, "textrefs.tsv")
    }

    /// The timestamp `artifact_log_path` falls back to when there is no
    /// backup archive to borrow one from. Computed once and cached: it is
    /// asked for at least twice per run-without-backup (once per artefact,
    /// again on a write failure's error path), and two independent
    /// `chrono::Local::now()` calls could in principle land a second apart,
    /// breaking the pairing this is meant to preserve.
    fn run_timestamp(&self) -> &str {
        self.timestamp
            .get_or_init(|| chrono::Local::now().format("%Y%m%d-%H%M%S").to_string())
    }

    /// Writes the Relink Log — see CONTEXT.md's *Relink Log* — one
    /// `path<TAB>old<TAB>new` row per repointed link. Absent when `entries`
    /// is empty: an empty log file would read as a failed run, not a quiet
    /// one.
    ///
    /// Sorted by `path`, with the same rationale `text_refs_report` documents
    /// for its own sort: the Scan Roots feeding `entries` are walked in
    /// parallel, so nothing else gives these rows a stable order across
    /// machines and runs.
    ///
    /// Returns where the log landed, so the run can say so. A record nobody
    /// is pointed at does not serve as the account of what ccmv wrote into
    /// other people's projects.
    fn write_relink_log(
        &self,
        entries: &[RelinkLogEntry],
        backup_paths: &[PathBuf],
    ) -> Result<Option<PathBuf>> {
        if entries.is_empty() {
            return Ok(None);
        }
        let log_path = self.relink_log_path(backup_paths)?;
        let backup_dir = log_path
            .parent()
            .with_context(|| format!("relink log path has no parent: {}", log_path.display()))?;
        self.fs.create_dir_all(backup_dir)?;

        let mut sorted: Vec<&RelinkLogEntry> = entries.iter().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));

        let mut content = String::new();
        for entry in sorted {
            let _ = writeln!(
                content,
                "{}\t{}\t{}",
                entry.path.display(),
                entry.old.display(),
                entry.new.display()
            );
        }
        self.fs.write_atomically(&log_path, &content)?;
        Ok(Some(log_path))
    }

    /// Writes the Text Reference report — see CONTEXT.md's *Text
    /// Reference* — and returns the path it landed at. `None` when there is
    /// nothing to report (the same rule `write_relink_log` follows for an
    /// empty Relink Log) or under `--dry-run`, when nothing is written at
    /// all: unlike a Relink, a Text Reference is read-only regardless, but
    /// the report only means anything alongside the changes a real run just
    /// made.
    fn write_text_refs_log(
        &self,
        findings: &[links::TextReference],
        backup_paths: &[PathBuf],
    ) -> Result<Option<PathBuf>> {
        if self.dry_run || findings.is_empty() {
            return Ok(None);
        }
        let log_path = self.text_refs_log_path(backup_paths)?;
        let backup_dir = log_path.parent().with_context(|| {
            format!(
                "text references log path has no parent: {}",
                log_path.display()
            )
        })?;
        self.fs.create_dir_all(backup_dir)?;
        self.fs
            .write_atomically(&log_path, &text_refs_report(findings))?;
        Ok(Some(log_path))
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
        scan_outcome: ScanOutcome,
    ) -> MigrationReport {
        let ScanOutcome {
            relink_candidates,
            relink_failures,
            relinked_count,
            relink_log_path,
            text_reference_count,
            text_refs_report_path,
            text_reference_findings,
        } = scan_outcome;
        let moves = self
            .active()
            .zip(renames)
            .map(|(unit, global_dir_rename)| MoveReport {
                source: unit.source.clone(),
                target: unit.target.clone(),
                global_dir_rename,
            })
            .collect();
        // Only meaningful under --dry-run: on a real run repoint_candidates
        // has already repointed every alive candidate, so listing them again
        // here would just restate the move. Dead candidates are left alone and
        // so are not shown as pending changes.
        let relink_candidates = if self.dry_run {
            relink_candidates
                .iter()
                .filter(|c| c.alive)
                .map(|c| RelinkCandidateReport {
                    old: c.path.clone(),
                    new: c.new_target.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        // Same rule as `relink_candidates` above: on a real run these have
        // already been written to `text_refs_report_path`, so they are only
        // worth handing back to the caller under `--dry-run`.
        let text_reference_findings = if self.dry_run {
            text_reference_findings
        } else {
            Vec::new()
        };
        MigrationReport {
            action: action.to_owned(),
            moves,
            files_updated,
            backup_paths,
            dry_run: self.dry_run,
            nothing_to_do: false,
            relink_failures,
            relink_candidates,
            relinked_count,
            relink_log_path,
            text_reference_count,
            text_refs_report_path,
            text_reference_findings,
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
            relink_failures: Vec::new(),
            relink_candidates: Vec::new(),
            relinked_count: 0,
            relink_log_path: None,
            text_reference_count: 0,
            text_refs_report_path: None,
            text_reference_findings: Vec::new(),
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
                fs::can_rename(self.fs, &unit.source, &unit.target).with_context(|| {
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
                unit.state.global_project_dir.as_ref()?;
                Some(backup::create_backup(
                    &unit.source,
                    self.claude_home,
                    &unit.state,
                    !self.session_only,
                ))
            })
            .collect()
    }
}

/// Renders `findings` as the Text Reference report's TSV body — one
/// `file<TAB>line<TAB>old<TAB>new` row per Text Reference, see CONTEXT.md's
/// *Text Reference*. Sorted by `(file, line)`: the scan that produces
/// `findings` runs its files through a `par_iter`, so nothing else puts a
/// file's rows next to each other or the files themselves in a stable order
/// — sorting does both, and as a side effect lets a consumer select one
/// project's rows with a prefix match on the first column, which always
/// carries the full absolute path.
fn text_refs_report(findings: &[links::TextReference]) -> String {
    let mut sorted: Vec<&links::TextReference> = findings.iter().collect();
    sorted.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    let mut content = String::new();
    for finding in sorted {
        let _ = writeln!(
            content,
            "{}\t{}\t{}\t{}",
            finding.file.display(),
            finding.line,
            finding.old,
            finding.new
        );
    }
    content
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
    if state.global_project_dir.is_none() {
        bail!("no global project directory found; nothing to back up");
    }

    let backup_path = backup::create_backup(path, claude_home, &state, true)?;

    Ok(MigrationReport {
        action: "backup".to_owned(),
        moves: Vec::new(),
        files_updated: Vec::new(),
        backup_paths: vec![backup_path],
        dry_run: false,
        nothing_to_do: false,
        relink_failures: Vec::new(),
        relink_candidates: Vec::new(),
        relinked_count: 0,
        relink_log_path: None,
        text_reference_count: 0,
        text_refs_report_path: None,
        text_reference_findings: Vec::new(),
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
        relink_failures: Vec::new(),
        relink_candidates: Vec::new(),
        relinked_count: 0,
        relink_log_path: None,
        text_reference_count: 0,
        text_refs_report_path: None,
        text_reference_findings: Vec::new(),
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
            relink: false,
            scan_root: Vec::new(),
        }
    }

    fn dry_run_opts() -> MigrateOpts {
        MigrateOpts {
            dry_run: true,
            force: false,
            no_backup: true,
            session_only: false,
            relink: false,
            scan_root: Vec::new(),
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
            relink: false,
            scan_root: Vec::new(),
        }
    }

    fn session_only_dry_opts() -> MigrateOpts {
        MigrateOpts {
            dry_run: true,
            force: false,
            no_backup: true,
            session_only: true,
            relink: false,
            scan_root: Vec::new(),
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

    fn relink_opts(scan_root: Vec<PathBuf>) -> MigrateOpts {
        MigrateOpts {
            dry_run: false,
            force: false,
            no_backup: true,
            session_only: false,
            relink: true,
            scan_root,
        }
    }

    fn relink_dry_run_opts(scan_root: Vec<PathBuf>) -> MigrateOpts {
        MigrateOpts {
            dry_run: true,
            force: false,
            no_backup: true,
            session_only: false,
            relink: true,
            scan_root,
        }
    }

    #[test]
    fn relink_repoints_absolute_link() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");
        fs.add_dir(Path::new("/outside"));
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/home/user/old-project/target.txt"),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(
            fs.read_link(Path::new("/outside/link")).unwrap(),
            PathBuf::from("/home/user/new-project/target.txt")
        );
    }

    #[test]
    fn relink_keeps_relative_link_relative() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");
        fs.add_dir(Path::new("/home/user/somewhere/sub"));
        // From /home/user/somewhere/sub, "../../old-project/target.txt"
        // resolves to /home/user/old-project/target.txt.
        fs.add_symlink(
            Path::new("/home/user/somewhere/sub/link"),
            Path::new("../../old-project/target.txt"),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/home/user/somewhere")]),
        };
        cmd.execute(&fs, claude_home).unwrap();

        let written = fs
            .read_link(Path::new("/home/user/somewhere/sub/link"))
            .unwrap();
        assert!(written.is_relative(), "{}", written.display());
        assert_eq!(
            written,
            PathBuf::from("../../new-project/target.txt"),
            "{}",
            written.display()
        );
    }

    #[test]
    fn relink_repoints_self_link_at_new_location() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_dir(Path::new("/home/user/old-project/.claude/lib"));
        fs.add_file(
            Path::new("/home/user/old-project/.claude/lib/real-tool"),
            "content",
        );
        fs.add_symlink(
            Path::new("/home/user/old-project/.claude/bin/tool"),
            Path::new("/home/user/old-project/.claude/lib/real-tool"),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/home/user/old-project")]),
        };
        cmd.execute(&fs, claude_home).unwrap();

        assert!(
            !fs.is_symlink(Path::new("/home/user/old-project/.claude/bin/tool")),
            "old path must be gone; the whole project directory moved"
        );
        assert_eq!(
            fs.read_link(Path::new("/home/user/new-project/.claude/bin/tool"))
                .unwrap(),
            PathBuf::from("/home/user/new-project/.claude/lib/real-tool")
        );
    }

    #[test]
    fn relink_skips_link_that_was_already_dead() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        // /home/user/old-project/gone.txt was never registered — dead before
        // the Move.
        fs.add_dir(Path::new("/outside"));
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/home/user/old-project/gone.txt"),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(
            fs.read_link(Path::new("/outside/link")).unwrap(),
            PathBuf::from("/home/user/old-project/gone.txt"),
            "a link dead before the Move is not this Run's business"
        );
    }

    #[test]
    fn relink_does_nothing_when_move_fails() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");
        fs.add_dir(Path::new("/outside"));
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/home/user/old-project/target.txt"),
        );
        // The rename can never succeed: the target's parent directory
        // refuses writes.
        fs.add_dir(Path::new("/readonly-dest"));
        fs.add_readonly_dir(Path::new("/readonly-dest"));

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/readonly-dest/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        let err = cmd.execute(&fs, claude_home).unwrap_err();

        assert!(err.to_string().contains("nothing was changed"), "{err}");
        assert_eq!(
            fs.read_link(Path::new("/outside/link")).unwrap(),
            PathBuf::from("/home/user/old-project/target.txt"),
            "a failed Move must relink nothing"
        );
    }

    /// Two units in one batch, joined by a link that lives inside unit A's
    /// source tree and points into unit B's source tree — not a link outside
    /// both, matching only one unit's substitution, which any per-unit scan
    /// would still catch on that unit's own turn regardless of the other's.
    /// Here, whether the link is found *at all* depends on a *sibling* unit's
    /// rename: a scan that ran after A had already renamed itself away would
    /// find nothing left at `/home/user/a/...` to scan, and would never
    /// discover this link, let alone repoint it. Only a scan that runs before
    /// every rename in the batch finds it.
    #[test]
    fn relink_scans_before_any_rename_in_batch() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/a", "/home/user/.claude");
        setup_project(&fs, "/home/user/b", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/b/marker.txt"), "content");
        fs.add_symlink(
            Path::new("/home/user/a/.claude/link"),
            Path::new("/home/user/b/marker.txt"),
        );
        fs.add_dir(Path::new("/home/user/dest"));

        let moves = vec![
            batch::Move {
                source: PathBuf::from("/home/user/a"),
                target: PathBuf::from("/home/user/dest/a"),
                line: None,
            },
            batch::Move {
                source: PathBuf::from("/home/user/b"),
                target: PathBuf::from("/home/user/dest/b"),
                line: None,
            },
        ];

        Command::Batch {
            moves,
            opts: relink_opts(vec![PathBuf::from("/home/user/a")]),
        }
        .execute(&fs, claude_home)
        .unwrap();

        assert_eq!(
            fs.read_link(Path::new("/home/user/dest/a/.claude/link"))
                .unwrap(),
            PathBuf::from("/home/user/dest/b/marker.txt"),
            "the link's target was alive before the batch started, regardless of which unit's \
             rename ran first — a scan after A's rename would never have found this link at all"
        );
    }

    /// A link in a read-only directory cannot be written; the run must not
    /// stop there — the rename already landed, and a link elsewhere is
    /// demonstrably still repointed afterwards.
    #[test]
    fn relink_continues_after_write_failure() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");

        fs.add_dir(Path::new("/readonly"));
        fs.add_readonly_dir(Path::new("/readonly"));
        fs.add_symlink(
            Path::new("/readonly/link"),
            Path::new("/home/user/old-project/target.txt"),
        );

        fs.add_dir(Path::new("/outside"));
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/home/user/old-project/target.txt"),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/readonly"), PathBuf::from("/outside")]),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(
            report.relink_failures.len(),
            1,
            "{:?}",
            report.relink_failures
        );
        assert_eq!(
            fs.read_link(Path::new("/readonly/link")).unwrap(),
            PathBuf::from("/home/user/old-project/target.txt"),
            "a write failure must not silently succeed"
        );
        assert_eq!(
            fs.read_link(Path::new("/outside/link")).unwrap(),
            PathBuf::from("/home/user/new-project/target.txt"),
            "a link elsewhere must still be repointed despite the failure"
        );
    }

    #[test]
    fn relink_write_failure_sets_exit_code() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");

        fs.add_dir(Path::new("/readonly"));
        fs.add_readonly_dir(Path::new("/readonly"));
        fs.add_symlink(
            Path::new("/readonly/link"),
            Path::new("/home/user/old-project/target.txt"),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/readonly")]),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(
            report.relink_failures.len(),
            1,
            "{:?}",
            report.relink_failures
        );
        assert_eq!(
            report.exit_code(),
            1,
            "a genuine write failure from replace_symlink must set a non-zero exit code"
        );
    }

    /// A run that repoints links in other people's projects has to say so.
    /// The dry run lists its candidates, but a real run used to report
    /// nothing at all: no count, and no pointer to the Relink Log, which is
    /// the only record that those foreign trees were written to.
    #[test]
    fn real_run_reports_how_many_links_it_repointed_and_where_the_log_is() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");
        fs.add_dir(Path::new("/outside"));
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/home/user/old-project/target.txt"),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert!(!report.dry_run);
        assert_eq!(report.relinked_count, 1);
        let log_path = report
            .relink_log_path
            .as_ref()
            .expect("a real run names the relink log it wrote");
        assert!(
            log_path.to_string_lossy().ends_with(".relink.tsv"),
            "{}",
            log_path.display()
        );
        assert!(
            fs.get_file(log_path).is_some(),
            "the reported path must be the file that was actually written"
        );
    }

    /// Nothing repointed, nothing to announce — and no log path pointing at a
    /// file that was never written.
    #[test]
    fn run_without_relink_reports_no_repointed_links() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: default_opts(),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(report.relinked_count, 0);
        assert!(report.relink_log_path.is_none());
    }

    #[test]
    fn relink_log_records_every_change() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");
        fs.add_dir(Path::new("/outside"));
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/home/user/old-project/target.txt"),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        cmd.execute(&fs, claude_home).unwrap();

        let logs = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        let log_path = logs
            .iter()
            .find(|p| p.to_string_lossy().ends_with(".relink.tsv"))
            .expect("relink log written");
        let content = fs.get_file(log_path).unwrap();

        assert_eq!(
            content,
            "/outside/link\t/home/user/old-project/target.txt\t/home/user/new-project/target.txt\n"
        );
    }

    /// `write_relink_log` must not just transcribe `entries` in arrival
    /// order — the same argument `text_refs_report`'s sort documents: the
    /// scan feeding these rows runs its Scan Roots through a `par_iter`, so
    /// nothing else gives them a stable order. Entries are handed in
    /// deliberately out of path order so this assertion would fail if the
    /// sort were dropped and they were written back exactly as given.
    #[test]
    fn write_relink_log_sorts_rows_by_path() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        let state = scanner::scan(&fs, Path::new("/home/user/old-project"), claude_home).unwrap();
        let unit = Unit::from_state(
            Path::new("/home/user/old-project"),
            Path::new("/home/user/new-project"),
            state,
        )
        .unwrap();
        let migration =
            Migration::from_units(&fs, vec![unit], claude_home, &relink_opts(Vec::new())).unwrap();

        let entries = vec![
            RelinkLogEntry {
                path: PathBuf::from("/outside/c"),
                old: PathBuf::from("/old/c"),
                new: PathBuf::from("/new/c"),
            },
            RelinkLogEntry {
                path: PathBuf::from("/outside/a"),
                old: PathBuf::from("/old/a"),
                new: PathBuf::from("/new/a"),
            },
            RelinkLogEntry {
                path: PathBuf::from("/outside/b"),
                old: PathBuf::from("/old/b"),
                new: PathBuf::from("/new/b"),
            },
        ];

        migration.write_relink_log(&entries, &[]).unwrap();

        let logs = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        let log_path = logs
            .iter()
            .find(|p| p.to_string_lossy().ends_with(".relink.tsv"))
            .expect("relink log written");
        let content = fs.get_file(log_path).unwrap();

        assert_eq!(
            content,
            "/outside/a\t/old/a\t/new/a\n/outside/b\t/old/b\t/new/b\n/outside/c\t/old/c\t/new/c\n"
        );
    }

    /// The Relink Log is the only way back into a foreign project ccmv wrote
    /// to — CONTEXT.md's *Relink Log*: "der einzige Nachweis darüber, dass
    /// ccmv in fremde Projekte geschrieben hat". For a relative link that
    /// travels with its own project (its own path is rebased by the same
    /// Move, one directory deeper than before), the README's rollback line,
    /// `ln -sfn "$old" "$path"`, must recreate the link's *pre-Move*
    /// resolution — not some other path that merely shares a name.
    ///
    /// `a` holds a relative link into `b`; both move in the same batch, `a`
    /// one segment deeper than before, `b` unchanged in depth relative to
    /// `a`'s new location — exactly the shape that breaks a *raw* old target
    /// carried over to the new path.
    #[test]
    fn relink_log_old_column_survives_the_readme_rollback() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/a", "/home/user/.claude");
        setup_project(&fs, "/home/user/b", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/b/marker.txt"), "content");
        // From /home/user/a/.claude, "../../b/marker.txt" resolves to
        // /home/user/b/marker.txt. The link's own path (under a) and its
        // target (under b) belong to two *different* Substitution entries,
        // so this is a candidate — not the excluded self-referential case,
        // which only excludes a link and its target moving under the *same*
        // entry.
        fs.add_symlink(
            Path::new("/home/user/a/.claude/link"),
            Path::new("../../b/marker.txt"),
        );
        fs.add_dir(Path::new("/home/user/dest"));

        let moves = vec![
            batch::Move {
                source: PathBuf::from("/home/user/a"),
                target: PathBuf::from("/home/user/dest/a"),
                line: None,
            },
            batch::Move {
                source: PathBuf::from("/home/user/b"),
                target: PathBuf::from("/home/user/dest/b"),
                line: None,
            },
        ];

        Command::Batch {
            moves,
            opts: relink_opts(vec![PathBuf::from("/home/user/a")]),
        }
        .execute(&fs, claude_home)
        .unwrap();

        let logs = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        let log_path = logs
            .iter()
            .find(|p| p.to_string_lossy().ends_with(".relink.tsv"))
            .expect("relink log written");
        let content = fs.get_file(log_path).unwrap();
        let row = content.lines().next().expect("one relink row");
        let mut columns = row.split('\t');
        let path = PathBuf::from(columns.next().unwrap());
        let old = PathBuf::from(columns.next().unwrap());

        // Simulate the README's documented rollback line. (b itself moved
        // away as part of this same run, so /home/user/b/marker.txt no
        // longer exists on disk — that is the Move working as intended, not
        // this assertion's concern. What matters is that recreating the
        // link with the logged `old` reproduces the *string* the link
        // resolved to before the Move, not some other location that merely
        // shares a name.)
        fs.replace_symlink(&path, &old).unwrap();

        assert_eq!(
            fs.read_link(&path).unwrap(),
            PathBuf::from("/home/user/b/marker.txt"),
            "the rolled-back link must resolve exactly where the original did before the Move"
        );
    }

    /// The rename and the relink it enabled have both already landed by the
    /// time the log is written — an error writing the log itself must not
    /// discard that report, nor abort with a hard `Err`: it joins
    /// `relink_failures` like any other partial failure, and the run still
    /// returns `Ok`.
    #[test]
    fn relink_log_failure_is_reported_not_fatal() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");
        fs.add_dir(Path::new("/outside"));
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/home/user/old-project/target.txt"),
        );
        // The Relink Log's own directory refuses writes — the log can't be
        // written, even though the rename and the relink into `/outside/link`
        // already succeeded.
        fs.add_readonly_dir(Path::new("/home/user/.claude/backups/ccmv"));

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(
            report.relink_failures.len(),
            1,
            "{:?}",
            report.relink_failures
        );
        assert_ne!(report.exit_code(), 0);
        assert_eq!(
            fs.read_link(Path::new("/outside/link")).unwrap(),
            PathBuf::from("/home/user/new-project/target.txt"),
            "the relink itself must still have landed despite the log write failing"
        );
    }

    /// No candidates changed — the scan found nothing alive to repoint. An
    /// empty log file would read as a failed run, not a quiet one.
    #[test]
    fn relink_log_absent_when_nothing_changed() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_dir(Path::new("/outside"));

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        cmd.execute(&fs, claude_home).unwrap();

        let logs = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        assert!(logs.is_empty(), "{logs:?}");
    }

    /// With no backup archive to borrow a timestamp from, the log is still
    /// written — and, per the plan's "dann mit eigenem Zeitstempel", carries
    /// a timestamp of its own, in the same `{encoded}-{YYYYMMDD-HHMMSS}`
    /// shape `relink_log_shares_backup_timestamp` checks for the "has a
    /// backup" case.
    #[test]
    fn relink_log_written_with_no_backup() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");
        fs.add_dir(Path::new("/outside"));
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/home/user/old-project/target.txt"),
        );

        // relink_opts already sets no_backup: true — no archive is created,
        // so relink_log_path has nothing to borrow a timestamp from.
        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        cmd.execute(&fs, claude_home).unwrap();

        let logs = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        let log_path = logs
            .iter()
            .find(|p| p.to_string_lossy().ends_with(".relink.tsv"))
            .expect("relink log written even without a backup archive");

        let encoded = crate::encoder::encode(Path::new("/home/user/old-project")).unwrap();
        let file_name = log_path.file_name().unwrap().to_string_lossy();
        let timestamp = file_name
            .strip_prefix(&format!("{encoded}-"))
            .and_then(|rest| rest.strip_suffix(".relink.tsv"))
            .unwrap_or_else(|| {
                panic!("expected {{encoded}}-{{timestamp}}.relink.tsv, got {file_name}")
            });
        assert_eq!(
            timestamp.len(),
            "20260101-000000".len(),
            "expected a YYYYMMDD-HHMMSS timestamp of its own, got {file_name}"
        );
        assert!(
            timestamp.chars().all(|c| c.is_ascii_digit() || c == '-'),
            "expected a YYYYMMDD-HHMMSS timestamp of its own, got {file_name}"
        );
    }

    /// The Relink Log shares its `{encoded}-{timestamp}` name with the run's
    /// backup archive, so the two are visibly a pair without a manifest.
    ///
    /// Exercised directly against `relink_log_path` rather than through a
    /// full `Command::execute`: both the archive name and the log's
    /// "no backup" fallback name are derived from the same moving project,
    /// so an end-to-end comparison of the two filenames would pass even if
    /// the log stopped reusing the archive's name — nothing forces them
    /// apart. A crafted, unrelated backup filename does.
    #[test]
    fn relink_log_shares_backup_timestamp() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        let state = scanner::scan(&fs, Path::new("/home/user/old-project"), claude_home).unwrap();
        let unit = Unit::from_state(
            Path::new("/home/user/old-project"),
            Path::new("/home/user/new-project"),
            state,
        )
        .unwrap();
        let migration =
            Migration::from_units(&fs, vec![unit], claude_home, &relink_opts(Vec::new())).unwrap();

        let backup_path =
            PathBuf::from("/home/user/.claude/backups/ccmv/unrelated-name-20260101-000000.tar.gz");
        let log_path = migration.relink_log_path(&[backup_path]).unwrap();

        assert_eq!(
            log_path,
            PathBuf::from(
                "/home/user/.claude/backups/ccmv/unrelated-name-20260101-000000.relink.tsv"
            )
        );
    }

    #[test]
    fn dry_run_lists_candidates_without_writing() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");
        fs.add_dir(Path::new("/outside"));
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/home/user/old-project/target.txt"),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_dry_run_opts(vec![PathBuf::from("/outside")]),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(
            report.relink_candidates.len(),
            1,
            "{:?}",
            report.relink_candidates
        );
        assert_eq!(
            fs.read_link(Path::new("/outside/link")).unwrap(),
            PathBuf::from("/home/user/old-project/target.txt"),
            "no replace_symlink must have happened"
        );
        let logs = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        assert!(
            logs.iter()
                .all(|p| !p.to_string_lossy().ends_with(".relink.tsv")),
            "dry-run must not write a relink log, {logs:?}"
        );
    }

    #[test]
    fn dry_run_lists_self_link_under_old_path() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_dir(Path::new("/home/user/old-project/.claude/lib"));
        fs.add_file(
            Path::new("/home/user/old-project/.claude/lib/real-tool"),
            "content",
        );
        fs.add_symlink(
            Path::new("/home/user/old-project/.claude/bin/tool"),
            Path::new("/home/user/old-project/.claude/lib/real-tool"),
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_dry_run_opts(vec![PathBuf::from("/home/user/old-project")]),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(
            report.relink_candidates.len(),
            1,
            "{:?}",
            report.relink_candidates
        );
        assert_eq!(
            report.relink_candidates[0].old,
            PathBuf::from("/home/user/old-project/.claude/bin/tool"),
            "a candidate appears under its OLD path — nothing was moved"
        );
        assert_eq!(
            report.relink_candidates[0].new,
            PathBuf::from("/home/user/new-project/.claude/lib/real-tool")
        );
    }

    /// A second run over the same, already-relinked state must be a no-op —
    /// see the plan's "Idempotenz": a link that already points at the new
    /// target falls out of the prefix match and so is not a candidate the
    /// second time around.
    #[test]
    fn relink_second_run_reports_no_changes() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_file(Path::new("/home/user/old-project/target.txt"), "content");
        fs.add_dir(Path::new("/outside"));
        fs.add_symlink(
            Path::new("/outside/link"),
            Path::new("/home/user/old-project/target.txt"),
        );

        let first = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        first.execute(&fs, claude_home).unwrap();
        let logs_after_first = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        let log_path = logs_after_first
            .iter()
            .find(|p| p.to_string_lossy().ends_with(".relink.tsv"))
            .expect("the first run must have written a log")
            .clone();
        // `artifact_log_path` names the log from the source path and a
        // second-granularity timestamp, so a second run over the same move
        // lands on the very same file name: a plain "does a .relink.tsv
        // still exist, exactly one of them" count can't tell a quiet second
        // run apart from one that re-wrote this same file with identical
        // content. `write_count` on this exact path can: it only goes up
        // when `write_atomically` is actually called again.
        assert_eq!(
            fs.write_count(&log_path),
            1,
            "sanity: the first run must have written the log exactly once"
        );

        // Same invocation again: the old source path is gone, and
        // /outside/link already points at the new target.
        let second = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        let report = second.execute(&fs, claude_home).unwrap();

        assert!(
            report.relink_candidates.is_empty(),
            "{:?}",
            report.relink_candidates
        );
        assert!(
            report.relink_failures.is_empty(),
            "{:?}",
            report.relink_failures
        );
        assert_eq!(
            fs.read_link(Path::new("/outside/link")).unwrap(),
            PathBuf::from("/home/user/new-project/target.txt"),
            "the second run must not touch an already-relinked link"
        );
        assert_eq!(
            fs.write_count(&log_path),
            1,
            "the second run must not rewrite the relink log at all — a regression that \
             re-matches the link and repoints it at the identical target would still bump this, \
             even though the file's final contents look unchanged"
        );
    }

    /// Wiring test for `Migration::scan_relink_candidates` /
    /// `write_text_refs_log`: a Text Reference found during a real
    /// `--relink` run lands in a `.textrefs.tsv` file next to the Relink
    /// Log, sharing its `{encoded}-{timestamp}` name.
    #[test]
    fn text_refs_log_records_findings() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_dir(Path::new("/outside"));
        fs.add_file(
            Path::new("/outside/config.toml"),
            "path = \"/home/user/old-project/data\"",
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(report.text_reference_count, 1, "{report:?}");
        let logs = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        let log_path = logs
            .iter()
            .find(|p| p.to_string_lossy().ends_with(".textrefs.tsv"))
            .expect("text references report written");
        assert_eq!(
            Some(log_path.clone()),
            report.text_refs_report_path,
            "the report must name the file it actually wrote"
        );
        let content = fs.get_file(log_path).unwrap();
        assert_eq!(
            content,
            "/outside/config.toml\t1\t/home/user/old-project\t/home/user/new-project\n"
        );
    }

    /// No Text References were found — an empty report file would read as a
    /// failed run, the same rule `relink_log_absent_when_nothing_changed`
    /// checks for the Relink Log.
    #[test]
    fn text_refs_log_absent_when_nothing_found() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_dir(Path::new("/outside"));

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_opts(vec![PathBuf::from("/outside")]),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(report.text_reference_count, 0);
        assert_eq!(report.text_refs_report_path, None);
        let logs = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        assert!(
            logs.iter()
                .all(|p| !p.to_string_lossy().ends_with(".textrefs.tsv")),
            "{logs:?}"
        );
    }

    /// Without `--relink`, the text scan must not run at all — the same
    /// gate the symlink scan already has. A finding that a `--relink` run
    /// would have caught here must go completely unreported.
    #[test]
    fn text_scan_does_not_run_without_relink_flag() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_dir(Path::new("/outside"));
        fs.add_file(
            Path::new("/outside/config.toml"),
            "path = \"/home/user/old-project/data\"",
        );
        // Same Scan Root a --relink run would use — only `relink` itself is
        // off, so a leftover scan would still have somewhere to look.
        let mut opts = relink_opts(vec![PathBuf::from("/outside")]);
        opts.relink = false;

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts,
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(report.text_reference_count, 0);
        assert_eq!(report.text_refs_report_path, None);
        let logs = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        assert!(
            logs.iter()
                .all(|p| !p.to_string_lossy().ends_with(".textrefs.tsv")),
            "{logs:?}"
        );
    }

    /// Under `--dry-run` the report still counts what it found, in the
    /// style of the rest of the dry-run output, but writes nothing — see
    /// `dry_run_lists_candidates_without_writing` for the same rule applied
    /// to Relink Candidates.
    #[test]
    fn text_refs_dry_run_reports_count_without_writing() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");
        setup_project(&fs, "/home/user/old-project", "/home/user/.claude");
        fs.add_dir(Path::new("/outside"));
        fs.add_file(
            Path::new("/outside/config.toml"),
            "path = \"/home/user/old-project/data\"",
        );

        let cmd = Command::Move {
            source: PathBuf::from("/home/user/old-project"),
            target: PathBuf::from("/home/user/new-project"),
            opts: relink_dry_run_opts(vec![PathBuf::from("/outside")]),
        };
        let report = cmd.execute(&fs, claude_home).unwrap();

        assert_eq!(report.text_reference_count, 1, "{report:?}");
        assert_eq!(report.text_refs_report_path, None);
        // The count alone is not the report: under --dry-run there is no
        // file to point at, so the findings themselves must ride along on
        // the report, the same way relink_candidates does for symlinks.
        assert_eq!(
            report.text_reference_findings.len(),
            1,
            "{:?}",
            report.text_reference_findings
        );
        assert_eq!(
            report.text_reference_findings[0].file,
            PathBuf::from("/outside/config.toml")
        );
        assert_eq!(
            report.text_reference_findings[0].old,
            "/home/user/old-project"
        );
        assert_eq!(
            report.text_reference_findings[0].new,
            "/home/user/new-project"
        );
        let logs = fs
            .list_dir_recursive(&claude_home.join("backups/ccmv"))
            .unwrap();
        assert!(
            logs.iter()
                .all(|p| !p.to_string_lossy().ends_with(".textrefs.tsv")),
            "dry-run must not write a text references report, {logs:?}"
        );
    }

    fn text_reference(file: &str, line: usize) -> links::TextReference {
        links::TextReference {
            file: PathBuf::from(file),
            line,
            old: "/x/proj".to_owned(),
            new: "/y/proj".to_owned(),
        }
    }

    /// The scan that feeds `text_refs_report` runs its two files through a
    /// `par_iter`, so nothing hands them to the report in file order. If the
    /// report just echoed `findings` as given, b.toml's row would sit before
    /// a.toml's, and a.toml's own two rows would not be adjacent.
    #[test]
    fn report_groups_findings_by_file() {
        let findings = vec![
            text_reference("/outside/b.toml", 1),
            text_reference("/outside/a.toml", 5),
            text_reference("/outside/a.toml", 2),
        ];

        let report = text_refs_report(&findings);

        assert_eq!(
            report.lines().collect::<Vec<_>>(),
            vec![
                "/outside/a.toml\t2\t/x/proj\t/y/proj",
                "/outside/a.toml\t5\t/x/proj\t/y/proj",
                "/outside/b.toml\t1\t/x/proj\t/y/proj",
            ]
        );
    }

    /// The first column is the full absolute path, not a name relative to
    /// some root — a consumer greps for one project's rows by that prefix,
    /// and a shortened or relative path would make the filter miss them.
    #[test]
    fn report_is_filterable_by_project_prefix() {
        let findings = vec![
            text_reference("/home/user/project-a/notes.md", 1),
            text_reference("/home/user/project-b/notes.md", 1),
        ];

        let report = text_refs_report(&findings);
        let project_a_rows: Vec<&str> = report
            .lines()
            .filter(|line| line.starts_with("/home/user/project-a"))
            .collect();

        assert_eq!(
            project_a_rows,
            vec!["/home/user/project-a/notes.md\t1\t/x/proj\t/y/proj"]
        );
    }

    #[test]
    fn report_columns_are_file_line_old_new() {
        let findings = vec![text_reference("/outside/config.toml", 3)];

        let report = text_refs_report(&findings);
        let columns: Vec<&str> = report.trim_end().split('\t').collect();

        assert_eq!(
            columns,
            vec!["/outside/config.toml", "3", "/x/proj", "/y/proj"]
        );
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
            relink: false,
            scan_root: Vec::new(),
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
