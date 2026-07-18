// CLI: clap-based command line interface

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
