//! `ecm`: audit and repair an Exchange mailbox after a botched migration.

#![forbid(unsafe_code)]

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};

mod audit_cmd;
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
    Target(TargetCommand),
    /// Export the whole target mailbox before any change. Only reads the mailbox.
    /// Run it again into the same directory to resume.
    Backup {
        #[command(flatten)]
        ews: EwsArgs,
        /// Backup directory, created if needed.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
    },
    /// Read the source and the target, item by item, into inventory files. Only
    /// reads both.
    Inventory {
        /// Work directory for inventories and reports. It holds mailbox metadata:
        /// keep it private (e.g. the gitignored `reports/`).
        #[arg(long, value_name = "DIR")]
        work: PathBuf,
        /// PST file to inventory as the source. It is never written to.
        #[arg(long, value_name = "FILE")]
        pst: Option<PathBuf>,
        /// EWS endpoint of the target mailbox to inventory.
        #[arg(long, value_name = "URL")]
        ews: Option<String>,
        /// Credential name for `--ews`, `ecm:HOST` by default.
        #[arg(long, value_name = "NAME", requires = "ews")]
        credential: Option<String>,
        /// Also inventory an Outlook.com mailbox through Microsoft Graph, as a
        /// second source (`outlook.jsonl`). You sign in in your browser with a code;
        /// only read permissions are asked for.
        #[arg(long, requires = "client_id")]
        outlook_com: bool,
        /// Client id of the app registration used to sign in to Microsoft Graph
        /// (personal accounts, public client flows, delegated `Mail.Read`).
        #[arg(long, value_name = "ID")]
        client_id: Option<String>,
        /// Sign-in authority: `consumers` for Outlook.com, `organizations` or a
        /// tenant id for Microsoft 365.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
    },
    /// Match the inventories and report what is OK, misplaced, degraded, missing or
    /// duplicated, as a summary and CSV files in the work directory.
    Audit {
        /// Work directory holding the inventories.
        #[arg(long, value_name = "DIR")]
        work: PathBuf,
        /// Migration date, `YYYY-MM-DD` (UTC). Target items without a source match
        /// received from that day on are new mail, kept as they are.
        #[arg(long, value_name = "DATE")]
        cutoff: String,
        /// Which source inventory to audit the target against.
        #[arg(long, value_enum, default_value = "pst")]
        source: audit_cmd::SourceKind,
    },
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
enum TargetCommand {
    /// Authenticate and show the mailbox's folder tree, counts and sizes. Only
    /// reads the mailbox.
    Check {
        #[command(flatten)]
        ews: EwsArgs,
        /// Show every folder from the mailbox root, system folders included, not
        /// only those a mail client shows.
        #[arg(long)]
        all_folders: bool,
    },
}

/// How to reach an Exchange mailbox.
#[derive(Debug, clap::Args)]
struct EwsArgs {
    /// EWS endpoint, e.g. `https://mail.example.com/EWS/Exchange.asmx`
    #[arg(long, value_name = "URL")]
    ews: String,
    /// Generic credential in the Windows Credential Manager holding the mailbox
    /// user and password. Defaults to `ecm:HOST`, created with
    /// `cmdkey /generic:ecm:HOST /user:ADDRESS /pass`.
    #[arg(long, value_name = "NAME")]
    credential: Option<String>,
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
        Command::Target(TargetCommand::Check { ews, all_folders }) => {
            report(target_check(&ews, all_folders))
        }
        Command::Backup { ews, out } => report(backup(&ews, out)),
        Command::Inventory {
            work,
            pst,
            ews,
            credential,
            outlook_com,
            client_id,
            tenant,
        } => report(audit_cmd::inventory(
            &work,
            pst.as_deref(),
            ews.map(|ews| EwsArgs { ews, credential }).as_ref(),
            client_id
                .filter(|_| outlook_com)
                .map(|client_id| audit_cmd::GraphArgs { client_id, tenant })
                .as_ref(),
        )),
        Command::Audit {
            work,
            cutoff,
            source,
        } => report(audit_cmd::audit(&work, &cutoff, source)),
        command => {
            eprintln!("ecm: {command:?} is not implemented yet");
            ExitCode::from(2)
        }
    }
}

/// Exit code for a command's outcome, printing its error if any.
fn report(outcome: Result<(), String>) -> ExitCode {
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ecm: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn connect(args: &EwsArgs) -> Result<ecm_ews::EwsClient<ecm_http::HttpClient>, String> {
    let url = ecm_http::HttpsUrl::parse(&args.ews).map_err(|e| e.to_string())?;
    let name = args
        .credential
        .clone()
        .unwrap_or_else(|| format!("ecm:{}", url.host));
    let credential = ecm_http::read_generic_credential(&name).map_err(|e| e.to_string())?;
    let client = ecm_http::HttpClient::new(url, credential).map_err(|e| e.to_string())?;
    Ok(ecm_ews::EwsClient::new(client))
}

#[cfg(not(windows))]
fn connect(_: &EwsArgs) -> Result<ecm_ews::EwsClient<NoTransport>, String> {
    Err("Exchange access needs Windows for now (WinHTTP and the Credential Manager)".into())
}

#[cfg(not(windows))]
struct NoTransport;

#[cfg(not(windows))]
impl ecm_ews::Transport for NoTransport {
    fn post_xml(&self, _: &[u8]) -> ecm_ews::Result<(u16, Vec<u8>)> {
        unreachable!()
    }
    fn user(&self) -> &str {
        unreachable!()
    }
}

fn target_check(args: &EwsArgs, all_folders: bool) -> Result<(), String> {
    let client = connect(args)?;
    let root = if all_folders { "root" } else { "msgfolderroot" };
    let tree = client.folder_tree(root).map_err(|e| e.to_string())?;
    let summary = tree.summary();
    println!("Target   {}", args.ews);
    println!("User     {}", client.user());
    println!();
    print_tree(&summary);
    println!();
    println!(
        "{} folders, {} items, {}. Nothing was changed.",
        summary.folder_count(),
        summary.total_item_count(),
        format_size(summary.total_size())
    );
    Ok(())
}

fn backup(args: &EwsArgs, out: PathBuf) -> Result<(), String> {
    use ecm_ews::backup::{BackupEvent, BackupOptions};
    use std::io::Write as _;

    let client = connect(args)?;
    let options = BackupOptions::new(&out);
    let mut on_event = |event: BackupEvent<'_>| match event {
        BackupEvent::Started {
            folders,
            items,
            bytes,
            already_saved,
        } => {
            println!(
                "Backing up {folders} folders, {items} items ({}) into {}",
                format_size(bytes),
                out.display()
            );
            if already_saved > 0 {
                println!("{already_saved} items are already saved and will be skipped.");
            }
        }
        BackupEvent::Folder {
            path,
            items,
            already_saved,
        } => {
            let name = if path.is_empty() { "(root)" } else { path };
            eprint!("\r\x1b[K");
            println!("  {name}: {items} items, {already_saved} already saved");
        }
        BackupEvent::Saved { items, bytes } => {
            eprint!("\r\x1b[K    saved {items} items ({})", format_size(bytes));
            let _ = std::io::stderr().flush();
        }
        BackupEvent::Failed { path, code } => {
            eprint!("\r\x1b[K");
            println!("  {path}: an item could not be exported ({code})");
        }
    };
    let report = client
        .backup(&options, &mut on_event)
        .map_err(|e| e.to_string())?;
    eprint!("\r\x1b[K");
    println!();
    println!(
        "{} items saved ({}), {} already saved, {} failed. The mailbox was not changed.",
        report.saved,
        format_size(report.saved_bytes),
        report.already_saved,
        report.failed
    );
    if report.failed > 0 {
        println!(
            "Failed items are listed in {}; run the backup again to retry them.",
            out.join("errors.jsonl").display()
        );
    }
    Ok(())
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
        "{:width$}  {:>8}  {:>7}  {:>6}  {:>10}  Class",
        "Folder", "Items", "Unread", "Assoc.", "Size"
    );
    for (label, folder) in lines {
        println!(
            "{label:width$}  {:>8}  {:>7}  {:>6}  {:>10}  {}",
            folder.item_count,
            folder.unread_count,
            folder.associated_count,
            folder.size.map_or_else(|| "-".to_owned(), format_size),
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
