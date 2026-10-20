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
    plan::{Action, Applied, QUARANTINE_FOLDER, counts, plan},
    verify::is_parked,
};
use ecm_model::{
    item::{format_utc, parse_utc},
    path,
};
use ecm_store::{SOURCE_INVENTORY, read_inventory};

use crate::{
    EwsArgs,
    audit_cmd::{GraphArgs, OUTLOOK_INVENTORY},
    connect,
    plan_cmd::{created_items, folder_map, moved_items},
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
    /// Copies `check-content` found damaged: replaced in the folder they are in,
    /// when an applied plan of `after` created or moved them.
    pub damaged: Option<&'a Path>,
}

/// The target keys of the copies a `check-content` file lists.
fn damaged_keys(path: &Path) -> Result<HashSet<String>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    if lines
        .next()
        .is_none_or(|header| !header.starts_with("target_key,"))
    {
        return Err(format!(
            "{}: not a file `ecm check-content` wrote",
            path.display()
        ));
    }
    Ok(lines
        .filter_map(|line| line.split(',').next())
        .map(|key| key.trim_matches('"').to_owned())
        .filter(|key| !key.is_empty())
        .collect())
}

/// Above this size, a rebuilt message is created without its files, which then
/// go one request each: an Exchange server refused a 97 MB `CreateItem` (HTTP
/// 413) and took one of 30 MB.
const MAX_MIME: usize = 25 * 1024 * 1024;

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
/// The mailbox a PST plan is for.
pub enum PstTarget<'a> {
    /// An Exchange mailbox (EWS), listed in `target.after.jsonl`: messages are
    /// created from MIME.
    Ews(&'a EwsArgs),
    /// An Outlook.com mailbox (Microsoft Graph), listed in `outlook.jsonl`:
    /// messages are created from JSON with the properties only creation sets,
    /// their files as `attach` steps (`ecm apply-graph`).
    Graph(&'a GraphArgs),
}

/// The folder map and folder paths of the target.
/// Where the folders of an Outlook.com target are kept after a first sign-in,
/// so that plans in batches need no other: names only, no token.
pub const OUTLOOK_FOLDERS: &str = "outlook-folders.json";

fn target_folders(
    target: &PstTarget<'_>,
    work: &Path,
) -> Result<
    (
        ecm_audit::plan::FolderMap,
        std::collections::BTreeSet<String>,
    ),
    String,
> {
    match target {
        PstTarget::Ews(ews) => folder_map(&connect(ews)?),
        #[cfg(windows)]
        PstTarget::Graph(graph) => {
            let saved = work.join(OUTLOOK_FOLDERS);
            if let Ok(text) = fs::read_to_string(&saved) {
                let value: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", saved.display()))?;
                let map = ecm_audit::plan::FolderMap {
                    well_known: serde_json::from_value(value["well_known"].clone())
                        .map_err(|e| format!("{}: {e}", saved.display()))?,
                };
                let existing = serde_json::from_value(value["existing"].clone())
                    .map_err(|e| format!("{}: {e}", saved.display()))?;
                println!(
                    "Outlook.com folders from {} (delete it to read them again).",
                    saved.display()
                );
                return Ok((map, existing));
            }
            let client = crate::pim_cmd::sign_in(graph)?;
            let folders = crate::graph_plan_cmd::graph_folder_map(&client)?;
            let value = serde_json::json!({
                "well_known": folders.map.well_known,
                "existing": folders.existing,
            });
            fs::write(&saved, value.to_string())
                .map_err(|e| format!("{}: {e}", saved.display()))?;
            Ok((folders.map, folders.existing))
        }
        #[cfg(not(windows))]
        PstTarget::Graph(_) => Err("Microsoft Graph access needs Windows for now (WinHTTP)".into()),
    }
}

pub fn plan_pst_command(
    target_mailbox: &PstTarget<'_>,
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
        damaged,
    } = *replacements;
    let damaged = damaged.map(damaged_keys).transpose()?.unwrap_or_default();
    let cutoff = parse_utc(&format!("{cutoff}T00:00:00Z"))
        .ok_or_else(|| format!("invalid --cutoff {cutoff:?}: expected YYYY-MM-DD"))?;
    let mut source = read_inventory(&work.join(SOURCE_INVENTORY)).map_err(|e| e.to_string())?;
    let graph = matches!(target_mailbox, PstTarget::Graph(_));
    let inventory = work.join(if graph {
        OUTLOOK_INVENTORY
    } else {
        VERIFIED_INVENTORY
    });
    let target: Vec<_> = read_inventory(&inventory)
        .map_err(|e| format!("{}: {e}", inventory.display()))?
        .into_iter()
        .filter(|item| !is_parked(&item.folder))
        .collect();

    let (folders, existing) = target_folders(target_mailbox, work)?;
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
        &Applied::default(),
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
    let mut moved = HashSet::new();
    for plan in after {
        created.extend(created_items(plan)?);
        moved.extend(moved_items(plan)?);
    }
    let quarantine = path::join(&path::join("", QUARANTINE_FOLDER), &run);
    let mut copies: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut replace: Vec<usize> = Vec::new();
    // Damaged copies are rebuilt where they are, with their read state: the
    // user may have filed or read them since.
    let mut in_place: HashMap<usize, usize> = HashMap::new();
    // Damaged extra copies of a message whose kept copy is healthy: only set
    // aside.
    let mut damaged_duplicates: Vec<(usize, usize)> = Vec::new();
    let mut not_ours = Vec::new();
    for (index, role) in audit.target_roles.iter().enumerate() {
        let key = &target[index].key;
        if damaged.contains(key) {
            let ours = created.contains_key(key) || moved.contains(key);
            match role {
                TargetRole::Copy { source: s, .. } if ours => {
                    replace.push(*s);
                    copies.entry(*s).or_default().push(index);
                    in_place.insert(*s, index);
                }
                TargetRole::Duplicate { source: s } if ours => {
                    copies.entry(*s).or_default().push(index);
                    damaged_duplicates.push((*s, index));
                }
                _ => not_ours.push(index),
            }
            continue;
        }
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
    replace.dedup();
    for (s, index) in damaged_duplicates {
        if replace.binary_search(&s).is_err() {
            actions.push(Action::Move {
                item: target[index].key.clone(),
                to: quarantine.clone(),
                reason: "damaged".into(),
                requires: None,
            });
        }
    }
    if !not_ours.is_empty() {
        println!(
            "{} damaged copies left alone: no plan of --after created or moved them, or the PST message is not theirs.",
            not_ours.len()
        );
    }
    for s in replace {
        let item = &source[s];
        let step = actions.len();
        let copy = in_place.get(&s).map(|&index| &target[index]);
        actions.push(Action::Recreate {
            source: item.key.clone(),
            to: copy.map_or_else(|| folders.target(&item.folder), |c| c.folder.clone()),
            read: copy.and_then(|c| c.read).or(item.read),
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
    let mut partial: Vec<String> = Vec::new();
    let mut split: Vec<String> = Vec::new();
    let mut files: HashMap<usize, Vec<ecm_pst::content::AttachedFile>> = HashMap::new();
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
                Ok(content) if ecm_pst::content::loses_body(&content) => {
                    failed.push(format!(
                        "{to}: {subject:?}: its RTF body could not be turned into HTML or text"
                    ));
                    dropped.insert(step);
                }
                Ok(mut content) => {
                    if content.skipped_attachments > 0 {
                        partial.push(format!(
                            "{to}: {subject:?}: {} left out",
                            content.skipped_attachments
                        ));
                    }
                    if graph {
                        // From JSON, with the plan's read state and times; every
                        // file as its own step, inline only when the HTML shows it.
                        content.read = *read;
                        content.sent = *sent;
                        content.received = *received;
                        let message = crate::graph_cmd::graph_message(&content, sender);
                        let shown: Vec<Option<String>> = content
                            .attachments
                            .iter()
                            .map(|f| {
                                ecm_pst::content::shown_content_id(&content, f).map(str::to_owned)
                            })
                            .collect();
                        let mut attachments = std::mem::take(&mut content.attachments);
                        for (file, id) in attachments.iter_mut().zip(shown) {
                            file.content_id = id;
                        }
                        if !attachments.is_empty() {
                            files.insert(step, attachments);
                        }
                        *action = Action::CreateMessage {
                            to: to.clone(),
                            subject: subject.clone(),
                            message: serde_json::to_string(&message).map_err(|e| e.to_string())?,
                        };
                        continue;
                    }
                    let mut mime = ecm_pst::content::to_mime_with(&content, sender);
                    // Too large for one request: created without its files, which
                    // follow one by one. Not a signed message, whose body is a file.
                    let mut bare = content.clone();
                    bare.attachments.clear();
                    if mime.len() > MAX_MIME
                        && !content.attachments.is_empty()
                        && ecm_pst::content::has_body(&bare) == ecm_pst::content::has_body(&content)
                    {
                        mime = ecm_pst::content::to_mime_with(&bare, sender);
                        split.push(format!(
                            "{to}: {subject:?}: {} files",
                            content.attachments.len()
                        ));
                        // Files go with the Content-ID only the HTML shows.
                        let shown: Vec<Option<String>> = content
                            .attachments
                            .iter()
                            .map(|f| {
                                ecm_pst::content::shown_content_id(&content, f).map(str::to_owned)
                            })
                            .collect();
                        let mut attachments = content.attachments;
                        for (file, id) in attachments.iter_mut().zip(shown) {
                            file.content_id = id;
                        }
                        files.insert(step, attachments);
                    }
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

    let kept = finish(actions, &dropped, files);

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
    report_list(
        "Rebuilt without some attachments: embedded messages, no data, or unreadable",
        &partial,
    );
    report_list(
        "Too large for one request: created first, then given their files one by one",
        &split,
    );
    report_list("Not rebuilt from the PST", &failed);
    println!();
    println!(
        "Nothing was changed; the PST is unchanged. Review the plan, then run `ecm {} --plan {} --dry-run`.",
        if graph { "apply-graph" } else { "apply" },
        out.display()
    );
    Ok(())
}

fn report_list(title: &str, lines: &[String]) {
    if !lines.is_empty() {
        println!();
        println!("{title} ({}):", lines.len());
        for line in lines {
            println!("  {line}");
        }
    }
}

/// The plan's last pass. Drops the steps that could not be rebuilt, the moves
/// waiting for them, and the folders nothing is left to go into (the quarantine
/// run folder when nothing is replaced, a source folder whose messages are all
/// left out). After each message too large for one request come its files, each
/// waiting for the message or the file before it; the moves that waited for the
/// message wait for its last file.
fn finish(
    actions: Vec<Action>,
    dropped: &HashSet<usize>,
    mut files: HashMap<usize, Vec<ecm_pst::content::AttachedFile>>,
) -> Vec<Action> {
    let waits_on_dropped = |action: &Action| matches!(action, Action::Move { requires: Some(r), .. } if dropped.contains(r));
    let mut used: HashSet<String> = HashSet::new();
    for (step, action) in actions.iter().enumerate() {
        let to = match action {
            Action::Recreate { to, .. }
            | Action::RecreateMime { to, .. }
            | Action::CreateMessage { to, .. }
            | Action::Move { to, .. } => to,
            _ => continue,
        };
        if dropped.contains(&step) || waits_on_dropped(action) {
            continue;
        }
        let mut folder = Some(to.clone());
        while let Some(path) = folder.filter(|p| !p.is_empty()) {
            folder = path::parent(&path);
            used.insert(path);
        }
    }
    let mut index_map = HashMap::new();
    let mut kept = Vec::new();
    for (step, action) in actions.into_iter().enumerate() {
        let unused_folder =
            matches!(&action, Action::CreateFolder { path } if !used.contains(path));
        if dropped.contains(&step) || waits_on_dropped(&action) || unused_folder {
            continue;
        }
        index_map.insert(step, kept.len());
        kept.push(action);
        for file in files.remove(&step).unwrap_or_default() {
            kept.push(Action::Attach {
                requires: kept.len() - 1,
                name: file.name,
                content_type: file
                    .mime_type
                    .unwrap_or_else(|| "application/octet-stream".into()),
                content_id: file.content_id,
                data: base64::engine::general_purpose::STANDARD.encode(file.data),
            });
            index_map.insert(step, kept.len() - 1);
        }
    }
    for action in &mut kept {
        if let Action::Move {
            requires: Some(r), ..
        } = action
        {
            *r = index_map[r];
        }
    }

    kept
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use ecm_audit::plan::Action;
    use ecm_pst::content::AttachedFile;

    use super::{finish, renamed};

    fn mime(to: &str) -> Action {
        Action::RecreateMime {
            to: to.into(),
            subject: None,
            read: None,
            sent: None,
            received: None,
            mime: String::new(),
        }
    }

    fn quarantine(item: &str, requires: usize) -> Action {
        Action::Move {
            item: item.into(),
            to: "Q/run".into(),
            reason: "replaced".into(),
            requires: Some(requires),
        }
    }

    #[test]
    fn drops_what_failed_and_sends_large_messages_files_one_by_one() {
        let folder = |path: &str| Action::CreateFolder { path: path.into() };
        let actions = vec![
            folder("Q"),
            folder("Q/run"),
            folder("Unused"),
            mime("Inbox"),
            mime("Unused"),
            quarantine("old", 3),
            quarantine("gone", 4),
        ];
        let file = |name: &str| AttachedFile {
            name: name.into(),
            mime_type: Some("video/mp4".into()),
            content_id: None,
            data: b"x".to_vec(),
        };
        let files = HashMap::from([(3, vec![file("a.mp4"), file("b.mp4")])]);
        let kept = finish(actions, &HashSet::from([4]), files);
        let attach = |requires: usize, name: &str| Action::Attach {
            requires,
            name: name.into(),
            content_type: "video/mp4".into(),
            content_id: None,
            data: "eA==".into(),
        };
        assert_eq!(
            kept,
            [
                folder("Q"),
                folder("Q/run"),
                mime("Inbox"),
                attach(2, "a.mp4"),
                attach(3, "b.mp4"),
                quarantine("old", 4),
            ]
        );
    }

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
