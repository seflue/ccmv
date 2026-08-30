// CLI: clap-based command line interface

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};

#[allow(clippy::struct_excessive_bools)]
#[derive(Parser)]
#[command(
    name = "ccmv",
    about = "Move and rename Claude Code projects",
    version,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Source projects, then the target. With more than two paths the target
    /// must be an existing directory, like `mv`.
    pub paths: Vec<PathBuf>,

    /// Read moves from a tab-separated file, one `SOURCE<TAB>TARGET` per line;
    /// `-` reads standard input
    #[arg(long, value_name = "FILE", conflicts_with = "paths")]
    pub batch: Option<PathBuf>,

    /// Show what would change without modifying anything
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Show detailed output
    #[arg(short, long)]
    pub verbose: bool,
    /// Overwrite existing Claude data at target
    #[arg(long)]
    pub force: bool,
    /// Skip automatic backup before migration
    #[arg(long)]
    pub no_backup: bool,
    /// Move only Claude Code session data, leave project directories untouched
    #[arg(long)]
    pub session_only: bool,
    /// Repoint symlinks that point into the moved project (see CONTEXT.md's
    /// Relink). Conflicts with `--session-only`: relinking needs both the old
    /// and the new path from the same run, and a project already moved by
    /// hand needs Repair instead, which ccmv does not build yet (ccmv-0019).
    ///
    /// Not marked `conflicts_with`: clap's own conflict message says neither
    /// of those things, so the inherent parse methods below check it by hand
    /// and raise the same `ArgumentConflict` with a message that does.
    #[arg(long)]
    pub relink: bool,
    /// Additional directory to scan for Relink Candidates, on top of every
    /// project's Local Claude Directory. Repeatable; each occurrence adds a
    /// directory rather than replacing the default set.
    #[arg(long, value_name = "DIR")]
    pub scan_root: Vec<PathBuf>,
}

impl Cli {
    /// Shadows `clap::Parser::try_parse_from`: an inherent method of the same
    /// name takes priority over the trait's when called as `Cli::method(..)`,
    /// which is how `main.rs` and every test in this module call it. Needed
    /// because `--relink` / `--session-only` is no longer a `conflicts_with`
    /// clap can catch on its own — see the doc comment on `relink` above.
    ///
    /// All three of clap's entry points are shadowed rather than only the two
    /// this crate calls today: one left unshadowed is a way into a `Cli` with
    /// both flags set, and it would look like ordinary clap usage.
    pub fn try_parse_from<I, T>(itr: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let cli = <Self as Parser>::try_parse_from(itr)?;
        if cli.relink && cli.session_only {
            return Err(Self::relink_session_only_conflict());
        }
        Ok(cli)
    }

    /// Shadows `clap::Parser::parse_from`, exiting on a bad argument list the
    /// way the trait method does. Only the tests call it; it exists so that
    /// the one entry point this crate does not otherwise use cannot quietly
    /// hand back a `Cli` the checks above would have rejected.
    #[cfg(test)]
    pub fn parse_from<I, T>(itr: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        Self::try_parse_from(itr).unwrap_or_else(|e| e.exit())
    }

    /// Shadows `clap::Parser::parse` the same way, so a real invocation
    /// (`main.rs`) gets the same message as `try_parse_from`.
    pub fn parse() -> Self {
        Self::try_parse_from(std::env::args_os()).unwrap_or_else(|e| e.exit())
    }

    /// A human-first message: relinking needs both the old and the new path
    /// from the same run, and a project already moved by hand needs Repair,
    /// which ccmv does not build yet — ccmv-0019 second, for anyone who wants
    /// to go look.
    fn relink_session_only_conflict() -> clap::Error {
        <Self as CommandFactory>::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            "--relink cannot be combined with --session-only: relinking needs both the old and \
             the new path from the same run, and a project you already moved by hand needs \
             Repair, which ccmv does not build yet (ccmv-0019)",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(cli: &Cli) -> Vec<&str> {
        cli.paths.iter().map(|p| p.to_str().unwrap()).collect()
    }

    #[test]
    fn parse_session_only_flag_sets_field() {
        let cli = Cli::parse_from(["ccmv", "--session-only", "/a", "/b"]);
        assert!(cli.session_only);
        assert_eq!(paths(&cli), ["/a", "/b"]);
    }

    #[test]
    fn parse_without_session_only_flag_defaults_false() {
        let cli = Cli::parse_from(["ccmv", "/a", "/b"]);
        assert!(!cli.session_only);
    }

    #[test]
    fn parse_session_only_combines_with_dry_run() {
        let cli = Cli::parse_from(["ccmv", "-n", "--session-only", "/a", "/b"]);
        assert!(cli.dry_run);
        assert!(cli.session_only);
    }

    #[test]
    fn parse_three_paths_last_is_target() {
        let cli = Cli::parse_from(["ccmv", "/a", "/b", "/dst"]);
        assert_eq!(paths(&cli), ["/a", "/b", "/dst"]);
    }

    #[test]
    fn batch_flag_conflicts_with_positional_paths() {
        let Err(err) = Cli::try_parse_from(["ccmv", "--batch", "moves.tsv", "/a", "/b"]) else {
            panic!("expected a conflict")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn batch_dash_means_stdin() {
        let cli = Cli::parse_from(["ccmv", "--batch", "-"]);
        assert_eq!(cli.batch.as_deref(), Some(std::path::Path::new("-")));
    }

    #[test]
    fn relink_flag_defaults_to_off() {
        let cli = Cli::parse_from(["ccmv", "/a", "/b"]);
        assert!(!cli.relink);
    }

    #[test]
    fn scan_root_accepts_repeated_flag() {
        let cli = Cli::parse_from([
            "ccmv",
            "--scan-root",
            "/work/vendor/cef",
            "--scan-root",
            "/other/root",
            "/a",
            "/b",
        ]);
        assert_eq!(
            cli.scan_root,
            vec![
                PathBuf::from("/work/vendor/cef"),
                PathBuf::from("/other/root"),
            ]
        );
    }

    #[test]
    fn relink_conflicts_with_session_only() {
        let Err(err) = Cli::try_parse_from(["ccmv", "--relink", "--session-only", "/a", "/b"])
        else {
            panic!("expected a conflict")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let message = err.to_string().to_lowercase();
        assert!(message.contains("repair"), "{message}");
        assert!(message.contains("ccmv-0019"), "{message}");
    }
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a backup of Claude Code references
    Backup {
        /// Project directory to back up
        path: PathBuf,
    },
    /// Restore Claude Code references from a backup
    Restore {
        /// Path to the backup .tar.gz file
        backup_file: PathBuf,
    },
}
