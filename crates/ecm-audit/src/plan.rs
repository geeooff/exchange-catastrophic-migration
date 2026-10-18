//! Repair plans: the actions that turn an audited target into a faithful copy of
//! its source.
//!
//! A plan only creates folders, re-creates items from the source, and moves items.
//! Nothing is deleted: damaged copies and duplicates are moved to a quarantine
//! folder. Each quarantine of a damaged copy depends on the re-creation that
//! replaces it, so a failed re-creation keeps the old copy in place.

use std::collections::{BTreeSet, HashMap, HashSet};

use ecm_model::{ItemRecord, path};
use serde::{Deserialize, Serialize};

use crate::{Audit, SourceStatus, TargetRole};

/// Top-level folder that receives quarantined items, one sub-folder per plan.
pub const QUARANTINE_FOLDER: &str = "ecm-quarantine";
/// Top-level folder that receives target items no source knows, for review.
pub const UNMATCHED_FOLDER: &str = "ecm-unmatched";

/// Source folders that are system folders, never rebuilt (compared lower-cased on
/// the first name of the path).
pub const SKIPPED_SOURCE_FOLDERS: &[&str] = &[
    "recoverable items",
    "sync issues",
    "problèmes de synchronisation",
];

/// Well-known folders, by EWS distinguished id, with the names sources use for
/// them (English and French; compared lower-cased).
pub const WELL_KNOWN: &[(&str, &[&str])] = &[
    ("inbox", &["inbox", "boîte de réception"]),
    ("sentitems", &["sent items", "éléments envoyés"]),
    ("deleteditems", &["deleted items", "éléments supprimés"]),
    ("junkemail", &["junk email", "courrier indésirable"]),
    ("drafts", &["drafts", "brouillons"]),
    ("outbox", &["outbox", "boîte d'envoi"]),
    ("archive", &["archive"]),
    ("notes", &["notes"]),
    (
        "conversationhistory",
        &["conversation history", "historique des conversations"],
    ),
    (
        "syncissues",
        &["sync issues", "problèmes de synchronisation"],
    ),
];

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Action {
    /// Create a folder at this target path; its parent exists or is created by
    /// an earlier step.
    CreateFolder {
        /// Target folder path.
        path: String,
    },
    /// Re-create a source item in a target folder.
    Recreate {
        /// Source item key (e.g. a Graph message id).
        source: String,
        /// Target folder path.
        to: String,
        /// Read state to restore.
        read: Option<bool>,
        /// Send time to restore (Unix seconds).
        sent: Option<i64>,
        /// Receive time to restore (Unix seconds).
        received: Option<i64>,
        /// Subject, for reports.
        subject: Option<String>,
    },
    /// Re-create an item from MIME carried by the plan (rebuilt from a PST).
    RecreateMime {
        /// Target folder path.
        to: String,
        /// Subject, for reports.
        subject: Option<String>,
        /// Read state to restore.
        read: Option<bool>,
        /// Send time to restore (Unix seconds).
        sent: Option<i64>,
        /// Receive time to restore (Unix seconds).
        received: Option<i64>,
        /// The message, as base64 MIME.
        mime: String,
    },
    /// Move a target item to a folder.
    Move {
        /// Target item id.
        item: String,
        /// Target folder path.
        to: String,
        /// Why: `misplaced`, `duplicate`, `damaged`, `unmatched`.
        reason: String,
        /// Index of the step that must have succeeded first, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requires: Option<usize>,
    },
    /// Create an item from its EWS XML: a calendar item, contact or task copied
    /// from a source that has no MIME for it.
    Create {
        /// `event`, `contact` or `task`.
        kind: String,
        /// Target folder path.
        to: String,
        /// Subject or name, for reports.
        subject: Option<String>,
        /// The item, as EWS XML (`<t:CalendarItem>`, `<t:Contact>`, `<t:Task>`).
        item: String,
    },
    /// Set the photo of a target contact.
    ContactPhoto {
        /// Target contact id.
        item: String,
        /// Contact name, for reports.
        subject: Option<String>,
        /// The photo, base64.
        photo: String,
    },
}

impl Action {
    /// Short name, for counts.
    pub fn kind(&self) -> &'static str {
        match self {
            Action::CreateFolder { .. } => "create-folder",
            Action::Recreate { .. } | Action::RecreateMime { .. } => "recreate",
            Action::Move { reason, .. } => match reason.as_str() {
                "misplaced" => "move",
                _ => "quarantine",
            },
            Action::Create { kind, .. } => match kind.as_str() {
                "event" => "create-event",
                "contact" => "create-contact",
                "task" => "create-task",
                _ => "create",
            },
            Action::ContactPhoto { .. } => "contact-photo",
        }
    }
}

/// Maps source folder paths to target folder paths: well-known folders to the
/// target's own (localized) folder, other names kept as they are.
#[derive(Debug, Clone, Default)]
pub struct FolderMap {
    /// Target path of each well-known folder, by distinguished id.
    pub well_known: HashMap<String, String>,
}

impl FolderMap {
    /// The target path for a source folder path.
    pub fn target(&self, source: &str) -> String {
        let names = path::split(source);
        let mut iter = names.iter();
        let mut target = String::new();
        if let Some(first) = iter.next() {
            let lower = first.to_lowercase();
            target = WELL_KNOWN
                .iter()
                .find(|(_, aliases)| aliases.contains(&lower.as_str()))
                .and_then(|(id, _)| self.well_known.get(*id))
                .cloned()
                .unwrap_or_else(|| path::join("", first));
        }
        for name in iter {
            target = path::join(&target, name);
        }
        target
    }
}

/// What a plan does, by kind of step.
pub fn counts(actions: &[Action]) -> Vec<(&'static str, usize)> {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for action in actions {
        match counts.iter_mut().find(|(kind, _)| *kind == action.kind()) {
            Some((_, n)) => *n += 1,
            None => counts.push((action.kind(), 1)),
        }
    }
    counts
}

/// Builds the plan for an audit of `target` against `source`.
///
/// `existing` holds the target's folder paths. `run` names the quarantine
/// sub-folder. Only messages are re-created: other source items are left out.
///
/// Only target items at the top of the mailbox, where the migration left its
/// copies, are moved. Items in any other folder were delivered or filed there
/// and are left alone, and so are the source items they serve.
///
/// `replace` holds migration copies an earlier `apply` moved into folders (their
/// ids, from its journal). They are never trusted, even when their properties
/// look right (bodies emptied, attachments with every LF turned into CRLF): each
/// is re-created from its source item, then quarantined.
pub fn plan(
    audit: &Audit,
    source: &[ItemRecord],
    target: &[ItemRecord],
    folders: &FolderMap,
    existing: &BTreeSet<String>,
    replace: &HashSet<String>,
    run: &str,
) -> Vec<Action> {
    let quarantine = path::join(&path::join("", QUARANTINE_FOLDER), run);
    let unmatched = path::join("", UNMATCHED_FOLDER);

    // Which source item each chosen copy serves.
    let mut copy_of: HashMap<usize, usize> = HashMap::new();
    for (index, role) in audit.target_roles.iter().enumerate() {
        if let TargetRole::Copy { source: s, .. } = role {
            copy_of.insert(*s, index);
        }
    }

    let mut needed: BTreeSet<String> = BTreeSet::new();
    let mut recreate = Vec::new();
    let mut moves = Vec::new();
    // Items a plan may move: migration debris at the top, and migration copies an
    // earlier `apply` moved, never mail filed by the user.
    let to_replace = |copy: usize| replace.contains(target[copy].key.as_str());
    let at_top = |copy: usize| target[copy].folder.is_empty() || to_replace(copy);
    for (s, (item, status)) in source.iter().zip(&audit.source_status).enumerate() {
        let skipped_folder = path::split(&item.folder)
            .first()
            .is_some_and(|first| SKIPPED_SOURCE_FOLDERS.contains(&first.to_lowercase().as_str()));
        if !item.is_message() || skipped_folder {
            continue;
        }
        if copy_of.get(&s).is_some_and(|&copy| !at_top(copy)) {
            continue;
        }
        let to = folders.target(&item.folder);
        let status = match (status, copy_of.get(&s)) {
            (SourceStatus::Ok | SourceStatus::Misplaced, Some(&copy)) if to_replace(copy) => {
                &SourceStatus::Degraded
            }
            (status, _) => status,
        };
        match status {
            SourceStatus::Ok => {}
            SourceStatus::Misplaced => {
                needed.insert(to.clone());
                moves.push((copy_of[&s], to, "misplaced", None));
            }
            SourceStatus::Degraded | SourceStatus::Missing => {
                needed.insert(to.clone());
                recreate.push((s, to));
            }
        }
    }

    let mut actions: Vec<Action> = Vec::new();
    // Folders first, parents before children.
    needed.insert(quarantine.clone());
    needed.insert(unmatched.clone());
    let mut to_create: BTreeSet<String> = BTreeSet::new();
    for folder in &needed {
        let mut current = Some(folder.clone());
        while let Some(path) = current.filter(|p| !p.is_empty()) {
            if !existing.contains(&path) {
                to_create.insert(path.clone());
            }
            current = path::parent(&path);
        }
    }
    let mut to_create: Vec<String> = to_create.into_iter().collect();
    to_create.sort_by_key(|p| path::split(p).len());
    actions.extend(
        to_create
            .into_iter()
            .map(|path| Action::CreateFolder { path }),
    );

    // Re-creations, each followed later by the quarantine of the copy it replaces.
    let mut replaced: Vec<(usize, usize)> = Vec::new();
    for (s, to) in recreate {
        let item = &source[s];
        if let Some(&copy) = copy_of.get(&s) {
            replaced.push((copy, actions.len()));
        }
        actions.push(Action::Recreate {
            source: item.key.clone(),
            to,
            read: item.read,
            sent: item.sent,
            received: item.received,
            subject: item.subject.clone(),
        });
    }
    for (copy, to, reason, requires) in moves {
        actions.push(Action::Move {
            item: target[copy].key.clone(),
            to,
            reason: reason.into(),
            requires,
        });
    }
    for (copy, requires) in replaced {
        actions.push(Action::Move {
            item: target[copy].key.clone(),
            to: quarantine.clone(),
            reason: "damaged".into(),
            requires: Some(requires),
        });
    }
    for (index, role) in audit.target_roles.iter().enumerate() {
        if !at_top(index) {
            continue;
        }
        let (to, reason) = match role {
            TargetRole::Duplicate { .. } => (&quarantine, "duplicate"),
            TargetRole::Unmatched if target[index].folder.is_empty() => (&unmatched, "unmatched"),
            _ => continue,
        };
        actions.push(Action::Move {
            item: target[index].key.clone(),
            to: to.clone(),
            reason: reason.into(),
            requires: None,
        });
    }
    actions
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::audit;

    fn message(key: &str, folder: &str, id: &str, native: u8) -> ItemRecord {
        ItemRecord {
            key: key.into(),
            folder: folder.into(),
            class: "IPM.Note".into(),
            message_id: Some(id.into()),
            subject: Some(id.into()),
            sent: Some(100),
            received: Some(100),
            native_body: Some(native),
            read: Some(true),
            ..Default::default()
        }
    }

    fn folders() -> FolderMap {
        FolderMap {
            well_known: [
                ("inbox", "Boîte de réception"),
                ("sentitems", "Éléments envoyés"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        }
    }

    #[test]
    fn maps_well_known_folders_and_keeps_other_names() {
        let map = folders();
        assert_eq!(map.target("Inbox"), "Boîte de réception");
        assert_eq!(
            map.target(&path::join("Sent Items", "2019")),
            "Éléments envoyés/2019"
        );
        let factures = path::join("Archive", "Commandes / Factures");
        assert_eq!(map.target(&factures), factures);
    }

    #[test]
    fn plans_folders_recreations_moves_and_quarantines_in_order() {
        let source = vec![
            message("ok", "Inbox", "<1>", 3),
            message("moved", &path::join("Archive", "Banque"), "<2>", 3),
            message("damaged", "Sent Items", "<3>", 3),
            message("missing", "Inbox", "<4>", 3),
        ];
        let target = vec![
            message("t-ok", "Boîte de réception", "<1>", 3),
            message("t-moved", "", "<2>", 3),
            message("t-dup", "", "<2>", 3),
            message("t-damaged", "", "<3>", 1),
            message("t-debris", "", "<other>", 1),
            message("t-new", "Boîte de réception", "<new>", 3),
        ];
        let mut source_ok = source.clone();
        source_ok[0].folder = "Boîte de réception".into();
        let audit = audit(&source_ok, &target, 1000);
        let existing: BTreeSet<String> = ["Boîte de réception", "Éléments envoyés", "Archive"]
            .into_iter()
            .map(String::from)
            .collect();

        let actions = plan(
            &audit,
            &source,
            &target,
            &folders(),
            &existing,
            &HashSet::new(),
            "run1",
        );

        let quarantine = "ecm-quarantine/run1".to_owned();
        assert_eq!(
            actions[..4],
            [
                Action::CreateFolder {
                    path: "ecm-quarantine".into()
                },
                Action::CreateFolder {
                    path: "ecm-unmatched".into()
                },
                Action::CreateFolder {
                    path: "Archive/Banque".into()
                },
                Action::CreateFolder {
                    path: quarantine.clone()
                },
            ]
        );
        assert!(
            matches!(&actions[4], Action::Recreate { source, to, .. } if source == "damaged" && to == "Éléments envoyés")
        );
        assert!(
            matches!(&actions[5], Action::Recreate { source, to, .. } if source == "missing" && to == "Boîte de réception")
        );
        assert!(
            matches!(&actions[6], Action::Move { item, to, reason, .. } if item == "t-moved" && to == "Archive/Banque" && reason == "misplaced")
        );
        assert_eq!(
            actions[7],
            Action::Move {
                item: "t-damaged".into(),
                to: quarantine.clone(),
                reason: "damaged".into(),
                requires: Some(4)
            }
        );
        assert!(
            matches!(&actions[8], Action::Move { item, reason, .. } if item == "t-dup" && reason == "duplicate")
        );
        assert!(
            matches!(&actions[9], Action::Move { item, to, reason, .. } if item == "t-debris" && to == "ecm-unmatched" && reason == "unmatched")
        );
        assert_eq!(
            actions.len(),
            10,
            "the healthy item and new mail are left alone"
        );
        assert_eq!(
            counts(&actions),
            [
                ("create-folder", 4),
                ("recreate", 2),
                ("move", 1),
                ("quarantine", 3)
            ]
        );
    }

    #[test]
    fn leaves_items_outside_the_top_and_system_folders_alone() {
        let source = vec![
            message("filed", "Inbox", "<1>", 3),
            message(
                "system",
                &path::join("Recoverable Items", "Calendar Logging"),
                "<2>",
                3,
            ),
        ];
        let target = vec![
            message("t-filed", "Éléments supprimés", "<1>", 1),
            message("t-test-dup", "ecm-test", "<1>", 1),
        ];
        let audit = audit(&source, &target, 1000);
        let existing: BTreeSet<String> = ["ecm-quarantine", "ecm-unmatched", "ecm-quarantine/r"]
            .into_iter()
            .map(String::from)
            .collect();
        let actions = plan(
            &audit,
            &source,
            &target,
            &folders(),
            &existing,
            &HashSet::new(),
            "r",
        );
        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn replaces_a_migration_copy_an_earlier_apply_moved() {
        let source = vec![message("s", "Inbox", "<1>", 3)];
        // In the right folder and looking healthy: still a migration copy.
        let emptied = message("t-moved", "Boîte de réception", "<1>", 3);
        let mut mapped = source.clone();
        mapped[0].folder = "Boîte de réception".into();
        let audit = audit(&mapped, std::slice::from_ref(&emptied), 1000);
        assert_eq!(audit.source_status, [SourceStatus::Ok]);
        let existing: BTreeSet<String> = [
            "Boîte de réception",
            "ecm-quarantine",
            "ecm-quarantine/r",
            "ecm-unmatched",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let untouched = plan(
            &audit,
            &mapped,
            std::slice::from_ref(&emptied),
            &folders(),
            &existing,
            &HashSet::new(),
            "r",
        );
        assert!(untouched.is_empty(), "{untouched:?}");
        let moved: HashSet<String> = ["t-moved".to_owned()].into();
        let actions = plan(
            &audit,
            &mapped,
            &[emptied],
            &folders(),
            &existing,
            &moved,
            "r",
        );
        assert_eq!(counts(&actions), [("recreate", 1), ("quarantine", 1)]);
        assert!(
            matches!(&actions[1], Action::Move { requires: Some(0), reason, .. } if reason == "damaged")
        );
    }

    #[test]
    fn actions_round_trip_as_json() {
        let action = Action::Move {
            item: "x".into(),
            to: "y".into(),
            reason: "duplicate".into(),
            requires: None,
        };
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(
            json,
            r#"{"action":"move","item":"x","to":"y","reason":"duplicate"}"#
        );
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
    }
}
