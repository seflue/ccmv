use std::process;

use anyhow::Result;
use clap::Parser;

mod backup;
mod batch;
mod cli;
mod encoder;
mod fs;
mod migration;
mod scanner;
mod updater;

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:#}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = cli::Cli::parse();
    let fs = fs::RealFs;
    let claude_home = dirs::home_dir()
        .expect("could not determine home directory")
        .join(".claude");

    let cmd = migration::Command::from_cli(&cli)?;
    let report = cmd.execute(&fs, &claude_home)?;
    print_report(&report, cli.verbose);
    Ok(())
}

/// How many entries any one list shows before it is cut short. Enough to see
/// a small plan whole, few enough that a 120-move batch stays readable.
const LIST_SHOWN: usize = 10;

fn print_report(report: &migration::MigrationReport, verbose: bool) {
    if report.nothing_to_do {
        println!("Nothing to do — project references are already consistent.");
        return;
    }

    if report.dry_run {
        println!("Dry run — no changes made.");
        println!();
    }

    match report.moves.len() {
        0 | 1 => println!("Action: {}", report.action),
        n => println!("Action: {} {n} projects", report.action),
    }

    print_moves(&report.moves, verbose);

    for backup in &report.backup_paths {
        println!("Backup: {}", backup.display());
    }

    let updated: Vec<_> = report.files_updated.iter().filter(|r| !r.skipped).collect();
    if !updated.is_empty() {
        println!("Files updated: {}", updated.len());
        print_truncated(&updated, verbose, |r| {
            println!("  {} ({} replacements)", r.file.display(), r.replacements);
        });
    }

    let total_replacements: usize = updated.iter().map(|r| r.replacements).sum();
    println!("Total path replacements: {total_replacements}");
}

/// One line per move, plus the global session directory it took with it.
///
/// The list is printed once the run is over rather than as each move lands:
/// the moves run in parallel, so streaming them would interleave.
fn print_moves(moves: &[migration::MoveReport], verbose: bool) {
    print_truncated(moves, verbose, |mv| {
        println!("  {} -> {}", mv.source.display(), mv.target.display());
        if let Some((old_dir, new_dir)) = &mv.global_dir_rename {
            println!("    {} -> {}", old_dir.display(), new_dir.display());
        }
    });
}

/// Prints a list, cut short unless `--verbose` asks for all of it. A 120-move
/// batch touches thousands of files, and an unabridged dump of either list
/// buries the summary above it.
fn print_truncated<T>(items: &[T], verbose: bool, print: impl Fn(&T)) {
    let limit = if verbose { usize::MAX } else { LIST_SHOWN };
    for item in items.iter().take(limit) {
        print(item);
    }
    if items.len() > limit {
        println!("  ... ({} more, use -v for all)", items.len() - limit);
    }
}
