//! `ecm plan-pst`: plan re-creating, from a PST, the messages the target has no
//! healthy copy of, rebuilt as MIME so that `ecm apply` needs neither the PST
//! nor Graph.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use ecm_audit::{
    Defect, SourceStatus, TargetRole,
    plan::{Action, QUARANTINE_FOLDER, counts, plan},
    verify::is_parked,
};
use ecm_model::{
    item::{format_utc, parse_utc},
    path,
};
use ecm_store::{SOURCE_INVENTORY, read_inventory};

use crate::{
    EwsArgs, connect,
    plan_cmd::{created_items, folder_map},
    verify_cmd::VERIFIED_INVENTORY,
};

/// What earlier plans made that a PST plan may replace.
pub struct Replacements<'a> {
    /// Applied plans whose re-created messages may be replaced.
    pub after: &'a [std::path::PathBuf],
    /// Mailbox written as an empty or address-less `From`.
    pub sender: Option<&'a str>,
    /// Only replace; do not re-create messages the target lacks.
    pub only: bool,
}

/// Which PST messages a plan considers, and where they go.
pub struct Selection<'a> {
    /// A file listing PST item keys, one per line: only those are considered,
    /// to split a large restore into plans of a reasonable size.
    pub keys: Option<&'a Path>,
    /// `FROM=TO` folder renames, applied to source paths (and their sub-folders)
    /// before they are mapped to the target, for folders renamed since.
    pub renames: &'a [String],
}

/// A folder path with the first matching `(from, to)` rename applied to it or to
/// the folder it is under.
fn renamed(folder: &str, renames: &[(&str, &str)]) -> String {
    for (from, to) in renames {
        if folder == *from {
            return (*to).to_owned();
        }
        if let Some(rest) = folder.strip_prefix(&format!("{from}/")) {
            return format!("{to}/{rest}");
        }
    }
    folder.to_owned()
}

/// Writes the plan. Reads the PST, read-only, and checks it did not change.
pub fn plan_pst_command(
    ews: &EwsArgs,
    work: &Path,
    pst: &Path,
    cutoff: &str,
    replacements: &Replacements<'_>,
    selection: &Selection<'_>,
    out: &Path,
) -> Result<(), String> {
    let Replacements {
        after,
        sender,
        only: replacements_only,
    } = *replacements;
    let cutoff = parse_utc(&format!("{cutoff}T00:00:00Z"))
        .ok_or_else(|| format!("invalid --cutoff {cutoff:?}: expected YYYY-MM-DD"))?;
    let mut source = read_inventory(&work.join(SOURCE_INVENTORY)).map_err(|e| e.to_string())?;
    let target: Vec<_> = read_inventory(&work.join(VERIFIED_INVENTORY))
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|item| !is_parked(&item.folder))
        .collect();

    let client = connect(ews)?;
    let (folders, existing) = folder_map(&client)?;
    if let Some(keys) = selection.keys {
        let text = fs::read_to_string(keys).map_err(|e| format!("{}: {e}", keys.display()))?;
        let keys: HashSet<&str> = text
            .lines()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .collect();
        source.retain(|item| keys.contains(item.key.as_str()));
    }
    let renames: Vec<(&str, &str)> = selection
        .renames
        .iter()
        .map(|r| {
            r.split_once('=')
                .ok_or_else(|| format!("--rename {r:?}: expected FROM=TO"))
        })
        .collect::<Result<_, _>>()?;
    for item in &mut source {
        item.folder = renamed(&item.folder, &renames);
        item.folder = folders.target(&item.folder);
    }
    source.retain(|item| item.received.is_none_or(|received| received < cutoff));

    let audit = ecm_audit::audit(&source, &target, cutoff);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let run = format_utc(now).replace([' ', ':'], "-");
    let mut actions = plan(
        &audit,
        &source,
        &target,
        &folders,
        &existing,
        &HashSet::new(),
        &run,
    );
    if replacements_only {
        // Messages missing now may have been deleted by the user since: only
        // the replacements below are wanted.
        actions.retain(|action| matches!(action, Action::CreateFolder { .. }));
    }

    // Messages an earlier plan re-created from a source that lost their
    // Message-ID or sender: the PST's own copy replaces them, and every copy
    // made that way is set aside once it is back.
    let mut created = HashMap::new();
    for plan in after {
        created.extend(created_items(plan)?);
    }
    let quarantine = path::join(&path::join("", QUARANTINE_FOLDER), &run);
    let mut copies: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut replace: Vec<usize> = Vec::new();
    for (index, role) in audit.target_roles.iter().enumerate() {
        match role {
            TargetRole::Copy { source: s, defects }
                if ecm_audit::verify::in_scope(&source[*s])
                    && created.contains_key(&target[index].key)
                    // A Message-ID lost on a PST rebuild would be lost again.
                    && ((defects.contains(&Defect::MessageIdChanged)
                        && created.get(&target[index].key) == Some(&false)
                        || sender.is_some() && defects.contains(&Defect::SenderLost))
                        && audit.source_status[*s] == SourceStatus::Degraded
                        // A signed message rebuilt before signed entities were
                        // handled came back without a body.
                        || source[*s].class.starts_with("IPM.Note.SMIME")
                            && target[index].body_empty == Some(true)
                        // Sent items whose Outlook.com MIME named the sender by its
                        // internal Exchange address (IMCEAEX-...).
                        || sender.is_some()
                            && target[index]
                                .sender
                                .as_deref()
                                .is_some_and(|s| s.starts_with("imceaex-"))) =>
            {
                replace.push(*s);
                copies.entry(*s).or_default().push(index);
            }
            TargetRole::Duplicate { source: s } if created.contains_key(&target[index].key) => {
                copies.entry(*s).or_default().push(index);
            }
            _ => {}
        }
    }
    replace.sort_unstable();
    for s in replace {
        let item = &source[s];
        let step = actions.len();
        actions.push(Action::Recreate {
            source: item.key.clone(),
            to: folders.target(&item.folder),
            read: item.read,
            sent: item.sent,
            received: item.received,
            subject: item.subject.clone(),
        });
        for &index in &copies[&s] {
            actions.push(Action::Move {
                item: target[index].key.clone(),
                to: quarantine.clone(),
                reason: "replaced".into(),
                requires: Some(step),
            });
        }
    }

    // Rebuild each re-created message from the PST.
    let before = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
    let mut failed: Vec<String> = Vec::new();
    let mut skipped_attachments = 0;
    let mut dropped = HashSet::new();
    {
        let reader = ecm_pst::PstSource::open(pst).map_err(|e| e.to_string())?;
        for (step, action) in actions.iter_mut().enumerate() {
            let Action::Recreate {
                source: key,
                to,
                read,
                sent,
                received,
                subject,
            } = action
            else {
                continue;
            };
            match reader.message_content(key) {
                Ok(content) if !ecm_pst::content::has_body(&content) => {
                    failed.push(format!(
                        "{to}: {subject:?}: no text or HTML body in the PST (RTF only)"
                    ));
                    dropped.insert(step);
                }
                Ok(content) => {
                    skipped_attachments += content.skipped_attachments;
                    let mime = ecm_pst::content::to_mime_with(&content, sender);
                    *action = Action::RecreateMime {
                        to: to.clone(),
                        subject: subject.clone(),
                        read: *read,
                        sent: *sent,
                        received: *received,
                        mime: base64::engine::general_purpose::STANDARD.encode(mime),
                    };
                }
                Err(error) => {
                    failed.push(format!("{to}: {subject:?}: {error}"));
                    dropped.insert(step);
                }
            }
        }
    }
    let after = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
    if before != after {
        return Err(format!("{} changed while it was read", pst.display()));
    }

    // Drop what could not be rebuilt, and the moves waiting for it, and the
    // quarantine run folder when nothing is left to go into it.
    let quarantine_used = actions.iter().any(|action| {
        matches!(action, Action::Move { to, requires, .. }
            if *to == quarantine && !requires.is_some_and(|r| dropped.contains(&r)))
    });
    let mut index_map = HashMap::new();
    let mut kept = Vec::new();
    for (step, action) in actions.into_iter().enumerate() {
        let waits_on_dropped =
            matches!(&action, Action::Move { requires: Some(r), .. } if dropped.contains(r));
        let unused_folder = !quarantine_used
            && matches!(&action, Action::CreateFolder { path } if *path == quarantine);
        if dropped.contains(&step) || waits_on_dropped || unused_folder {
            continue;
        }
        index_map.insert(step, kept.len());
        kept.push(action);
    }
    for action in &mut kept {
        if let Action::Move {
            requires: Some(r), ..
        } = action
        {
            *r = index_map[r];
        }
    }

    let mut text = String::new();
    for action in &kept {
        text.push_str(&serde_json::to_string(action).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("Plan {run}: {} steps into {}", kept.len(), out.display());
    for (kind, n) in counts(&kept) {
        println!("  {kind:<14} {n:>7}");
    }
    if skipped_attachments > 0 {
        println!("{skipped_attachments} attachments left out (embedded messages or no data).");
    }
    if !failed.is_empty() {
        println!();
        println!("Not rebuilt from the PST ({}):", failed.len());
        for line in &failed {
            println!("  {line}");
        }
    }
    println!();
    println!(
        "Nothing was changed; the PST is unchanged. Review the plan, then run `ecm apply --plan {} --dry-run`.",
        out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::renamed;

    #[test]
    fn renames_a_folder_and_what_is_under_it() {
        let renames = [("Archive/Shop", "Archive/Shop and more")];
        assert_eq!(renamed("Archive/Shop", &renames), "Archive/Shop and more");
        assert_eq!(
            renamed("Archive/Shop/2019", &renames),
            "Archive/Shop and more/2019"
        );
        assert_eq!(renamed("Archive/Shopping", &renames), "Archive/Shopping");
    }
}
