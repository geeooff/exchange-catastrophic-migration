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

use crate::{Audit, Defect, SourceStatus, TargetRole};

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
    /// Create a message in an Outlook.com mailbox (Microsoft Graph) from its
    /// JSON resource, which carries the properties only creation sets; its files
    /// follow as `attach` steps.
    CreateMessage {
        /// Target folder path.
        to: String,
        /// Subject, for reports.
        subject: Option<String>,
        /// The message, as Graph JSON.
        message: String,
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
    /// Move a quarantine run folder into Deleted Items, for the user to empty.
    DiscardFolder {
        /// Folder path, under the quarantine folder.
        path: String,
        /// Items it holds, for reports.
        items: u64,
    },
    /// Create an inbox rule from its EWS XML (`<t:Rule>`).
    CreateRule {
        /// Rule name, for reports.
        name: String,
        /// The rule, as EWS XML.
        rule: String,
    },
    /// Attach a file to the item an earlier step created: a message too large
    /// for one request is created without its files, then given them one by one.
    Attach {
        /// The step that created the item, or attached its previous file; its
        /// journal holds the item id.
        requires: usize,
        /// File name.
        name: String,
        /// MIME type.
        content_type: String,
        /// `Content-ID`, for a file the HTML body shows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_id: Option<String>,
        /// The file, base64.
        data: String,
    },
    /// Create a calendar item, contact, To Do task, inbox rule or category in an
    /// Outlook.com mailbox (Microsoft Graph) from its JSON resource.
    CreateGraph {
        /// `event`, `contact`, `task`, `rule` or `category`.
        kind: String,
        /// The calendar, contact folder or To Do list, by name: empty for the
        /// default one, and for rules and categories.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        to: String,
        /// Subject or name, for reports.
        subject: Option<String>,
        /// The item, as Graph JSON. A rule names its folders by path, after
        /// `ecm-folder:`.
        item: String,
    },
    /// Set the photo of the contact an earlier `create-graph` step created in an
    /// Outlook.com mailbox.
    SetPhoto {
        /// The step that created the contact; its journal holds the contact id.
        requires: usize,
        /// The contact folder, by name: empty for the default one.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        to: String,
        /// Contact name, for reports.
        subject: Option<String>,
        /// The photo, base64.
        photo: String,
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
            Action::Recreate { .. }
            | Action::RecreateMime { .. }
            | Action::CreateMessage { .. } => "recreate",
            Action::Move { reason, .. } => match reason.as_str() {
                "misplaced" => "move",
                _ => "quarantine",
            },
            Action::Create { kind, .. } | Action::CreateGraph { kind, .. } => match kind.as_str() {
                "event" => "create-event",
                "contact" => "create-contact",
                "task" => "create-task",
                "note" => "create-note",
                "rule" => "create-rule",
                "category" => "create-category",
                _ => "create",
            },
            Action::ContactPhoto { .. } | Action::SetPhoto { .. } => "contact-photo",
            Action::Attach { .. } => "attach",
            Action::CreateRule { .. } => "create-rule",
            Action::DiscardFolder { .. } => "discard-folder",
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

/// Target items earlier applied plans put where they are, by id (from their
/// journals).
#[derive(Debug, Clone, Default)]
pub struct Applied {
    /// Migration copies a plan moved into folders. They are never trusted, even
    /// when their properties look right (bodies emptied, attachments with every
    /// LF turned into CRLF): each is re-created from its source item, then
    /// quarantined.
    pub moved: HashSet<String>,
    /// Copies a plan created from the same source. Trusted while healthy; a
    /// degraded one is re-created from its source item, then quarantined, unless
    /// another copy would stay next to the new one; an extra one is quarantined.
    pub created: HashSet<String>,
}

/// Builds the plan for an audit of `target` against `source`.
///
/// `existing` holds the target's folder paths. `run` names the quarantine
/// sub-folder. Only messages are re-created: other source items are left out.
///
/// Only target items at the top of the mailbox, where the migration left its
/// copies, and items earlier plans put where they are (`applied`) are moved.
/// Items in any other folder were delivered or filed there and are left alone,
/// and so are the source items they serve.
pub fn plan(
    audit: &Audit,
    source: &[ItemRecord],
    target: &[ItemRecord],
    folders: &FolderMap,
    existing: &BTreeSet<String>,
    applied: &Applied,
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
    // Items a plan may move: migration debris at the top, migration copies an
    // earlier `apply` moved, and copies it created, never mail filed by the user.
    let to_replace = |copy: usize| applied.moved.contains(target[copy].key.as_str());
    let created = |copy: usize| applied.created.contains(target[copy].key.as_str());
    let at_top = |copy: usize| target[copy].folder.is_empty() || to_replace(copy);
    // Source items with an extra copy no plan may move: replacing the copy an
    // earlier plan created would leave that one next to the new one.
    let mut kept: HashSet<usize> = HashSet::new();
    for (index, role) in audit.target_roles.iter().enumerate() {
        if let TargetRole::Duplicate { source: s } = role
            && !at_top(index)
            && !created(index)
        {
            kept.insert(*s);
        }
    }
    for (s, (item, status)) in source.iter().zip(&audit.source_status).enumerate() {
        let skipped_folder = path::split(&item.folder)
            .first()
            .is_some_and(|first| SKIPPED_SOURCE_FOLDERS.contains(&first.to_lowercase().as_str()));
        if !item.is_message() || skipped_folder {
            continue;
        }
        // A copy matched only by fingerprint (another Message-ID) may be another
        // message: one a plan created is then left alone.
        let by_fingerprint = |copy: usize| {
            matches!(&audit.target_roles[copy], TargetRole::Copy { defects, .. }
                if defects.contains(&Defect::MessageIdChanged))
        };
        let movable = |copy: usize| {
            at_top(copy)
                || created(copy)
                    && *status == SourceStatus::Degraded
                    && !kept.contains(&s)
                    && !by_fingerprint(copy)
        };
        if copy_of.get(&s).is_some_and(|&copy| !movable(copy)) {
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
    let mut recreated: HashMap<usize, usize> = HashMap::new();
    for (s, to) in recreate {
        let item = &source[s];
        if let Some(&copy) = copy_of.get(&s) {
            replaced.push((copy, actions.len()));
        }
        recreated.insert(s, actions.len());
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
        let (to, reason, requires) = match role {
            TargetRole::Duplicate { .. } if at_top(index) => (&quarantine, "duplicate", None),
            // An extra copy an earlier plan made, once its source item keeps a
            // healthy copy or is re-created.
            TargetRole::Duplicate { source: s } if created(index) => match recreated.get(s) {
                Some(&step) => (&quarantine, "duplicate", Some(step)),
                None if crate::verify::in_scope(&source[*s])
                    && matches!(
                        audit.source_status[*s],
                        SourceStatus::Ok | SourceStatus::Misplaced
                    ) =>
                {
                    (&quarantine, "duplicate", None)
                }
                None => continue,
            },
            TargetRole::Unmatched if target[index].folder.is_empty() => {
                (&unmatched, "unmatched", None)
            }
            _ => continue,
        };
        actions.push(Action::Move {
            item: target[index].key.clone(),
            to: to.clone(),
            reason: reason.into(),
            requires,
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
            &Applied::default(),
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
            &Applied::default(),
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
            &Applied::default(),
            "r",
        );
        assert!(untouched.is_empty(), "{untouched:?}");
        let applied = Applied {
            moved: ids(&["t-moved"]),
            ..Default::default()
        };
        let actions = plan(
            &audit,
            &mapped,
            &[emptied],
            &folders(),
            &existing,
            &applied,
            "r",
        );
        assert_eq!(counts(&actions), [("recreate", 1), ("quarantine", 1)]);
        assert!(
            matches!(&actions[1], Action::Move { requires: Some(0), reason, .. } if reason == "damaged")
        );
    }

    fn ids(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|&key| key.to_owned()).collect()
    }

    fn existing() -> BTreeSet<String> {
        [
            "Boîte de réception",
            "Éléments envoyés",
            "ecm-quarantine",
            "ecm-quarantine/r",
            "ecm-unmatched",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    #[test]
    fn replaces_a_degraded_copy_an_earlier_apply_created() {
        let inbox = "Boîte de réception";
        let source = vec![
            message("s1", inbox, "<1>", 3),
            message("s2", inbox, "<2>", 3),
            message("s3", inbox, "<3>", 3),
        ];
        // HTML turned into text: degraded.
        let created = message("t-created", inbox, "<1>", 1);
        let mut again = message("t-again", inbox, "<1>", 1);
        again.read = Some(false);
        let filed = message("t-filed", inbox, "<2>", 1);
        // Replacing this created copy would leave the filed one next to the new
        // one.
        let beside = message("t-beside", inbox, "<3>", 1);
        let mut filed_too = message("t-filed-too", inbox, "<3>", 1);
        filed_too.read = Some(false);
        let target = vec![created, again, filed, beside, filed_too];
        let audit = audit(&source, &target, 1000);
        assert_eq!(audit.source_status, [SourceStatus::Degraded; 3]);

        // Copies ecm did not put there are never touched.
        let untouched = plan(
            &audit,
            &source,
            &target,
            &folders(),
            &existing(),
            &Applied::default(),
            "r",
        );
        assert!(untouched.is_empty(), "{untouched:?}");

        let applied = Applied {
            created: ids(&["t-created", "t-again", "t-beside"]),
            ..Default::default()
        };
        let actions = plan(
            &audit,
            &source,
            &target,
            &folders(),
            &existing(),
            &applied,
            "r",
        );
        assert!(
            matches!(&actions[0], Action::Recreate { source, to, .. } if source == "s1" && to == inbox)
        );
        let quarantine = "ecm-quarantine/r".to_owned();
        assert_eq!(
            actions[1..],
            [
                Action::Move {
                    item: "t-created".into(),
                    to: quarantine.clone(),
                    reason: "damaged".into(),
                    requires: Some(0),
                },
                Action::Move {
                    item: "t-again".into(),
                    to: quarantine,
                    reason: "duplicate".into(),
                    requires: Some(0),
                },
            ]
        );
    }

    #[test]
    fn keeps_a_healthy_copy_an_earlier_apply_created_and_sets_its_extras_aside() {
        let inbox = "Boîte de réception";
        let source = vec![message("s1", inbox, "<1>", 3)];
        let target = vec![
            message("t-created", inbox, "<1>", 3),
            message("t-again", inbox, "<1>", 3),
            message("t-filed", inbox, "<1>", 3),
        ];
        let audit = audit(&source, &target, 1000);
        assert_eq!(audit.source_status, [SourceStatus::Ok]);
        let applied = Applied {
            created: ids(&["t-created", "t-again"]),
            ..Default::default()
        };
        let actions = plan(
            &audit,
            &source,
            &target,
            &folders(),
            &existing(),
            &applied,
            "r",
        );
        assert_eq!(
            actions,
            [Action::Move {
                item: "t-again".into(),
                to: "ecm-quarantine/r".into(),
                reason: "duplicate".into(),
                requires: None,
            }]
        );
    }

    #[test]
    fn does_not_recreate_a_sent_item_whose_twin_is_in_recoverable_items() {
        // Sent twice, and kept once more in Recoverable Items; an earlier plan
        // created both sent copies.
        let sent = "Éléments envoyés";
        let source = vec![
            message("kept", &path::join("Recoverable Items", "Purges"), "<1>", 3),
            message("s1", sent, "<1>", 3),
            message("s2", sent, "<1>", 3),
        ];
        let target = vec![message("t1", sent, "<1>", 3), message("t2", sent, "<1>", 3)];
        let audit = audit(&source, &target, 1000);
        let applied = Applied {
            created: ids(&["t1", "t2"]),
            ..Default::default()
        };
        let actions = plan(
            &audit,
            &source,
            &target,
            &folders(),
            &existing(),
            &applied,
            "r",
        );
        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn a_created_copy_goes_to_the_twin_it_matches_and_stays() {
        // Filed twice in the source, received days apart; one copy, an earlier
        // plan's, received as the second twin: it is that twin's, sound, and
        // only the first twin is re-created.
        let folder = "Archive";
        let mut first = message("s1", folder, "<1>", 2);
        first.received = Some(400_000);
        let second = message("s2", folder, "<1>", 2);
        let target = vec![message("t1", folder, "<1>", 3)];
        let source = vec![first, second];
        let audit = audit(&source, &target, 1_000_000);
        assert_eq!(
            audit.source_status,
            [SourceStatus::Missing, SourceStatus::Ok]
        );
        let applied = Applied {
            created: ids(&["t1"]),
            ..Default::default()
        };
        let actions = plan(
            &audit,
            &source,
            &target,
            &folders(),
            &existing(),
            &applied,
            "r",
        );
        let steps: Vec<&Action> = actions
            .iter()
            .filter(|a| !matches!(a, Action::CreateFolder { .. }))
            .collect();
        assert!(
            matches!(&steps[..], [Action::Recreate { source, .. }] if source == "s1"),
            "{actions:?}"
        );
    }

    #[test]
    fn a_created_copy_matched_only_by_fingerprint_is_left_alone() {
        // Same subject and send time, another Message-ID: maybe another message.
        let sent = "Éléments envoyés";
        let source = vec![message("s1", sent, "<1>", 2)];
        let mut copy = message("t1", sent, "<other>", 2);
        copy.subject = Some("<1>".into());
        let target = vec![copy];
        let audit = audit(&source, &target, 1000);
        assert_eq!(audit.source_status, [SourceStatus::Degraded]);
        let applied = Applied {
            created: ids(&["t1"]),
            ..Default::default()
        };
        let actions = plan(
            &audit,
            &source,
            &target,
            &folders(),
            &existing(),
            &applied,
            "r",
        );
        assert!(actions.is_empty(), "{actions:?}");
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

        let attach = Action::Attach {
            requires: 3,
            name: "a.mp4".into(),
            content_type: "video/mp4".into(),
            content_id: None,
            data: "eA==".into(),
        };
        let json = serde_json::to_string(&attach).unwrap();
        assert_eq!(
            json,
            r#"{"action":"attach","requires":3,"name":"a.mp4","content_type":"video/mp4","data":"eA=="}"#
        );
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), attach);

        let photo = Action::SetPhoto {
            requires: 4,
            to: String::new(),
            subject: Some("Ann".into()),
            photo: "eA==".into(),
        };
        let json = serde_json::to_string(&photo).unwrap();
        assert_eq!(
            json,
            r#"{"action":"set-photo","requires":4,"subject":"Ann","photo":"eA=="}"#
        );
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), photo);
        let rule = Action::CreateGraph {
            kind: "rule".into(),
            to: String::new(),
            subject: Some("Bank".into()),
            item: "{}".into(),
        };
        assert_eq!(rule.kind(), "create-rule");
        let json = serde_json::to_string(&rule).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), rule);
    }
}
