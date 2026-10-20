//! `ecm`: audit and repair an Exchange mailbox after a botched migration.

#![forbid(unsafe_code)]

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};

mod audit_cmd;
mod cleanup_cmd;
mod content_cmd;
mod fidelity_cmd;
mod graph_cmd;
mod graph_plan_cmd;
mod notes_cmd;
mod pim_cmd;
mod plan_cmd;
mod pst_plan_cmd;
mod rules_cmd;
mod verify_cmd;
use ecm_model::FolderSummary;

/// Audit and repair an Exchange mailbox after a botched migration.
#[derive(Debug, Parser)]
#[command(name = "ecm", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check a source: open it, verify its checksum, show its folder tree and counts.
    #[command(subcommand)]
    Source(SourceCommand),
    /// Look at the target mailbox: folder tree and counts, items, calendar and
    /// inbox rules. Only reads.
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
    /// Re-create a few items in the `ecm-test` folder and compare them property
    /// by property with their reference. Only creates items, only in that folder.
    FidelityTest {
        #[command(flatten)]
        ews: EwsArgs,
        /// Work directory holding the inventories (to pick damaged items).
        #[arg(long, value_name = "DIR")]
        work: PathBuf,
        /// Backup directory: restore a few of its items with UploadItems.
        #[arg(long, value_name = "DIR")]
        backup: Option<PathBuf>,
        /// Also re-create damaged messages from Outlook.com (MIME through Graph).
        #[arg(long, requires = "client_id")]
        outlook_com: bool,
        /// Client id of the Microsoft Graph app registration.
        #[arg(long, value_name = "ID")]
        client_id: Option<String>,
        /// Sign-in authority for Graph.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
        /// Items per test.
        #[arg(long, default_value_t = 3)]
        count: usize,
    },
    /// Re-create a few PST messages in the `ecm-test` folder of an Outlook.com
    /// mailbox through Microsoft Graph, from MIME and from JSON, and compare each
    /// copy with a known good one and the PST's files. Only creates items, only in
    /// that folder; asks for `Mail.ReadWrite`.
    FidelityGraph {
        /// Client id of the Microsoft Graph app registration.
        #[arg(long, value_name = "ID")]
        client_id: String,
        /// Sign-in authority: `consumers` for Outlook.com.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
        /// PST file. It is never written to.
        #[arg(long, value_name = "FILE")]
        pst: PathBuf,
        /// A file of PST item keys, one per line: the messages to re-create.
        #[arg(long, value_name = "FILE")]
        keys: PathBuf,
        /// Mailbox written as `From` when a message has none (sent items), e.g.
        /// `Name <address>`.
        #[arg(long, value_name = "MAILBOX")]
        sender: Option<String>,
        /// EWS endpoint of an Exchange mailbox holding known good copies, found by
        /// Message-ID in `--work`'s `target.after.jsonl`, to compare with.
        #[arg(long, value_name = "URL", requires = "work")]
        ews: Option<String>,
        /// Credential name for `--ews`, `ecm:HOST` by default.
        #[arg(long, value_name = "NAME", requires = "ews")]
        credential: Option<String>,
        /// Work directory holding the Exchange inventory.
        #[arg(long, value_name = "DIR")]
        work: Option<PathBuf>,
    },
    /// Write a reviewable plan of repair actions (JSON Lines). Only reads.
    Plan {
        #[command(flatten)]
        ews: EwsArgs,
        /// Work directory holding the inventories.
        #[arg(long, value_name = "DIR")]
        work: PathBuf,
        /// Which source inventory to rebuild the target from.
        #[arg(long, value_enum, default_value = "outlook")]
        source: audit_cmd::SourceKind,
        /// First day real mail reached the target, `YYYY-MM-DD` (UTC).
        #[arg(long, value_name = "DATE")]
        cutoff: String,
        /// Plan file to write.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
        /// An applied plan: the migration copies it moved into folders are
        /// re-created from the source and quarantined. Repeat for several plans.
        #[arg(long, value_name = "PLAN")]
        after: Vec<PathBuf>,
    },
    /// Write a plan that copies the Outlook.com calendar, contacts and To Do tasks
    /// into the target's default folders, leaving out what they already hold.
    /// Only reads.
    PlanPim {
        #[command(flatten)]
        ews: EwsArgs,
        /// Client id of the Microsoft Graph app registration.
        #[arg(long, value_name = "ID")]
        client_id: String,
        /// Sign-in authority for Graph.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
        /// Plan file to write.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Write a plan that gives the target's contacts the photos their
    /// Outlook.com counterparts have (matched by e-mail address, then name).
    /// Only reads.
    PlanContactPhotos {
        #[command(flatten)]
        ews: EwsArgs,
        /// Client id of the Microsoft Graph app registration.
        #[arg(long, value_name = "ID")]
        client_id: String,
        /// Sign-in authority for Graph.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
        /// Plan file to write.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Write a plan that re-creates, from a PST, the messages the target has no
    /// healthy live copy of, rebuilt as MIME with their original headers. Uses
    /// the PST inventory (`source.jsonl`) and the target's `target.after.jsonl`.
    /// Only reads.
    PlanPst {
        /// EWS endpoint of the Exchange target (`target.after.jsonl`).
        #[arg(long, value_name = "URL", required_unless_present = "outlook_com")]
        ews: Option<String>,
        /// Credential name for `--ews`, `ecm:HOST` by default.
        #[arg(long, value_name = "NAME", requires = "ews")]
        credential: Option<String>,
        /// Plan for an Outlook.com mailbox through Microsoft Graph instead
        /// (`outlook.jsonl`), to run with `ecm apply-graph`. You sign in with a
        /// code to read its folders.
        #[arg(long, conflicts_with = "ews", requires = "client_id")]
        outlook_com: bool,
        /// Client id of the Microsoft Graph app registration.
        #[arg(long, value_name = "ID")]
        client_id: Option<String>,
        /// Sign-in authority: `consumers` for Outlook.com.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
        /// Work directory holding the inventories.
        #[arg(long, value_name = "DIR")]
        work: PathBuf,
        /// PST file. It is never written to.
        #[arg(long, value_name = "FILE")]
        pst: PathBuf,
        /// First day real mail reached the target, `YYYY-MM-DD` (UTC).
        #[arg(long, value_name = "DATE")]
        cutoff: String,
        /// Plan file to write.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
        /// An applied plan: the messages it re-created without their Message-ID
        /// are replaced by the PST's copy. Repeat for several plans.
        #[arg(long, value_name = "PLAN")]
        after: Vec<PathBuf>,
        /// Mailbox written as `From` when a message has an empty one (sent items),
        /// e.g. `Name <address>`; copies an earlier plan made without sender are
        /// then replaced.
        #[arg(long, value_name = "MAILBOX")]
        sender: Option<String>,
        /// Only replace copies earlier plans made; do not re-create messages the
        /// target lacks (the user may have deleted them since).
        #[arg(long)]
        replacements_only: bool,
        /// A file of PST item keys, one per line: consider only those messages.
        #[arg(long, value_name = "FILE")]
        keys: Option<PathBuf>,
        /// `FROM=TO`: put messages of a source folder (and its sub-folders) into
        /// another target folder, e.g. one renamed since. Repeat for each.
        #[arg(long = "rename", value_name = "FROM=TO")]
        renames: Vec<String>,
        /// Copies `ecm check-content` found damaged: each is rebuilt from the PST
        /// in the folder it is in, then quarantined, when a plan of `--after`
        /// created or moved it.
        #[arg(long, value_name = "FILE")]
        damaged: Option<PathBuf>,
    },
    /// Compare the target copies of PST messages, found by Message-ID, with the
    /// PST: emptied bodies, and each file attachment (missing, line ends changed
    /// from LF to CRLF, or otherwise different). Writes the damaged copies as CSV for
    /// `plan-pst --damaged`. Uses `source.jsonl` and `target.after.jsonl`; only
    /// reads both.
    CheckContent {
        /// EWS endpoint of the Exchange mailbox holding the copies
        /// (`target.after.jsonl`).
        #[arg(long, value_name = "URL", required_unless_present = "outlook_com")]
        ews: Option<String>,
        /// Credential name for `--ews`, `ecm:HOST` by default.
        #[arg(long, value_name = "NAME", requires = "ews")]
        credential: Option<String>,
        /// Read the copies in an Outlook.com mailbox through Microsoft Graph
        /// instead (`outlook.jsonl`). You sign in in your browser with a code.
        #[arg(long, conflicts_with = "ews", requires = "client_id")]
        outlook_com: bool,
        /// Client id of the app registration used to sign in to Microsoft Graph.
        #[arg(long, value_name = "ID")]
        client_id: Option<String>,
        /// Sign-in authority: `consumers` for Outlook.com.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
        /// Work directory holding the inventories.
        #[arg(long, value_name = "DIR")]
        work: PathBuf,
        /// PST file. It is never written to.
        #[arg(long, value_name = "FILE")]
        pst: PathBuf,
        /// A file of PST item keys, one per line: compare only those messages.
        #[arg(long, value_name = "FILE")]
        keys: Option<PathBuf>,
        /// CSV file to write.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Write a plan that copies the Outlook.com inbox rules into the target, with
    /// their folders mapped to the target's, leaving out rules it has by name.
    /// Only reads.
    PlanRules {
        #[command(flatten)]
        ews: EwsArgs,
        /// Re-create rules only classic Outlook ran (such as an alert on screen),
        /// which the server cannot run, as rules marking their messages as
        /// important.
        #[arg(long)]
        client_only_as_important: bool,
        /// Client id of the Microsoft Graph app registration.
        #[arg(long, value_name = "ID")]
        client_id: String,
        /// Sign-in authority for Graph.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
        /// Plan file to write.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Write a plan that copies the PST's sticky notes into the target's Notes
    /// folder, leaving out notes it has by subject. Only reads.
    PlanNotes {
        #[command(flatten)]
        ews: EwsArgs,
        /// Work directory holding the PST inventory (`source.jsonl`).
        #[arg(long, value_name = "DIR")]
        work: PathBuf,
        /// PST file. It is never written to.
        #[arg(long, value_name = "FILE")]
        pst: PathBuf,
        /// Plan file to write.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Write a plan moving the reviewed `ecm-unmatched` folder's items into a
    /// quarantine run folder. Only items an applied plan put there. Uses
    /// `target.after.jsonl` in the work directory. Only reads.
    PlanPark {
        #[command(flatten)]
        ews: EwsArgs,
        /// Work directory holding `target.after.jsonl`.
        #[arg(long, value_name = "DIR")]
        work: PathBuf,
        /// The applied plans whose moves put items in `ecm-unmatched`.
        #[arg(long, value_name = "PLAN", required = true)]
        after: Vec<PathBuf>,
        /// Plan file to write.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Sort the messages of an Outlook.com mailbox into those to keep, those safe
    /// to delete because the target has them, and those to look at, into
    /// `outlook-cleanup.csv`. Only reads: the deletion is up to you, in
    /// Outlook.com.
    ReportCleanup {
        /// Client id of the Microsoft Graph app registration.
        #[arg(long, value_name = "ID")]
        client_id: String,
        /// Sign-in authority for Graph.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
        /// Work directory holding the target's `target.after.jsonl`.
        #[arg(long, value_name = "DIR")]
        work: PathBuf,
        /// Domain that moved to the target, e.g. `example.com`.
        #[arg(long, value_name = "DOMAIN")]
        domain: String,
        /// An address the Outlook.com mailbox keeps; repeat for each.
        #[arg(long = "keep", value_name = "ADDRESS", required = true)]
        keep: Vec<String>,
        /// A sender domain whose mail went to the moved domain though it does not
        /// show it (e.g. `github.com` notifications); repeat for each.
        #[arg(long = "moved-sender", value_name = "DOMAIN")]
        moved_senders: Vec<String>,
    },
    /// Run a reviewed plan, journaled and resumable. Creates folders, items, inbox
    /// rules and contact photos, and moves items (and quarantine folders into
    /// Deleted Items); nothing is deleted.
    Apply {
        #[command(flatten)]
        ews: EwsArgs,
        /// Plan file written by `ecm plan`.
        #[arg(long, value_name = "FILE")]
        plan: PathBuf,
        /// Re-create items from Outlook.com (needed when the plan has
        /// `recreate` steps).
        #[arg(long, requires = "client_id")]
        outlook_com: bool,
        /// Client id of the Microsoft Graph app registration.
        #[arg(long, value_name = "ID")]
        client_id: Option<String>,
        /// Sign-in authority for Graph.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
        /// Run at most this many pending steps, then stop.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Only say what is left to do.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a plan written for an Outlook.com mailbox (`plan-pst --outlook-com`)
    /// through Microsoft Graph, journaled and resumable like `apply`. Asks for
    /// `Mail.ReadWrite`.
    ApplyGraph {
        /// Client id of the Microsoft Graph app registration.
        #[arg(long, value_name = "ID")]
        client_id: String,
        /// Sign-in authority: `consumers` for Outlook.com.
        #[arg(long, value_name = "TENANT", default_value = "consumers")]
        tenant: String,
        /// Plan file. Repeat to run several plans, in order, with one sign-in.
        #[arg(long = "plan", value_name = "FILE", required = true)]
        plans: Vec<PathBuf>,
        /// Run at most this many pending steps, then stop.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Only say what is left to do.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inventory the target again after `apply` and check it against the source:
    /// every source message must have a healthy copy in its folder, and nothing
    /// set aside may be the only copy. Only reads.
    Verify {
        #[command(flatten)]
        ews: EwsArgs,
        /// Work directory holding the source inventory. The new target inventory
        /// goes there as `target.after.jsonl`.
        #[arg(long, value_name = "DIR")]
        work: PathBuf,
        /// Which source inventory to check against.
        #[arg(long, value_enum, default_value = "outlook")]
        source: audit_cmd::SourceKind,
        /// First day real mail reached the target, `YYYY-MM-DD` (UTC).
        #[arg(long, value_name = "DATE")]
        cutoff: String,
        /// Reuse `target.after.jsonl` instead of reading the mailbox again.
        #[arg(long)]
        reuse_inventory: bool,
    },
    /// Write a plan moving quarantine run folders into Deleted Items, for you to
    /// empty when you are sure. Nothing is deleted by `ecm`. Only reads.
    PlanDiscard {
        #[command(flatten)]
        ews: EwsArgs,
        /// Run folder names under `ecm-quarantine` (e.g. `2026-01-01-12-00`);
        /// every run when left out.
        #[arg(long = "run", value_name = "NAME")]
        runs: Vec<String>,
        /// Plan file to write.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
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
    /// Show PST messages by inventory key: sender, dates, bodies, attachments.
    /// Only reads.
    Show {
        /// PST file to read. It is never written to.
        #[arg(long, value_name = "FILE")]
        pst: PathBuf,
        /// Item keys, as in the PST inventory's `key`.
        #[arg(long = "key", value_name = "KEY", required = true)]
        keys: Vec<String>,
        /// Also show each file attachment's size, first bytes and SHA-256, and
        /// whether its content looks like its type.
        #[arg(long)]
        attachments: bool,
        /// With `--attachments`, also save the files into this directory, to open
        /// them locally.
        #[arg(long, value_name = "DIR", requires = "attachments")]
        save: Option<PathBuf>,
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
    /// Show the properties of items by id: body type and length, attachments,
    /// dates, sender. Only reads: the items are not marked as read.
    Show {
        #[command(flatten)]
        ews: EwsArgs,
        /// Item ids, as in an inventory's `key`.
        #[arg(long = "id", value_name = "ID", required = true)]
        ids: Vec<String>,
        /// Also read each file attachment and show its size, first bytes and
        /// SHA-256, and whether its content looks like its type.
        #[arg(long)]
        attachments: bool,
        /// With `--attachments`, also save the files into this directory, to open
        /// them locally.
        #[arg(long, value_name = "DIR", requires = "attachments")]
        save: Option<PathBuf>,
    },
    /// List the default calendar (start in UTC, UID, subject) and the default
    /// contacts. Only reads.
    Pim {
        #[command(flatten)]
        ews: EwsArgs,
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
        Command::Source(SourceCommand::Show {
            pst,
            keys,
            attachments,
            save,
        }) => report(content_cmd::source_show(
            &pst,
            &keys,
            attachments,
            save.as_deref(),
        )),
        Command::CheckContent {
            ews,
            credential,
            outlook_com: _,
            client_id,
            tenant,
            work,
            pst,
            keys,
            out,
        } => {
            let ews = ews.map(|ews| EwsArgs { ews, credential });
            let graph = client_id.map(|client_id| audit_cmd::GraphArgs { client_id, tenant });
            let copies = match (&ews, &graph) {
                (Some(ews), _) => content_cmd::CopiesArgs::Ews(ews),
                (None, Some(graph)) => content_cmd::CopiesArgs::Graph(graph),
                (None, None) => unreachable!("clap requires --ews or --outlook-com"),
            };
            report(content_cmd::check_content_command(
                &copies,
                &work,
                &pst,
                keys.as_deref(),
                &out,
            ))
        }
        Command::FidelityGraph {
            client_id,
            tenant,
            pst,
            keys,
            sender,
            ews,
            credential,
            work,
        } => {
            let ews = ews.map(|ews| EwsArgs { ews, credential });
            let reference = ews.as_ref().zip(work.as_deref());
            report(graph_cmd::fidelity_graph(
                &audit_cmd::GraphArgs { client_id, tenant },
                &pst,
                &keys,
                sender.as_deref(),
                reference,
            ))
        }
        Command::Target(TargetCommand::Check { ews, all_folders }) => {
            report(target_check(&ews, all_folders))
        }
        Command::Target(TargetCommand::Show {
            ews,
            ids,
            attachments,
            save,
        }) => report(target_show(&ews, &ids, attachments, save.as_deref())),
        Command::Target(TargetCommand::Pim { ews }) => report(target_pim(&ews)),
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
        Command::FidelityTest {
            ews,
            work,
            backup,
            outlook_com,
            client_id,
            tenant,
            count,
        } => report(fidelity_cmd::fidelity_test(
            &ews,
            &work,
            backup.as_deref(),
            client_id
                .filter(|_| outlook_com)
                .map(|client_id| audit_cmd::GraphArgs { client_id, tenant })
                .as_ref(),
            count,
        )),
        Command::Plan {
            ews,
            work,
            source,
            cutoff,
            out,
            after,
        } => report(plan_cmd::plan_command(
            &work, source, &cutoff, &ews, &after, &out,
        )),
        Command::Apply {
            ews,
            plan,
            outlook_com,
            client_id,
            tenant,
            limit,
            dry_run,
        } => report(plan_cmd::apply_command(
            &plan,
            &ews,
            client_id
                .filter(|_| outlook_com)
                .map(|client_id| audit_cmd::GraphArgs { client_id, tenant })
                .as_ref(),
            limit,
            dry_run,
        )),
        Command::ApplyGraph {
            client_id,
            tenant,
            plans,
            limit,
            dry_run,
        } => report(graph_plan_cmd::apply_graph_command(
            &plans,
            &audit_cmd::GraphArgs { client_id, tenant },
            limit,
            dry_run,
        )),
        Command::PlanNotes {
            ews,
            work,
            pst,
            out,
        } => report(notes_cmd::plan_notes_command(&ews, &work, &pst, &out)),
        Command::PlanPim {
            ews,
            client_id,
            tenant,
            out,
        } => report(pim_cmd::plan_pim_command(
            &ews,
            &audit_cmd::GraphArgs { client_id, tenant },
            &out,
        )),
        Command::PlanContactPhotos {
            ews,
            client_id,
            tenant,
            out,
        } => report(pim_cmd::plan_contact_photos_command(
            &ews,
            &audit_cmd::GraphArgs { client_id, tenant },
            &out,
        )),
        Command::PlanPst {
            ews,
            credential,
            outlook_com: _,
            client_id,
            tenant,
            work,
            pst,
            cutoff,
            out,
            after,
            sender,
            replacements_only,
            keys,
            renames,
            damaged,
        } => report({
            let ews = ews.map(|ews| EwsArgs { ews, credential });
            let graph = client_id.map(|client_id| audit_cmd::GraphArgs { client_id, tenant });
            let target = match (&ews, &graph) {
                (Some(ews), _) => pst_plan_cmd::PstTarget::Ews(ews),
                (None, Some(graph)) => pst_plan_cmd::PstTarget::Graph(graph),
                (None, None) => unreachable!("clap requires --ews or --outlook-com"),
            };
            pst_plan_cmd::plan_pst_command(
                &target,
                &work,
                &pst,
                &cutoff,
                &pst_plan_cmd::Replacements {
                    after: &after,
                    sender: sender.as_deref(),
                    only: replacements_only,
                    damaged: damaged.as_deref(),
                },
                &pst_plan_cmd::Selection {
                    keys: keys.as_deref(),
                    renames: &renames,
                },
                &out,
            )
        }),
        Command::PlanRules {
            ews,
            client_only_as_important,
            client_id,
            tenant,
            out,
        } => report(rules_cmd::plan_rules_command(
            &ews,
            &audit_cmd::GraphArgs { client_id, tenant },
            client_only_as_important,
            &out,
        )),
        Command::PlanPark {
            ews,
            work,
            after,
            out,
        } => report(plan_cmd::park_command(&ews, &work, &after, &out)),
        Command::ReportCleanup {
            client_id,
            tenant,
            work,
            domain,
            keep,
            moved_senders,
        } => report(cleanup_cmd::report_cleanup_command(
            &audit_cmd::GraphArgs { client_id, tenant },
            &work,
            &keep,
            &domain,
            &moved_senders,
        )),
        Command::Verify {
            ews,
            work,
            source,
            cutoff,
            reuse_inventory,
        } => report(verify_cmd::verify_command(
            &work,
            source,
            &cutoff,
            &ews,
            reuse_inventory,
        )),
        Command::PlanDiscard { ews, runs, out } => {
            report(plan_cmd::discard_command(&ews, &runs, &out))
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

fn target_pim(ews: &EwsArgs) -> Result<(), String> {
    let client = connect(ews)?;
    let calendar = client
        .get_distinguished_folder("calendar")
        .map_err(|e| e.to_string())?;
    let mut events = client
        .calendar_briefs(&calendar.id)
        .map_err(|e| e.to_string())?;
    events.sort_by_key(|e| e.start);
    println!("Calendar: {} items", events.len());
    for event in &events {
        println!(
            "  {}  {}  {}",
            event
                .start
                .map(ecm_model::item::format_utc)
                .unwrap_or_default(),
            event.uid.as_deref().unwrap_or("-"),
            event.subject.as_deref().unwrap_or_default()
        );
    }
    let contacts = client
        .get_distinguished_folder("contacts")
        .map_err(|e| e.to_string())?;
    let contacts = client
        .contact_briefs(&contacts.id)
        .map_err(|e| e.to_string())?;
    println!(
        "Contacts: {} items, {} with a photo",
        contacts.len(),
        contacts.iter().filter(|c| c.has_picture).count()
    );
    let tree = client
        .folder_tree("msgfolderroot")
        .map_err(|e| e.to_string())?;
    let paths: std::collections::HashMap<String, String> = tree
        .with_paths()
        .into_iter()
        .map(|(path, folder)| (folder.id.clone(), path))
        .collect();
    let folder = |id: &str| -> String {
        paths
            .get(id)
            .cloned()
            .unwrap_or_else(|| match client.get_folder(id) {
                Ok(folder) => paths
                    .get(&folder.id)
                    .cloned()
                    .unwrap_or(format!("{} (id differs from the folder tree)", folder.name)),
                Err(error) => format!("UNKNOWN FOLDER ({error})"),
            })
    };
    let rules = client.inbox_rules().map_err(|e| e.to_string())?;
    println!("Inbox rules: {}", rules.len());
    for rule in rules {
        let mut notes = Vec::new();
        if let Some(id) = &rule.move_to {
            notes.push(format!("move to {}", folder(id)));
        }
        if let Some(id) = &rule.copy_to {
            notes.push(format!("copy to {}", folder(id)));
        }
        if rule.stops {
            notes.push("stop".into());
        }
        if !rule.enabled {
            notes.push("DISABLED".into());
        }
        if rule.in_error {
            notes.push("IN ERROR".into());
        }
        if rule.not_supported {
            notes.push("NOT SUPPORTED".into());
        }
        println!(
            "  {:>2}. {}: {}",
            rule.priority.unwrap_or_default(),
            rule.name,
            notes.join(", ")
        );
    }
    Ok(())
}

fn target_show(
    ews: &EwsArgs,
    ids: &[String],
    attachments: bool,
    save: Option<&std::path::Path>,
) -> Result<(), String> {
    let client = connect(ews)?;
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let items = client.item_properties(&ids).map_err(|e| e.to_string())?;
    for (id, item) in ids.iter().zip(items) {
        println!("{id}");
        match item {
            Ok(properties) => {
                for (name, value) in properties {
                    let value: String = value.chars().take(120).collect();
                    println!("  {name:<24} {value}");
                }
            }
            Err(error) => println!("  error: {error}"),
        }
        if attachments {
            for file in client.attachments(id).map_err(|e| e.to_string())? {
                content_cmd::print_file(&file.name, &file.content_type, &file.content, save)?;
                if file.inline || file.content_id.is_some() {
                    println!(
                        "       inline {}  content-id {}",
                        file.inline,
                        file.content_id.as_deref().unwrap_or("-")
                    );
                }
            }
        }
    }
    Ok(())
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

    /// Every `ecm <command>` the supported migrations page names exists: a command
    /// renamed or removed fails here until the page follows.
    #[test]
    fn the_supported_migrations_page_names_real_commands() {
        let page = include_str!("../../../docs/supported-migrations.md");
        let cli = Cli::command();
        let mut named = 0;
        for (index, _) in page.match_indices("`ecm ") {
            let rest = &page[index + 5..];
            let words: Vec<&str> = rest
                .split('`')
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .collect();
            let Some(command) = words.first() else {
                continue;
            };
            let found = cli.find_subcommand(command);
            assert!(
                found.is_some(),
                "the page names `ecm {command}`, which does not exist"
            );
            for flag in words.iter().skip(1).filter(|w| w.starts_with("--")) {
                let name = flag.trim_start_matches('-');
                assert!(
                    found
                        .unwrap()
                        .get_arguments()
                        .any(|arg| arg.get_long() == Some(name)),
                    "the page names `ecm {command} {flag}`, which does not exist"
                );
            }
            named += 1;
        }
        assert!(named > 10, "the page names only {named} commands");
    }
}
