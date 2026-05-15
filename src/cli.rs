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

    /// Source project directory
    pub source: Option<PathBuf>,
    /// Target directory or path
    pub target: Option<PathBuf>,

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

    #[test]
    fn parse_session_only_flag_sets_field() {
        let cli = Cli::parse_from(["cc-mv", "--session-only", "/a", "/b"]);
        assert!(cli.session_only);
        assert_eq!(cli.source.as_deref(), Some(std::path::Path::new("/a")));
        assert_eq!(cli.target.as_deref(), Some(std::path::Path::new("/b")));
    }

    #[test]
    fn parse_without_session_only_flag_defaults_false() {
        let cli = Cli::parse_from(["cc-mv", "/a", "/b"]);
        assert!(!cli.session_only);
    }

    #[test]
    fn parse_session_only_combines_with_dry_run() {
        let cli = Cli::parse_from(["cc-mv", "-n", "--session-only", "/a", "/b"]);
        assert!(cli.dry_run);
        assert!(cli.session_only);
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
