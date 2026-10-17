//! `ecm`: audit and repair an Exchange mailbox after a botched migration.

#![forbid(unsafe_code)]

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use ecm_model::FolderSummary;

/// Audit and repair an Exchange mailbox after a botched migration.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check a source: open it, verify its checksum, show its folder tree and counts.
    #[command(subcommand)]
    Source(SourceCommand),
    /// Check a target: authenticate, show its folder tree and counts.
    #[command(subcommand)]
    Target(CheckCommand),
    /// Export the whole target mailbox before any change.
    Backup,
    /// Read the source and the target into the local database.
    Inventory,
    /// Match items and report what is OK, degraded, missing or duplicated.
    Audit,
    /// Write a reviewable plan of repair actions.
    Plan,
    /// Run a reviewed plan, journaled and resumable.
    Apply,
    /// Inventory again and compare, after `apply`.
    Verify,
    /// Manage the quarantine folder.
    #[command(subcommand)]
    Quarantine(QuarantineCommand),
}

#[derive(Debug, Subcommand)]
enum SourceCommand {
    /// Open the source read-only, show its folder tree and counts, and verify its
    /// SHA-256 did not change.
    Check {
        /// PST file to read. It is never written to.
        #[arg(long, value_name = "FILE")]
        pst: PathBuf,
        /// Show every folder from the store's root, not only the IPM subtree.
        #[arg(long)]
        all_folders: bool,
    },
}

#[derive(Debug, Subcommand)]
enum CheckCommand {
    /// Run the check.
    Check,
}

#[derive(Debug, Subcommand)]
enum QuarantineCommand {
    /// Permanently delete the items quarantined by a run.
    Purge,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Source(SourceCommand::Check { pst, all_folders }) => {
            match ecm_pst::check(&pst, all_folders) {
                Ok(report) => {
                    print_source_report(&report);
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("ecm: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        command => {
            eprintln!("ecm: {command:?} is not implemented yet");
            ExitCode::from(2)
        }
    }
}

fn print_source_report(report: &ecm_pst::CheckReport) {
    println!("Source   {}", report.path.display());
    println!(
        "Format   {} PST, {}",
        report.format,
        format_size(report.size)
    );
    if let Some(name) = &report.display_name {
        println!("Store    {name}");
    }
    println!("SHA-256  {}", report.sha256);
    println!();
    print_tree(&report.folders);
    println!();
    println!(
        "{} folders, {} items. The file is unchanged: same SHA-256 before and after.",
        report.folders.folder_count(),
        report.folders.total_item_count()
    );
}

/// A size in bytes, in the largest decimal unit that keeps it at 1 or more.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

/// Prints a folder tree, one folder per line, indented by depth, with its counts
/// right-aligned.
fn print_tree(tree: &FolderSummary) {
    let mut lines = Vec::new();
    tree.walk(&mut |folder, depth| {
        let name = if folder.name.is_empty() && depth == 0 {
            "(root)".to_owned()
        } else {
            folder.name.clone()
        };
        lines.push((format!("{}{name}", "  ".repeat(depth)), folder));
    });
    let width = lines
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    println!(
        "{:width$}  {:>8}  {:>7}  {:>6}  Class",
        "Folder", "Items", "Unread", "Assoc."
    );
    for (label, folder) in lines {
        println!(
            "{label:width$}  {:>8}  {:>7}  {:>6}  {}",
            folder.item_count,
            folder.unread_count,
            folder.associated_count,
            folder.class.as_deref().unwrap_or("-"),
        );
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::{Cli, format_size};

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn sizes_use_decimal_units() {
        assert_eq!(format_size(999), "999 bytes");
        assert_eq!(format_size(271_360), "271.36 KB");
        assert_eq!(format_size(5_060_000_000), "5.06 GB");
    }
}
