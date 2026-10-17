//! `ecm`: audit and repair an Exchange mailbox after a botched migration.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    Source(CheckCommand),
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
    eprintln!("ecm: {:?} is not implemented yet", cli.command);
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
