// Batch plans: parse a move list, validate it before anything is written.

// Wired into the CLI in phase 4; until then only the tests below reach it.
#![allow(dead_code)]

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
    /// Checks the whole plan before the first write.
    ///
    /// Chains, swaps and nesting are rejected rather than topologically
    /// sorted: ordering them correctly buys little, and getting it wrong
    /// loses data silently.
    pub fn validate(&self, fs: &dyn Fs, claude_home: &Path, force: bool) -> Result<()> {
        let mut violations = Vec::new();
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

        for (index, outer) in self.moves.iter().enumerate() {
            for inner in &self.moves[index + 1..] {
                if nests(&outer.source, &inner.source) {
                    violations.push(Violation::new(
                        inner.line.or(outer.line),
                        format!(
                            "{} and {} are both sources (nesting)",
                            outer.source.display(),
                            inner.source.display()
                        ),
                    ));
                }
            }
        }

        for mv in &self.moves {
            let source_state = scanner::scan(fs, &mv.source, claude_home)?;
            if !source_state.has_claude_data() {
                violations.push(Violation::new(
                    mv.line,
                    format!(
                        "source not found: {} does not exist and has no Claude Code project data",
                        mv.source.display()
                    ),
                ));
            }

            if !force && mv.source != mv.target {
                let target_state = scanner::scan(fs, &mv.target, claude_home)?;
                if target_state.global_project_dir.is_some() {
                    violations.push(Violation::new(
                        mv.line,
                        format!(
                            "conflict: target {} already has Claude Code project data; use --force to overwrite",
                            mv.target.display()
                        ),
                    ));
                }
            }
        }

        if violations.is_empty() {
            return Ok(());
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

    fn validate(moves: Vec<Move>, fs: &MockFs, force: bool) -> anyhow::Result<()> {
        BatchPlan { moves }.validate(fs, Path::new(HOME), force)
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
