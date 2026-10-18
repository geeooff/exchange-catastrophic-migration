//! `ecm inventory` and `ecm audit`.

use std::{collections::BTreeMap, path::Path};

use ecm_audit::{Audit, Defect, SourceStatus, TargetRole};
use ecm_ews::folders::FolderKind;
use ecm_model::{
    ItemRecord,
    item::{format_utc, parse_utc},
};
use ecm_store::{SOURCE_INVENTORY, TARGET_INVENTORY, read_inventory, write_csv, write_inventory};

use crate::{EwsArgs, connect};

/// File name of the Outlook.com inventory in a work directory.
pub const OUTLOOK_INVENTORY: &str = "outlook.jsonl";

/// Which source inventory an audit uses.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SourceKind {
    /// The PST inventory (`source.jsonl`).
    Pst,
    /// The Outlook.com inventory (`outlook.jsonl`).
    Outlook,
}

/// How to sign in to Microsoft Graph.
#[derive(Debug)]
pub struct GraphArgs {
    /// App registration client id.
    pub client_id: String,
    /// Sign-in authority.
    pub tenant: String,
}

/// Inventories the PST, the target and/or Outlook.com into `work`.
pub fn inventory(
    work: &Path,
    pst: Option<&Path>,
    ews: Option<&EwsArgs>,
    graph: Option<&GraphArgs>,
) -> Result<(), String> {
    if pst.is_none() && ews.is_none() && graph.is_none() {
        return Err("nothing to inventory: give --pst, --ews and/or --outlook-com".into());
    }
    if let Some(graph) = graph {
        inventory_graph(work, graph)?;
    }
    if let Some(pst) = pst {
        let before = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
        let (items, errors) = {
            let source = ecm_pst::PstSource::open(pst).map_err(|e| e.to_string())?;
            source
                .items(&mut |path, count| {
                    eprintln!("  source {}: {count} items", display(path));
                })
                .map_err(|e| e.to_string())?
        };
        let after = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
        if before != after {
            return Err(format!("{} changed during the inventory", pst.display()));
        }
        for error in &errors {
            eprintln!("  source: {error}");
        }
        let path = work.join(SOURCE_INVENTORY);
        write_inventory(&path, &items).map_err(|e| e.to_string())?;
        println!(
            "Source: {} items into {} ({} unreadable). The PST is unchanged.",
            items.len(),
            path.display(),
            errors.len()
        );
    }
    if let Some(ews) = ews {
        let items = inventory_target(&connect(ews)?)?;
        let path = work.join(TARGET_INVENTORY);
        write_inventory(&path, &items).map_err(|e| e.to_string())?;
        println!(
            "Target: {} items into {}. The mailbox was not changed.",
            items.len(),
            path.display()
        );
    }
    Ok(())
}

/// Reads every item of the target mailbox, folder by folder. Only reads.
pub fn inventory_target<T: ecm_ews::Transport>(
    client: &ecm_ews::EwsClient<T>,
) -> Result<Vec<ItemRecord>, String> {
    let tree = client
        .folder_tree("msgfolderroot")
        .map_err(|e| e.to_string())?;
    let mut items = Vec::new();
    for (path, folder) in tree.with_paths() {
        if folder.kind == FolderKind::Search || folder.total_count == 0 {
            continue;
        }
        let found = client
            .inventory_items(&folder.id, &path)
            .map_err(|e| e.to_string())?;
        eprintln!("  target {}: {} items", display(&path), found.len());
        items.extend(found);
    }
    Ok(items)
}

#[cfg(windows)]
fn inventory_graph(work: &Path, args: &GraphArgs) -> Result<(), String> {
    let transport = ecm_graph::WinHttpTransport::default();
    let token =
        ecm_graph::auth::sign_in(&transport, &args.client_id, &args.tenant, &mut |prompt| {
            eprintln!();
            eprintln!(
                "Sign in to Outlook.com: open {} and enter the code {}",
                prompt.verification_uri, prompt.user_code
            );
            eprintln!("(Waiting for you to sign in…)");
        })
        .map_err(|e| e.to_string())?;
    let client = ecm_graph::GraphClient::new(transport, token);
    println!(
        "Signed in as {}.",
        client.mailbox().map_err(|e| e.to_string())?
    );
    let tree = client.mail_folders().map_err(|e| e.to_string())?;
    let mut items = Vec::new();
    for folder in tree.flatten().into_iter().skip(1) {
        if folder.total == 0 {
            continue;
        }
        let found = client
            .inventory_messages(folder)
            .map_err(|e| e.to_string())?;
        eprintln!("  outlook.com {}: {} items", folder.path, found.len());
        items.extend(found);
    }
    let path = work.join(OUTLOOK_INVENTORY);
    write_inventory(&path, &items).map_err(|e| e.to_string())?;
    println!(
        "Outlook.com: {} messages into {}. The mailbox was not changed.",
        items.len(),
        path.display()
    );
    Ok(())
}

#[cfg(not(windows))]
fn inventory_graph(_: &Path, _: &GraphArgs) -> Result<(), String> {
    Err("Microsoft Graph access needs Windows for now (WinHTTP)".into())
}

pub(crate) fn display(path: &str) -> &str {
    if path.is_empty() { "(top)" } else { path }
}

/// Audits the inventories in `work` and writes the reports there.
pub fn audit(work: &Path, cutoff: &str, kind: SourceKind) -> Result<(), String> {
    let cutoff = parse_utc(&format!("{cutoff}T00:00:00Z"))
        .ok_or_else(|| format!("invalid --cutoff {cutoff:?}: expected YYYY-MM-DD"))?;
    let source_file = match kind {
        SourceKind::Pst => SOURCE_INVENTORY,
        SourceKind::Outlook => OUTLOOK_INVENTORY,
    };
    let source = read_inventory(&work.join(source_file)).map_err(|e| e.to_string())?;
    let target = read_inventory(&work.join(TARGET_INVENTORY)).map_err(|e| e.to_string())?;
    let audit = ecm_audit::audit(&source, &target, cutoff);

    print_summary(&audit, &source, &target, cutoff);
    write_reports(work, &audit, &source, &target)?;
    println!();
    println!(
        "Details: {} and {}.",
        work.join("audit-source.csv").display(),
        work.join("audit-target.csv").display()
    );
    Ok(())
}

const STATUSES: [SourceStatus; 4] = [
    SourceStatus::Ok,
    SourceStatus::Misplaced,
    SourceStatus::Degraded,
    SourceStatus::Missing,
];

fn print_summary(audit: &Audit, source: &[ItemRecord], target: &[ItemRecord], cutoff: i64) {
    let counts = audit.source_counts();
    let count = |status| counts.get(&status).copied().unwrap_or(0);
    let roles =
        |want: fn(&TargetRole) -> bool| audit.target_roles.iter().filter(|r| want(r)).count();

    println!(
        "Source: {} items. Target: {} items.",
        source.len(),
        target.len()
    );
    println!();
    println!("Source items in the target");
    println!(
        "  ok          {:>7}  a healthy copy in the right folder",
        count(SourceStatus::Ok)
    );
    println!(
        "  misplaced   {:>7}  a healthy copy in another folder: move it",
        count(SourceStatus::Misplaced)
    );
    println!(
        "  degraded    {:>7}  damaged copies only: replace from the source",
        count(SourceStatus::Degraded)
    );
    println!(
        "  missing     {:>7}  no copy: upload from the source",
        count(SourceStatus::Missing)
    );
    println!();
    println!("Target items");
    println!(
        "  copies      {:>7}",
        roles(|r| matches!(r, TargetRole::Copy { .. }))
    );
    println!(
        "  duplicates  {:>7}  extra copies: quarantine them",
        audit.duplicates()
    );
    println!(
        "  new         {:>7}  no source item, received since {}: keep",
        roles(|r| *r == TargetRole::New),
        &format_utc(cutoff)[..10]
    );
    println!(
        "  unmatched   {:>7}  no source item, older: review",
        roles(|r| *r == TargetRole::Unmatched)
    );

    let defects = audit.defect_counts();
    if !defects.is_empty() {
        println!();
        println!("Defects of the chosen copies");
        let mut defects: Vec<_> = defects.into_iter().collect();
        defects.sort();
        for (defect, n) in defects {
            println!("  {:<20} {n:>7}", defect.name());
        }
    }

    // Per source folder.
    let mut folders: BTreeMap<&str, [usize; 4]> = BTreeMap::new();
    for (item, status) in source.iter().zip(&audit.source_status) {
        let index = STATUSES.iter().position(|s| s == status).unwrap_or(0);
        folders.entry(item.folder.as_str()).or_default()[index] += 1;
    }
    let width = folders
        .keys()
        .map(|f| display(f).chars().count())
        .max()
        .unwrap_or(6)
        .max(6);
    println!();
    println!(
        "{:width$}  {:>7}  {:>7}  {:>9}  {:>8}  {:>7}",
        "Source folder", "items", "ok", "misplaced", "degraded", "missing"
    );
    for (folder, n) in folders {
        println!(
            "{:width$}  {:>7}  {:>7}  {:>9}  {:>8}  {:>7}",
            display(folder),
            n.iter().sum::<usize>(),
            n[0],
            n[1],
            n[2],
            n[3]
        );
    }
}

fn write_reports(
    work: &Path,
    audit: &Audit,
    source: &[ItemRecord],
    target: &[ItemRecord],
) -> Result<(), String> {
    let time = |t: Option<i64>| t.map(format_utc).unwrap_or_default();
    let text = |t: &Option<String>| t.clone().unwrap_or_default();

    let rows: Vec<Vec<String>> = source
        .iter()
        .zip(&audit.source_status)
        .zip(&audit.source_copies)
        .map(|((item, status), copies)| {
            vec![
                status.name().into(),
                item.folder.clone(),
                item.class.clone(),
                text(&item.subject),
                text(&item.sender),
                time(item.sent),
                time(item.received),
                copies.to_string(),
                text(&item.message_id),
                item.key.clone(),
            ]
        })
        .collect();
    write_csv(
        &work.join("audit-source.csv"),
        &[
            "status",
            "folder",
            "class",
            "subject",
            "sender",
            "sent",
            "received",
            "copies",
            "message_id",
            "key",
        ],
        &rows,
    )
    .map_err(|e| e.to_string())?;

    let rows: Vec<Vec<String>> = target
        .iter()
        .zip(&audit.target_roles)
        .map(|(item, role)| {
            let (name, source_folder, defects) = match role {
                TargetRole::Copy { source: s, defects } => (
                    "copy",
                    source[*s].folder.clone(),
                    defects
                        .iter()
                        .map(|d: &Defect| d.name())
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                TargetRole::Duplicate { source: s } => {
                    ("duplicate", source[*s].folder.clone(), String::new())
                }
                TargetRole::New => ("new", String::new(), String::new()),
                TargetRole::Unmatched => ("unmatched", String::new(), String::new()),
            };
            vec![
                name.into(),
                defects,
                item.folder.clone(),
                source_folder,
                item.class.clone(),
                text(&item.subject),
                text(&item.sender),
                time(item.sent),
                time(item.received),
                time(item.created),
                text(&item.message_id),
                item.key.clone(),
            ]
        })
        .collect();
    write_csv(
        &work.join("audit-target.csv"),
        &[
            "role",
            "defects",
            "folder",
            "source_folder",
            "class",
            "subject",
            "sender",
            "sent",
            "received",
            "created",
            "message_id",
            "key",
        ],
        &rows,
    )
    .map_err(|e| e.to_string())
}
