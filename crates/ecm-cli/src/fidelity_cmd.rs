//! `ecm fidelity-test`: re-create a few items in a dedicated test folder and
//! compare them property by property with their reference (milestone M3).
//!
//! It only creates items, in the folder [`TEST_FOLDER`]; nothing else in the
//! mailbox is changed.

use std::{fs, path::Path};

use ecm_audit::{Defect, SourceStatus, TargetRole};
use ecm_ews::write::ExtendedValue;
use ecm_model::{
    ItemRecord,
    compare::{COMPARED, PropertyMap, compare},
    item::format_iso,
};
use ecm_store::{TARGET_INVENTORY, read_inventory};
use serde_json::Value;

use crate::{EwsArgs, audit_cmd::GraphArgs, audit_cmd::OUTLOOK_INVENTORY, connect};

/// The only folder this test writes to, directly under the top of the mailbox.
pub const TEST_FOLDER: &str = "ecm-test";

/// `MSGFLAG_READ`. `MSGFLAG_UNSENT` is never set, so items are not drafts.
const MSGFLAG_READ: u32 = 0x1;

/// Runs both tests and prints their comparisons.
pub fn fidelity_test(
    ews: &EwsArgs,
    work: &Path,
    backup: Option<&Path>,
    graph: Option<&GraphArgs>,
    count: usize,
) -> Result<(), String> {
    let client = connect(ews)?;
    let folder = client
        .ensure_top_folder(TEST_FOLDER)
        .map_err(|e| e.to_string())?;
    println!("Test folder: {TEST_FOLDER} (items are only created there).");

    if let Some(backup) = backup {
        println!();
        println!("== Restore from backup (UploadItems) ==");
        let entries = backup_entries(backup, count)?;
        let data = entries
            .iter()
            .map(|(file, _)| fs::read(backup.join(file)).map_err(|e| format!("{file}: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        let uploaded = client
            .upload_items(&folder, &data)
            .map_err(|e| e.to_string())?;
        for ((_, original), new) in entries.iter().zip(uploaded) {
            let new = new.map_err(|e| e.to_string())?;
            let props = client
                .item_properties(&[original.as_str(), new.as_str()])
                .map_err(|e| e.to_string())?;
            let mut props = props.into_iter();
            let reference = props
                .next()
                .ok_or("no reference")?
                .map_err(|e| e.to_string())?;
            let copy = props.next().ok_or("no copy")?.map_err(|e| e.to_string())?;
            print_comparison("original (target)", &reference, &[("restored", &copy)]);
        }
    }

    if let Some(graph) = graph {
        println!();
        println!("== Re-create from Outlook.com (MIME + creation-time properties) ==");
        reupload_from_outlook(&client, &folder, work, graph, count)?;
    }
    Ok(())
}

/// The first `count` message entries of a backup manifest: (file, original id).
fn backup_entries(backup: &Path, count: usize) -> Result<Vec<(String, String)>, String> {
    let manifest = fs::read_to_string(backup.join("manifest.jsonl")).map_err(|e| e.to_string())?;
    Ok(manifest
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["class"].as_str() == Some("IPM.Note"))
        .take(count)
        .map(|entry| {
            (
                entry["file"].as_str().unwrap_or_default().to_owned(),
                entry["item_id"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect())
}

/// Picks Outlook.com messages whose chosen target copy has a damaged body:
/// one with attachments, one sent item, then any.
fn pick_damaged(source: &[ItemRecord], target: &[ItemRecord], count: usize) -> Vec<(usize, usize)> {
    let audit = ecm_audit::audit(source, target, i64::MAX);
    let mut damaged: Vec<(usize, usize)> = audit
        .target_roles
        .iter()
        .enumerate()
        .filter_map(|(copy, role)| match role {
            TargetRole::Copy { source: s, defects }
                if defects.contains(&Defect::BodyFormatChanged)
                    && audit.source_status[*s] == SourceStatus::Degraded =>
            {
                Some((*s, copy))
            }
            _ => None,
        })
        .collect();
    damaged.sort();
    let mut picked: Vec<(usize, usize)> = Vec::new();
    let wants: [&dyn Fn(&ItemRecord) -> bool; 2] =
        [&|item| item.has_attachments == Some(true), &|item| {
            item.folder.to_lowercase().contains("sent")
        }];
    for want in wants {
        take_first(&damaged, &mut picked, |s| want(&source[s]));
    }
    while picked.len() < count && take_first(&damaged, &mut picked, |_| true) {}
    picked.truncate(count);
    picked
}

/// Adds the first not yet picked pair whose source matches `want`, and says
/// whether one was found.
fn take_first(
    candidates: &[(usize, usize)],
    picked: &mut Vec<(usize, usize)>,
    want: impl Fn(usize) -> bool,
) -> bool {
    let found = candidates
        .iter()
        .find(|&&(s, _)| want(s) && !picked.iter().any(|p| p.0 == s))
        .copied();
    if let Some(pair) = found {
        picked.push(pair);
    }
    found.is_some()
}

#[cfg(windows)]
fn reupload_from_outlook<T: ecm_ews::Transport>(
    client: &ecm_ews::EwsClient<T>,
    folder: &str,
    work: &Path,
    args: &GraphArgs,
    count: usize,
) -> Result<(), String> {
    let source = read_inventory(&work.join(OUTLOOK_INVENTORY)).map_err(|e| e.to_string())?;
    let target = read_inventory(&work.join(TARGET_INVENTORY)).map_err(|e| e.to_string())?;
    let picked = pick_damaged(&source, &target, count);
    if picked.is_empty() {
        return Err("no Outlook.com message with a damaged target copy to test".into());
    }

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
    let graph = ecm_graph::GraphClient::new(transport, token);

    for (s, old) in picked {
        let item = &source[s];
        let reference = graph
            .message_properties(&item.key)
            .map_err(|e| e.to_string())?;
        let mime = graph.mime(&item.key).map_err(|e| e.to_string())?;
        let mut extended = vec![ExtendedValue {
            tag: 0x0E07,
            kind: "Integer",
            value: if item.read == Some(true) {
                MSGFLAG_READ
            } else {
                0
            }
            .to_string(),
        }];
        if let Some(received) = item.received {
            extended.push(ExtendedValue {
                tag: 0x0E06,
                kind: "SystemTime",
                value: format_iso(received),
            });
        }
        if let Some(sent) = item.sent {
            extended.push(ExtendedValue {
                tag: 0x0039,
                kind: "SystemTime",
                value: format_iso(sent),
            });
        }
        let new = client
            .create_from_mime(folder, &mime, &extended)
            .map_err(|e| e.to_string())?;
        let props = client
            .item_properties(&[new.as_str(), target[old].key.as_str()])
            .map_err(|e| e.to_string())?;
        let mut props = props.into_iter();
        let copy = props.next().ok_or("no copy")?.map_err(|e| e.to_string())?;
        let damaged = props
            .next()
            .ok_or("no old copy")?
            .map_err(|e| e.to_string())?;
        println!();
        println!(
            "Source folder: {} ({} bytes of MIME)",
            item.folder,
            mime.len()
        );
        print_comparison(
            "Outlook.com",
            &reference,
            &[("re-created", &copy), ("old copy", &damaged)],
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn reupload_from_outlook<T: ecm_ews::Transport>(
    _: &ecm_ews::EwsClient<T>,
    _: &str,
    _: &Path,
    _: &GraphArgs,
    _: usize,
) -> Result<(), String> {
    Err("Microsoft Graph access needs Windows for now (WinHTTP)".into())
}

/// Prints the compared properties of a reference and its copies, marking each
/// copy's value ✓ (same) or ✗ (different), then the differences count.
pub(crate) fn print_comparison(
    reference_name: &str,
    reference: &PropertyMap,
    copies: &[(&str, &PropertyMap)],
) {
    let short = |value: Option<&String>| {
        let value = value.map(String::as_str).unwrap_or("-");
        let mut chars = value.chars();
        let head: String = chars.by_ref().take(38).collect();
        if chars.next().is_some() {
            format!("{head}…")
        } else {
            head
        }
    };
    print!("  {:<16} {:<40}", "property", reference_name);
    for (name, _) in copies {
        print!(" {:<42}", name);
    }
    println!();
    for &name in COMPARED
        .iter()
        .chain(["native_body", "body_has_markup_in_text", "message_flags"].iter())
    {
        if !reference.contains_key(name) && copies.iter().all(|(_, c)| !c.contains_key(name)) {
            continue;
        }
        print!("  {:<16} {:<40}", name, short(reference.get(name)));
        for (_, copy) in copies {
            let differs =
                COMPARED.contains(&name) && compare(reference, copy).iter().any(|d| d.name == name);
            let mark = if !COMPARED.contains(&name) {
                ' '
            } else if differs {
                '✗'
            } else {
                '✓'
            };
            print!(" {mark} {:<40}", short(copy.get(name)));
        }
        println!();
    }
    for (name, copy) in copies {
        let differences = compare(reference, copy);
        println!(
            "  → {name}: {} difference(s){}",
            differences.len(),
            if differences.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    differences
                        .iter()
                        .map(|d| d.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::pick_damaged;
    use ecm_model::ItemRecord;

    fn item(key: &str, folder: &str, id: &str, native: u8, attachments: bool) -> ItemRecord {
        ItemRecord {
            key: key.into(),
            folder: folder.into(),
            class: "IPM.Note".into(),
            message_id: Some(id.into()),
            subject: Some(id.into()),
            sent: Some(100),
            received: Some(100),
            native_body: Some(native),
            has_attachments: Some(attachments),
            ..Default::default()
        }
    }

    #[test]
    fn picks_damaged_copies_with_attachments_and_sent_items_first() {
        let source = vec![
            item("s0", "Inbox", "<0>", 3, false),
            item("s1", "Sent Items", "<1>", 3, false),
            item("s2", "Inbox", "<2>", 3, true),
            item("s3", "Inbox", "<3>", 3, false),
        ];
        let target = vec![
            item("t0", "", "<0>", 1, false),
            item("t1", "", "<1>", 1, false),
            item("t2", "", "<2>", 1, true),
            item("t3", "", "<3>", 3, false),
        ];
        let picked = pick_damaged(&source, &target, 3);
        assert_eq!(picked, [(2, 2), (1, 1), (0, 0)]);
    }
}
