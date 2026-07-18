// Batch plans: parse a move list, validate it before anything is written.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::fs::Fs;
use crate::scanner;

/// One batch-file line, before paths are resolved to absolute form.
#[derive(Debug)]
pub struct RawMove {
    pub source: String,
    pub target: String,
    pub line: usize,
}

pub struct Move {
    pub source: PathBuf,
    pub target: PathBuf,
    /// 1-based line in the batch file; `None` for moves given as arguments.
    pub line: Option<usize>,
}

impl Move {
    /// How to point at this move when a *different* move's message
    /// references it.
    fn locator(&self) -> String {
        match self.line {
            Some(n) => format!("line {n}"),
            None => self.source.display().to_string(),
        }
    }
}

pub struct BatchPlan {
    pub moves: Vec<Move>,
}

/// What the plan is checked against. Session-only moves the global session
/// data and leaves both project directories where they are, which changes
/// what counts as a violation.
pub struct PlanRules {
    pub force: bool,
    pub session_only: bool,
}

/// A rejected plan, carrying every problem found rather than the first.
/// With 120 lines, fixing them one error per run is unusable.
#[derive(Debug)]
pub struct PlanError {
    problems: Vec<String>,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.problems.len();
        let plural = if n == 1 { "problem" } else { "problems" };
        write!(f, "plan rejected, {n} {plural}:")?;
        for problem in &self.problems {
            write!(f, "\n  {problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for PlanError {}

struct Violation {
    line: Option<usize>,
    message: String,
}

impl Violation {
    fn new(line: Option<usize>, message: String) -> Self {
        Self { line, message }
    }
}

/// Parses `source<TAB>target` lines. Blank lines and `#` comments are
/// skipped; any other line must hold exactly two fields. A stray tab aborts
/// the whole batch instead of silently taking the first two fields — paths
/// may legally contain tabs, so guessing here would move the wrong thing.
pub fn parse(input: &str) -> Result<Vec<RawMove>> {
    let mut moves = Vec::new();

    for (index, raw) in input.lines().enumerate() {
        let line = index + 1;
        let text = raw.trim_end_matches('\r');
        if text.trim().is_empty() || text.trim_start().starts_with('#') {
            continue;
        }

        let mut fields = text.split('\t');
        let (Some(source), Some(target), None) = (fields.next(), fields.next(), fields.next())
        else {
            bail!("line {line}: expected exactly two tab-separated paths, got: {text}");
        };
        if source.is_empty() || target.is_empty() {
            bail!("line {line}: source and target must not be empty");
        }

        moves.push(RawMove {
            source: source.to_owned(),
            target: target.to_owned(),
            line,
        });
    }

    Ok(moves)
}

impl BatchPlan {
    /// Checks the whole plan before the first write and hands back the scanned
    /// source state of every move, in plan order, so the migration does not
    /// walk the same directories a second time.
    ///
    /// Chains, swaps and nested sources are rejected rather than
    /// topologically sorted: ordering them correctly buys little, and getting
    /// it wrong loses data silently.
    pub fn validate(
        &self,
        fs: &dyn Fs,
        claude_home: &Path,
        rules: &PlanRules,
    ) -> Result<Vec<scanner::ProjectState>> {
        let mut violations = Vec::new();
        self.check_shape(&mut violations);
        let source_states = self.check_paths(fs, claude_home, rules, &mut violations)?;

        if violations.is_empty() {
            return Ok(source_states);
        }

        // Argument-derived moves have no line and sort last, keeping their
        // relative order.
        violations.sort_by_key(|v| v.line.unwrap_or(usize::MAX));
        Err(PlanError {
            problems: violations
                .into_iter()
                .map(|v| match v.line {
                    Some(n) => format!("line {n}: {}", v.message),
                    None => v.message,
                })
                .collect(),
        }
        .into())
    }

    /// The rules that need no filesystem: a path used twice, a chain, two
    /// moves nesting one inside the other.
    fn check_shape(&self, violations: &mut Vec<Violation>) {
        let mut first_source: HashMap<&Path, usize> = HashMap::new();
        let mut first_target: HashMap<&Path, usize> = HashMap::new();

        for (index, mv) in self.moves.iter().enumerate() {
            match first_source.entry(mv.source.as_path()) {
                Entry::Occupied(seen) => violations.push(Violation::new(
                    mv.line,
                    format!(
                        "{} is a source twice (first at {})",
                        mv.source.display(),
                        self.moves[*seen.get()].locator()
                    ),
                )),
                Entry::Vacant(slot) => {
                    slot.insert(index);
                }
            }
            match first_target.entry(mv.target.as_path()) {
                Entry::Occupied(seen) => violations.push(Violation::new(
                    mv.line,
                    format!(
                        "two sources map to {} (also {})",
                        mv.target.display(),
                        self.moves[*seen.get()].locator()
                    ),
                )),
                Entry::Vacant(slot) => {
                    slot.insert(index);
                }
            }
        }

        for (index, mv) in self.moves.iter().enumerate() {
            let Some(&other) = first_source.get(mv.target.as_path()) else {
                continue;
            };
            if other != index {
                violations.push(Violation::new(
                    mv.line,
                    format!(
                        "{} is both a source and a target (chain with {})",
                        mv.target.display(),
                        self.moves[other].locator()
                    ),
                ));
            }
        }

        // Nesting between two moves, on either side. A target sitting inside
        // another move's source or target makes the result depend on which
        // move ran first — the same hazard as two nested sources, and the
        // reason the project directories can be moved in any order at all.
        for (index, outer) in self.moves.iter().enumerate() {
            for inner in &self.moves[index + 1..] {
                let crossing = [
                    (&outer.source, &inner.source),
                    (&outer.source, &inner.target),
                    (&outer.target, &inner.source),
                    (&outer.target, &inner.target),
                ];
                if let Some((a, b)) = crossing.into_iter().find(|(a, b)| nests(a, b)) {
                    violations.push(Violation::new(
                        inner.line.or(outer.line),
                        format!(
                            "{} and {} belong to different moves, one inside the other (nesting)",
                            a.display(),
                            b.display()
                        ),
                    ));
                }
            }
        }
    }

    /// The rules that have to look at the filesystem, plus the source states
    /// they scan on the way.
    fn check_paths(
        &self,
        fs: &dyn Fs,
        claude_home: &Path,
        rules: &PlanRules,
        violations: &mut Vec<Violation>,
    ) -> Result<Vec<scanner::ProjectState>> {
        let mut source_states = Vec::with_capacity(self.moves.len());
        for mv in &self.moves {
            let source_state = scanner::scan(fs, &mv.source, claude_home)?;
            if !source_state.has_claude_data() {
                violations.push(Violation::new(
                    mv.line,
                    scanner::missing_source_error(&mv.source),
                ));
            }
            if rules.session_only {
                // Only the global data moves, so a source without any has
                // nothing to contribute, and a target directory that does not
                // exist would be left holding orphaned sessions.
                if source_state.global_project_dir.is_none() {
                    violations.push(Violation::new(
                        mv.line,
                        format!(
                            "nothing to move: no global session data for {}",
                            mv.source.display()
                        ),
                    ));
                }
                if mv.source != mv.target && !fs.is_dir(&mv.target) {
                    violations.push(Violation::new(
                        mv.line,
                        format!(
                            "target path does not exist; sessions would be orphaned: {}",
                            mv.target.display()
                        ),
                    ));
                }
            }
            source_states.push(source_state);

            // Session-only merges into an existing target global on purpose,
            // so an occupied target is the normal case there.
            if !rules.force && !rules.session_only && mv.source != mv.target {
                let target_state = scanner::scan(fs, &mv.target, claude_home)?;
                if target_state.global_project_dir.is_some() {
                    violations.push(Violation::new(
                        mv.line,
                        scanner::occupied_target_error(&mv.target),
                    ));
                }
            }
        }

        Ok(source_states)
    }
}

/// True when one path contains the other. Comparison is component-wise, so
/// `/x/proj1` does not nest in `/x/proj11`.
fn nests(a: &Path, b: &Path) -> bool {
    a != b && (a.starts_with(b) || b.starts_with(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};

    use crate::encoder;
    use crate::fs::MockFs;

    const HOME: &str = "/home/u/.claude";

    fn mv(line: usize, source: &str, target: &str) -> Move {
        Move {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            line: Some(line),
        }
    }

    /// Makes `path` look like a real project to `scanner::scan`.
    fn with_project(fs: &MockFs, path: &str) {
        fs.add_dir(Path::new(path));
    }

    /// Makes `path` look like it already carries Claude data (rule 6).
    fn with_claude_data(fs: &MockFs, path: &str) {
        let encoded = encoder::encode(Path::new(path)).unwrap();
        fs.add_dir(&Path::new(HOME).join("projects").join(encoded));
    }

    fn validate(
        moves: Vec<Move>,
        fs: &MockFs,
        force: bool,
    ) -> anyhow::Result<Vec<scanner::ProjectState>> {
        BatchPlan { moves }.validate(
            fs,
            Path::new(HOME),
            &PlanRules {
                force,
                session_only: false,
            },
        )
    }

    fn validate_session_only(
        moves: Vec<Move>,
        fs: &MockFs,
    ) -> anyhow::Result<Vec<scanner::ProjectState>> {
        BatchPlan { moves }.validate(
            fs,
            Path::new(HOME),
            &PlanRules {
                force: false,
                session_only: true,
            },
        )
    }

    // --- Task 3.1: parser ---------------------------------------------

    #[test]
    fn parse_skips_comments_and_blanks() {
        let input = "# a comment\n\n/x/a\t/y/a\n\n  \n# another\n/x/b\t/y/b\n";
        let raw = parse(input).unwrap();

        assert_eq!(raw.len(), 2);
        assert_eq!(raw[0].source, "/x/a");
        assert_eq!(raw[0].target, "/y/a");
        assert_eq!(raw[0].line, 3);
        assert_eq!(raw[1].line, 7);
    }

    #[test]
    fn parse_rejects_line_with_extra_tab() {
        let input = "/x/a\t/y/a\n/x/b\t/y\tb\n";
        let err = parse(input).unwrap_err().to_string();

        assert!(err.contains("line 2"), "error must name the line: {err}");
    }

    #[test]
    fn parse_rejects_line_without_tab() {
        let err = parse("/x/a /y/a\n").unwrap_err().to_string();

        assert!(err.contains("line 1"), "error must name the line: {err}");
    }

    // --- Task 3.2: validation rules 1-6 -------------------------------

    #[test]
    fn rejects_duplicate_source() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");

        let err = validate(
            vec![mv(1, "/x/a", "/y/one"), mv(2, "/x/a", "/y/two")],
            &fs,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("source"), "{err}");
    }

    #[test]
    fn rejects_two_sources_one_target() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_project(&fs, "/x/b");

        let err = validate(
            vec![mv(1, "/x/a", "/y/one"), mv(2, "/x/b", "/y/one")],
            &fs,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("/y/one"), "{err}");
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn rejects_chain() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_project(&fs, "/x/b");

        let err = validate(
            vec![mv(1, "/x/a", "/x/b"), mv(2, "/x/b", "/x/c")],
            &fs,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("chain"), "{err}");
    }

    #[test]
    fn rejects_swap() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_project(&fs, "/x/b");

        let err = validate(
            vec![mv(1, "/x/a", "/x/b"), mv(2, "/x/b", "/x/a")],
            &fs,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("chain"), "swap is a chain: {err}");
    }

    #[test]
    fn rejects_nested_sources() {
        let fs = MockFs::new();
        with_project(&fs, "/x/rust");
        with_project(&fs, "/x/rust/concepts");

        let err = validate(
            vec![
                mv(1, "/x/rust", "/y/rust"),
                mv(2, "/x/rust/concepts", "/y/concepts"),
            ],
            &fs,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("nesting"), "{err}");
    }

    /// Neither a chain (no target equals a source) nor two nested sources,
    /// yet the outcome depends on the order: `/x/rust` is gone by the time
    /// the second move wants to land inside it.
    #[test]
    fn rejects_target_nested_in_another_source() {
        let fs = MockFs::new();
        with_project(&fs, "/x/rust");
        with_project(&fs, "/x/toolchain");

        let err = validate(
            vec![
                mv(1, "/x/rust", "/y/rust"),
                mv(2, "/x/toolchain", "/x/rust/toolchain"),
            ],
            &fs,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("nesting"), "{err}");
    }

    /// Two projects landing one inside the other is order-dependent the same
    /// way, and it is what lets the project directories move in any order.
    #[test]
    fn rejects_target_nested_in_another_target() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_project(&fs, "/x/b");

        let err = validate(
            vec![mv(1, "/x/a", "/y/dest"), mv(2, "/x/b", "/y/dest/sub")],
            &fs,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("nesting"), "{err}");
    }

    /// The everyday plan: many projects into one directory. Their targets
    /// share a parent but never contain one another, so the widened check
    /// must leave it alone.
    #[test]
    fn accepts_many_projects_into_one_directory() {
        let fs = MockFs::new();
        let moves = (0..5)
            .map(|i| {
                let source = format!("/x/proj{i}");
                with_project(&fs, &source);
                mv(i + 1, &source, &format!("/y/dest/proj{i}"))
            })
            .collect();

        validate(moves, &fs, false).unwrap();
    }

    #[test]
    fn rejects_missing_source() {
        let fs = MockFs::new();

        let err = validate(vec![mv(1, "/x/gone", "/y/gone")], &fs, false)
            .unwrap_err()
            .to_string();

        assert!(err.contains("/x/gone"), "{err}");
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn rejects_occupied_target_without_force() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_claude_data(&fs, "/y/a");

        let err = validate(vec![mv(1, "/x/a", "/y/a")], &fs, false)
            .unwrap_err()
            .to_string();

        assert!(err.contains("--force"), "{err}");
    }

    #[test]
    fn accepts_occupied_target_with_force() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_claude_data(&fs, "/y/a");

        validate(vec![mv(1, "/x/a", "/y/a")], &fs, true).unwrap();
    }

    #[test]
    fn accepts_valid_120_move_plan() {
        let fs = MockFs::new();
        let moves = (0..120)
            .map(|i| {
                let source = format!("/x/proj{i}");
                with_project(&fs, &source);
                mv(i + 1, &source, &format!("/y/proj{i}"))
            })
            .collect();

        validate(moves, &fs, false).unwrap();
    }

    /// The scanned states are the plan's second output: the migration builds
    /// its units from them instead of walking every source again.
    #[test]
    fn returns_the_scanned_source_states_in_plan_order() {
        let fs = MockFs::new();
        let moves = (0..3)
            .map(|i| {
                let source = format!("/x/proj{i}");
                with_project(&fs, &source);
                mv(i + 1, &source, &format!("/y/proj{i}"))
            })
            .collect();

        let states = validate(moves, &fs, false).unwrap();

        let paths: Vec<_> = states.iter().map(|s| s.project_path.clone()).collect();
        assert_eq!(
            paths,
            ["/x/proj0", "/x/proj1", "/x/proj2"].map(PathBuf::from)
        );
    }

    // --- session-only rules -------------------------------------------

    /// Merging into an existing target global is what session-only is for, so
    /// the rule that guards a full move must not fire here.
    #[test]
    fn session_only_accepts_occupied_target() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_claude_data(&fs, "/x/a");
        with_project(&fs, "/y/a");
        with_claude_data(&fs, "/y/a");

        validate_session_only(vec![mv(1, "/x/a", "/y/a")], &fs).unwrap();
    }

    #[test]
    fn session_only_rejects_source_without_global_data() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_project(&fs, "/y/a");

        let err = validate_session_only(vec![mv(1, "/x/a", "/y/a")], &fs)
            .unwrap_err()
            .to_string();

        assert!(err.contains("nothing to move"), "{err}");
    }

    #[test]
    fn session_only_rejects_missing_target_directory() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_claude_data(&fs, "/x/a");

        let err = validate_session_only(vec![mv(1, "/x/a", "/y/gone")], &fs)
            .unwrap_err()
            .to_string();

        assert!(err.contains("orphaned"), "{err}");
    }

    /// The point of routing session-only through the plan: both offending
    /// lines are named in one run.
    #[test]
    fn session_only_reports_all_violations_at_once() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_project(&fs, "/x/b");
        with_claude_data(&fs, "/x/b");
        with_project(&fs, "/y/a");

        let err = validate_session_only(vec![mv(1, "/x/a", "/y/a"), mv(2, "/x/b", "/y/gone")], &fs)
            .unwrap_err()
            .to_string();

        assert!(err.contains("2 problems"), "{err}");
    }

    // --- Task 3.3: report everything at once ---------------------------

    #[test]
    fn reports_all_violations_at_once() {
        let fs = MockFs::new();
        with_project(&fs, "/x/a");
        with_project(&fs, "/x/b");
        with_project(&fs, "/x/c");

        // line 2: chain on /x/b, line 3: collision on /y/one,
        // line 4: source does not exist.
        let err = validate(
            vec![
                mv(1, "/x/a", "/x/b"),
                mv(2, "/x/b", "/y/one"),
                mv(3, "/x/c", "/y/one"),
                mv(4, "/x/gone", "/y/gone"),
            ],
            &fs,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("chain"), "{err}");
        assert!(err.contains("/y/one"), "{err}");
        assert!(err.contains("/x/gone"), "{err}");
        assert!(err.contains("3 problems"), "{err}");
    }

    /// Moves from positional arguments carry no line number, so they must
    /// name themselves by path instead.
    #[test]
    fn moves_without_a_line_are_named_by_path() {
        let fs = MockFs::new();

        let err = validate(
            vec![Move {
                source: PathBuf::from("/x/gone"),
                target: PathBuf::from("/y/gone"),
                line: None,
            }],
            &fs,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("/x/gone"), "{err}");
        assert!(!err.contains("line"), "no line number to report: {err}");
    }

    #[test]
    fn violations_are_reported_in_line_order() {
        let fs = MockFs::new();

        let err = validate(
            vec![mv(7, "/x/gone7", "/y/a"), mv(2, "/x/gone2", "/y/b")],
            &fs,
            false,
        )
        .unwrap_err()
        .to_string();

        let at2 = err.find("line 2").unwrap();
        let at7 = err.find("line 7").unwrap();
        assert!(at2 < at7, "{err}");
    }
}
