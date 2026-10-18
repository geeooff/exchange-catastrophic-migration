//! Verification after `apply`: a second audit of the target, where only live
//! folders count, and a check that nothing parked was the only copy of an item.

use ecm_model::{ItemRecord, path};

use crate::{
    Audit, SourceStatus, TargetRole, audit,
    plan::{QUARANTINE_FOLDER, SKIPPED_SOURCE_FOLDERS, UNMATCHED_FOLDER},
};

/// Top-level folders whose items are set aside, not part of the repaired
/// mailbox (compared lower-cased).
pub const PARKING_FOLDERS: &[&str] = &[QUARANTINE_FOLDER, UNMATCHED_FOLDER, "ecm-test"];

/// Whether a target folder path is under a parking folder.
pub fn is_parked(folder: &str) -> bool {
    path::split(folder)
        .first()
        .is_some_and(|first| PARKING_FOLDERS.contains(&first.to_lowercase().as_str()))
}

/// Whether a source item is one a plan rebuilds: a message outside system folders.
pub fn in_scope(item: &ItemRecord) -> bool {
    item.is_message()
        && !path::split(&item.folder)
            .first()
            .is_some_and(|first| SKIPPED_SOURCE_FOLDERS.contains(&first.to_lowercase().as_str()))
}

/// The result of a verification.
#[derive(Debug, Clone)]
pub struct Verification {
    /// Indices of the live target items (outside parking folders), in order.
    pub live: Vec<usize>,
    /// Audit of the source against the live target items only. Its target roles
    /// follow `live`.
    pub audit: Audit,
    /// Parked target items (indices into the whole target) matching a source item
    /// that has no live copy: they were set aside but are still needed.
    pub parked_needed: Vec<usize>,
    /// Live target items left at the top of the mailbox.
    pub at_top: Vec<usize>,
}

impl Verification {
    /// In-scope source items per status.
    pub fn scoped_counts(&self, source: &[ItemRecord]) -> [usize; 4] {
        let mut counts = [0; 4];
        for (item, status) in source.iter().zip(&self.audit.source_status) {
            if in_scope(item) {
                counts[*status as usize] += 1;
            }
        }
        counts
    }

    /// Whether the target is a faithful copy: every in-scope source item has a
    /// healthy copy in its folder, no live duplicate, nothing left at the top and
    /// nothing needed in a parking folder.
    pub fn is_clean(&self, source: &[ItemRecord]) -> bool {
        let [_, misplaced, degraded, missing] = self.scoped_counts(source);
        misplaced + degraded + missing == 0
            && self.audit.duplicates() == 0
            && self.at_top.is_empty()
            && self.parked_needed.is_empty()
    }
}

/// Verifies `target` against `source`, whose folders are already mapped to target
/// paths. `cutoff` is as for [`audit`].
pub fn verify(source: &[ItemRecord], target: &[ItemRecord], cutoff: i64) -> Verification {
    let (live, parked): (Vec<usize>, Vec<usize>) =
        (0..target.len()).partition(|&i| !is_parked(&target[i].folder));
    let live_items: Vec<ItemRecord> = live.iter().map(|&i| target[i].clone()).collect();
    let parked_items: Vec<ItemRecord> = parked.iter().map(|&i| target[i].clone()).collect();

    let live_audit = audit(source, &live_items, cutoff);
    let parked_audit = audit(source, &parked_items, cutoff);
    let parked_needed = parked_audit
        .target_roles
        .iter()
        .zip(&parked)
        .filter_map(|(role, &index)| match role {
            TargetRole::Copy { source: s, .. } | TargetRole::Duplicate { source: s }
                if in_scope(&source[*s])
                    && live_audit.source_status[*s] == SourceStatus::Missing =>
            {
                Some(index)
            }
            _ => None,
        })
        .collect();
    let at_top = live
        .iter()
        .copied()
        .filter(|&i| target[i].folder.is_empty())
        .collect();

    Verification {
        live,
        audit: live_audit,
        parked_needed,
        at_top,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(key: &str, folder: &str, id: &str) -> ItemRecord {
        ItemRecord {
            key: key.into(),
            folder: folder.into(),
            class: "IPM.Note".into(),
            message_id: Some(id.into()),
            subject: Some(format!("Subject {id}")),
            sender: Some("a@example.test".into()),
            sent: Some(995),
            received: Some(1000),
            has_headers: Some(true),
            read: Some(true),
            native_body: Some(3),
            ..Default::default()
        }
    }

    #[test]
    fn parking_folders_are_recognized_at_the_top_only() {
        assert!(is_parked("ecm-quarantine/2026-01-01"));
        assert!(is_parked("ECM-Unmatched"));
        assert!(is_parked("ecm-test"));
        assert!(!is_parked("Archive/ecm-quarantine"));
        assert!(!is_parked(""));
    }

    #[test]
    fn a_repaired_target_is_clean() {
        let source = vec![message("s1", "Inbox", "1"), message("s2", "Archive", "2")];
        let target = vec![
            message("t1", "Inbox", "1"),
            message("t2", "Archive", "2"),
            message("q1", "ecm-quarantine/run", "1"),
            message("q2", "ecm-quarantine/run", "2"),
        ];
        let result = verify(&source, &target, 2000);
        assert_eq!(result.live, vec![0, 1]);
        assert_eq!(result.scoped_counts(&source), [2, 0, 0, 0]);
        assert!(result.is_clean(&source));
    }

    #[test]
    fn reports_leftovers_and_needed_parked_items() {
        let mut damaged = message("t1", "", "1");
        damaged.sender = None;
        let source = vec![
            message("s1", "Inbox", "1"),
            message("s2", "Archive", "2"),
            message("s3", "Inbox", "3"),
        ];
        let target = vec![
            damaged,
            message("q2", "ecm-quarantine/run", "2"),
            message("t3", "Inbox", "3"),
            message("t3b", "Inbox", "3"),
        ];
        let result = verify(&source, &target, 2000);
        let [ok, misplaced, degraded, missing] = result.scoped_counts(&source);
        assert_eq!((ok, misplaced, degraded, missing), (1, 0, 1, 1));
        assert_eq!(result.at_top, vec![0]);
        assert_eq!(result.parked_needed, vec![1]);
        assert_eq!(result.audit.duplicates(), 1);
        assert!(!result.is_clean(&source));
    }

    #[test]
    fn out_of_scope_source_items_do_not_count() {
        let mut contact = message("c", "Contacts", "c");
        contact.class = "IPM.Contact".into();
        let source = vec![contact, message("s1", "Inbox", "1")];
        let target = vec![message("t1", "Inbox", "1")];
        assert!(verify(&source, &target, 2000).is_clean(&source));
    }
}
