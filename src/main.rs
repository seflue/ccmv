use std::process;

use anyhow::Result;

mod backup;
mod batch;
mod cli;
mod encoder;
mod fs;
mod links;
mod migration;
mod scanner;
mod updater;

fn main() {
    match run() {
        Ok(exit_code) => process::exit(exit_code),
        Err(e) => {
            eprintln!("Error: {e:#}");
            process::exit(1);
        }
    }
}

/// The move itself is done by the time this returns `Ok` — a non-zero code
/// here reports an unresolved Relink Candidate, not a failed run. See
/// `MigrationReport::exit_code`.
fn run() -> Result<i32> {
    let cli = cli::Cli::parse();
    let fs = fs::RealFs;
    let claude_home = dirs::home_dir()
        .expect("could not determine home directory")
        .join(".claude");

    let cmd = migration::Command::from_cli(&cli)?;
    let report = cmd.execute(&fs, &claude_home)?;
    print_report(&report, cli.verbose);
    Ok(report.exit_code())
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

    if !report.relink_candidates.is_empty() {
        println!("Relink candidates: {}", report.relink_candidates.len());
        print_truncated(&report.relink_candidates, verbose, |c| {
            println!("  {} -> {}", c.old.display(), c.new.display());
        });
    }

    // These writes landed in projects other than the one that moved, which no
    // backup covers. Saying nothing would leave the user unaware that anything
    // outside the moved project changed, and unaware of the one file that
    // records it — so the count and the log's path are named, not just the
    // failures below.
    if report.relinked_count > 0 {
        match &report.relink_log_path {
            Some(path) => println!(
                "Relinked: {} (see {})",
                report.relinked_count,
                path.display()
            ),
            None => println!("Relinked: {}", report.relinked_count),
        }
    }

    if !report.relink_failures.is_empty() {
        println!("Relink failures: {}", report.relink_failures.len());
        print_truncated(&report.relink_failures, verbose, |f| {
            println!("  {}: {}", f.path.display(), f.error);
        });
    }

    // On a real run the findings stay in the TSV report — see
    // src/migration.rs's Migration::write_text_refs_log — and this is a
    // pointer to it, not a second copy of its contents. Under `--dry-run`
    // nothing is written, so `text_reference_findings` carries the rows
    // themselves instead, the same way `relink_candidates` does for
    // symlinks.
    if report.text_reference_count > 0 {
        match &report.text_refs_report_path {
            Some(path) => println!(
                "Text references: {} (see {})",
                report.text_reference_count,
                path.display()
            ),
            None => println!("Text references: {}", report.text_reference_count),
        }
        print_truncated(&report.text_reference_findings, verbose, |f| {
            println!("  {}:{} {} -> {}", f.file.display(), f.line, f.old, f.new);
        });
    }
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
