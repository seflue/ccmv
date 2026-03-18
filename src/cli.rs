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
