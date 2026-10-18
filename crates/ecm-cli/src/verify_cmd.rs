//! `ecm verify`: inventory the target again after `apply`, and audit it.

use std::path::Path;

use ecm_audit::{
    Defect, TargetRole,
    verify::{Verification, in_scope, verify},
};
use ecm_model::{
    ItemRecord,
    item::{format_utc, parse_utc},
};
use ecm_store::{SOURCE_INVENTORY, read_inventory, write_csv, write_inventory};

use crate::{
    EwsArgs,
    audit_cmd::{OUTLOOK_INVENTORY, SourceKind, display, inventory_target},
    connect,
    plan_cmd::folder_map,
};

/// File name of the target inventory taken by `verify`, next to the one the plan
/// was made from.
pub const VERIFIED_INVENTORY: &str = "target.after.jsonl";

/// Inventories the target into `work`, then checks it against the source
/// inventory. Only reads.
pub fn verify_command(
    work: &Path,
    kind: SourceKind,
    cutoff: &str,
    ews: &EwsArgs,
    reuse: bool,
) -> Result<(), String> {
    let cutoff = parse_utc(&format!("{cutoff}T00:00:00Z"))
        .ok_or_else(|| format!("invalid --cutoff {cutoff:?}: expected YYYY-MM-DD"))?;
    let source_file = match kind {
        SourceKind::Pst => SOURCE_INVENTORY,
        SourceKind::Outlook => OUTLOOK_INVENTORY,
    };
    let mut source = read_inventory(&work.join(source_file)).map_err(|e| e.to_string())?;

    let client = connect(ews)?;
    let (folders, _) = folder_map(&client)?;
    let inventory = work.join(VERIFIED_INVENTORY);
    let target = if reuse {
        read_inventory(&inventory).map_err(|e| e.to_string())?
    } else {
        let items = inventory_target(&client)?;
        write_inventory(&inventory, &items).map_err(|e| e.to_string())?;
        items
    };
    for item in &mut source {
        item.folder = folders.target(&item.folder);
    }

    let result = verify(&source, &target, cutoff);
    print_summary(&result, &source, &target);
    write_reports(work, &result, &source, &target)?;
    println!();
    println!(
        "Details: {} and {}. The mailbox was not changed.",
        work.join("verify-source.csv").display(),
        work.join("verify-target.csv").display()
    );
    Ok(())
}

fn print_summary(result: &Verification, source: &[ItemRecord], target: &[ItemRecord]) {
    let [ok, misplaced, degraded, missing] = result.scoped_counts(source);
    let scoped = source.iter().filter(|item| in_scope(item)).count();
    let roles = |want: fn(&TargetRole) -> bool| {
        result.audit.target_roles.iter().filter(|r| want(r)).count()
    };
    println!(
        "Source: {} messages to rebuild ({} other items not checked). Target: {} live items, {} parked.",
        scoped,
        source.len() - scoped,
        result.live.len(),
        target.len() - result.live.len()
    );
    println!();
    println!("Source messages in the live target");
    println!("  ok          {ok:>7}  a healthy copy in the right folder");
    println!("  misplaced   {misplaced:>7}  a healthy copy in another folder");
    println!("  degraded    {degraded:>7}  damaged copies only");
    println!("  missing     {missing:>7}  no live copy");
    println!();
    println!("Live target items");
    println!(
        "  duplicates  {:>7}  extra copies outside the quarantine",
        result.audit.duplicates()
    );
    println!(
        "  new         {:>7}  no source item, received since the cutoff",
        roles(|r| *r == TargetRole::New)
    );
    println!(
        "  unmatched   {:>7}  no source item, older",
        roles(|r| *r == TargetRole::Unmatched)
    );
    println!(
        "  at the top  {:>7}  left at the top of the mailbox",
        result.at_top.len()
    );
    println!(
        "Parked items still needed: {} (the only copy of a source message)",
        result.parked_needed.len()
    );

    let mut defects: Vec<(Defect, usize)> = result.audit.defect_counts().into_iter().collect();
    if !defects.is_empty() {
        defects.sort();
        println!();
        println!("Defects of the chosen live copies");
        for (defect, n) in defects {
            println!("  {:<20} {n:>7}", defect.name());
        }
    }
    println!();
    if result.is_clean(source) {
        println!("Verdict: clean. Every source message has a healthy copy in its folder.");
    } else {
        println!("Verdict: not clean yet. See the details below and in the reports.");
    }
}

fn write_reports(
    work: &Path,
    result: &Verification,
    source: &[ItemRecord],
    target: &[ItemRecord],
) -> Result<(), String> {
    let time = |t: Option<i64>| t.map(format_utc).unwrap_or_default();
    let text = |t: &Option<String>| t.clone().unwrap_or_default();

    // The chosen live copy of each source item, with its defects.
    let mut chosen: Vec<Option<(usize, &[Defect])>> = vec![None; source.len()];
    for (role, &index) in result.audit.target_roles.iter().zip(&result.live) {
        if let TargetRole::Copy { source: s, defects } = role {
            chosen[*s] = Some((index, defects));
        }
    }
    let rows: Vec<Vec<String>> = source
        .iter()
        .enumerate()
        .filter(|(s, item)| {
            in_scope(item) && result.audit.source_status[*s] != ecm_audit::SourceStatus::Ok
        })
        .map(|(s, item)| {
            let (folder, defects, key) = match chosen[s] {
                Some((index, defects)) => (
                    target[index].folder.clone(),
                    defects
                        .iter()
                        .map(|d| d.name())
                        .collect::<Vec<_>>()
                        .join(" "),
                    target[index].key.clone(),
                ),
                None => Default::default(),
            };
            vec![
                result.audit.source_status[s].name().into(),
                defects,
                item.folder.clone(),
                folder,
                text(&item.subject),
                text(&item.sender),
                time(item.received),
                text(&item.message_id),
                item.key.clone(),
                key,
            ]
        })
        .collect();
    write_csv(
        &work.join("verify-source.csv"),
        &[
            "status",
            "defects",
            "folder",
            "copy_folder",
            "subject",
            "sender",
            "received",
            "message_id",
            "source_key",
            "copy_key",
        ],
        &rows,
    )
    .map_err(|e| e.to_string())?;

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut push = |what: &str, index: usize| {
        let item = &target[index];
        rows.push(vec![
            what.into(),
            display(&item.folder).into(),
            item.class.clone(),
            text(&item.subject),
            text(&item.sender),
            time(item.received),
            text(&item.message_id),
            item.key.clone(),
        ]);
    };
    for (role, &index) in result.audit.target_roles.iter().zip(&result.live) {
        match role {
            TargetRole::Duplicate { .. } => push("duplicate", index),
            TargetRole::Unmatched => push("unmatched", index),
            _ => {}
        }
    }
    for &index in &result.at_top {
        push("at-top", index);
    }
    for &index in &result.parked_needed {
        push("parked-needed", index);
    }
    write_csv(
        &work.join("verify-target.csv"),
        &[
            "issue",
            "folder",
            "class",
            "subject",
            "sender",
            "received",
            "message_id",
            "key",
        ],
        &rows,
    )
    .map_err(|e| e.to_string())
}
