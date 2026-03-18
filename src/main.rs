use std::process;

use anyhow::Result;
use clap::Parser;

mod backup;
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
    print_report(&report);
    Ok(())
}

fn print_report(report: &migration::MigrationReport) {
    if report.nothing_to_do {
        println!("Nothing to do — project references are already consistent.");
        return;
    }

    if report.dry_run {
        println!("Dry run — no changes made.");
        println!();
    }

    println!("Action: {}", report.action);

    if let (Some(src), Some(tgt)) = (&report.source, &report.target) {
        println!("  {} -> {}", src.display(), tgt.display());
    }
    if let Some((old_dir, new_dir)) = &report.global_dir_rename {
        println!("  {} -> {}", old_dir.display(), new_dir.display());
    }

    if let Some(ref backup) = report.backup_path {
        println!("Backup: {}", backup.display());
    }

    let updated: Vec<_> = report.files_updated.iter().filter(|r| !r.skipped).collect();
    if !updated.is_empty() {
        println!("Files updated: {}", updated.len());
        for r in &updated {
            println!("  {} ({} replacements)", r.file.display(), r.replacements);
        }
    }

    let total_replacements: usize = updated.iter().map(|r| r.replacements).sum();
    println!("Total path replacements: {total_replacements}");
}
