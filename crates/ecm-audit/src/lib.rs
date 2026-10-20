//! Matching between source and target items, defect detection and reports.
//!
//! Each source item is matched with at most one target copy, its best one; the
//! other copies of the same item are duplicates. Matching uses the `Message-ID`
//! first, then a fingerprint (subject and send time) for copies whose `Message-ID`
//! was lost or rewritten.

#![forbid(unsafe_code)]

pub mod plan;
pub mod verify;

use std::collections::HashMap;

use ecm_model::ItemRecord;

/// Two delivery times this far apart, in seconds, are different.
const TIME_TOLERANCE: i64 = 120;

/// What is wrong with a target copy, compared with its source item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Defect {
    /// Not in the same folder as the source item.
    WrongFolder,
    /// Received time differs: typically reset to the migration date.
    DateChanged,
    /// The source has a sender, the copy has none.
    SenderLost,
    /// The source has Internet headers, the copy has none.
    HeadersLost,
    /// An HTML body became plain text. Other format changes are not damage: MIME
    /// has no RTF, so an RTF body comes back as HTML or text.
    BodyFormatChanged,
    /// The source item has a body, the copy's is empty.
    ContentLost,
    /// Matched by fingerprint: the `Message-ID` is missing or different.
    MessageIdChanged,
    /// Read state differs. Not damage, but worth restoring.
    ReadStateChanged,
}

impl Defect {
    /// Whether the copy lost content or metadata a move cannot restore, so it must
    /// be replaced by the source item.
    pub fn is_damage(self) -> bool {
        !matches!(self, Defect::WrongFolder | Defect::ReadStateChanged)
    }

    /// Short name for reports.
    pub fn name(self) -> &'static str {
        match self {
            Defect::WrongFolder => "wrong-folder",
            Defect::DateChanged => "date-changed",
            Defect::SenderLost => "sender-lost",
            Defect::HeadersLost => "headers-lost",
            Defect::BodyFormatChanged => "body-format-changed",
            Defect::ContentLost => "content-lost",
            Defect::MessageIdChanged => "message-id-changed",
            Defect::ReadStateChanged => "read-state-changed",
        }
    }
}

/// Status of a source item in the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceStatus {
    /// A healthy copy is in the right folder.
    Ok,
    /// A healthy copy exists, in another folder: move it.
    Misplaced,
    /// Only damaged copies exist: replace with the source item.
    Degraded,
    /// No copy: upload the source item.
    Missing,
}

impl SourceStatus {
    /// Short name for reports.
    pub fn name(self) -> &'static str {
        match self {
            SourceStatus::Ok => "ok",
            SourceStatus::Misplaced => "misplaced",
            SourceStatus::Degraded => "degraded",
            SourceStatus::Missing => "missing",
        }
    }
}

/// What a target item is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetRole {
    /// The copy chosen for this source item (index in the source inventory).
    Copy {
        /// Index of the source item.
        source: usize,
        /// What is wrong with it.
        defects: Vec<Defect>,
    },
    /// An extra copy of this source item.
    Duplicate {
        /// Index of the source item.
        source: usize,
    },
    /// No source item, received after the cutoff into a folder: new mail to keep.
    New,
    /// No source item, received before the cutoff: to review.
    Unmatched,
}

/// The audit of a target against a source.
#[derive(Debug, Clone)]
pub struct Audit {
    /// Status of each source item, in source inventory order.
    pub source_status: Vec<SourceStatus>,
    /// Number of target copies matched to each source item, its chosen copy
    /// included.
    pub source_copies: Vec<usize>,
    /// Role of each target item, in target inventory order.
    pub target_roles: Vec<TargetRole>,
}

impl Audit {
    /// Source items per status.
    pub fn source_counts(&self) -> HashMap<SourceStatus, usize> {
        let mut counts = HashMap::new();
        for status in &self.source_status {
            *counts.entry(*status).or_default() += 1;
        }
        counts
    }

    /// Number of target items that are duplicates.
    pub fn duplicates(&self) -> usize {
        self.target_roles
            .iter()
            .filter(|role| matches!(role, TargetRole::Duplicate { .. }))
            .count()
    }

    /// Chosen copies per defect.
    pub fn defect_counts(&self) -> HashMap<Defect, usize> {
        let mut counts = HashMap::new();
        for role in &self.target_roles {
            if let TargetRole::Copy { defects, .. } = role {
                for defect in defects {
                    *counts.entry(*defect).or_default() += 1;
                }
            }
        }
        counts
    }
}

/// Compares a target copy with its source item.
pub fn defects(source: &ItemRecord, copy: &ItemRecord, by_fingerprint: bool) -> Vec<Defect> {
    let mut defects = Vec::new();
    if source.folder.to_lowercase() != copy.folder.to_lowercase() {
        defects.push(Defect::WrongFolder);
    }
    if let (Some(a), Some(b)) = (source.received, copy.received)
        && (a - b).abs() > TIME_TOLERANCE
    {
        defects.push(Defect::DateChanged);
    }
    if source.sender.is_some() && copy.sender.is_none() {
        defects.push(Defect::SenderLost);
    }
    if source.has_headers == Some(true) && copy.has_headers == Some(false) {
        defects.push(Defect::HeadersLost);
    }
    if source.native_body == Some(3) && copy.native_body == Some(1) {
        defects.push(Defect::BodyFormatChanged);
    }
    if source.body_empty == Some(false) && copy.body_empty == Some(true) {
        defects.push(Defect::ContentLost);
    }
    if by_fingerprint {
        defects.push(Defect::MessageIdChanged);
    }
    if let (Some(a), Some(b)) = (source.read, copy.read)
        && a != b
    {
        defects.push(Defect::ReadStateChanged);
    }
    defects
}

/// How good a copy is, to pick the best of several: undamaged first, then in the
/// right folder, then fewest defects.
fn score(defects: &[Defect]) -> (bool, bool, usize) {
    (
        defects.iter().any(|d| d.is_damage()),
        defects.contains(&Defect::WrongFolder),
        defects.len(),
    )
}

/// The key items are matched on: the `Message-ID` for messages, else a
/// fingerprint of class, subject and send time.
fn message_key(item: &ItemRecord) -> Option<String> {
    item.message_id.clone().filter(|_| item.is_message())
}

fn fallback_key(item: &ItemRecord) -> String {
    let subject = ecm_model::item::normalize_subject(item.subject.as_deref().unwrap_or(""));
    match item.fingerprint() {
        Some(fingerprint) if item.is_message() => format!("msg|{fingerprint}"),
        _ => format!("{}|{subject}", item.class.to_lowercase()),
    }
}

/// Audits `target` against `source`. Target items received at or after `cutoff`
/// (Unix seconds) without a source match are new mail.
pub fn audit(source: &[ItemRecord], target: &[ItemRecord], cutoff: i64) -> Audit {
    // Candidates for each source item: (target index, by fingerprint).
    let mut candidates: Vec<Vec<(usize, bool)>> = vec![Vec::new(); source.len()];
    let mut matched = vec![false; target.len()];

    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_fallback: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, item) in source.iter().enumerate() {
        if let Some(id) = item.message_id.as_deref().filter(|_| item.is_message()) {
            by_id.entry(id).or_default().push(index);
        }
        by_fallback
            .entry(fallback_key(item))
            .or_default()
            .push(index);
    }

    // Distributes target copies over the source items sharing their key: each
    // source item gets its share, in order, so that a message filed twice in the
    // source keeps two copies. Items plans rebuild come first, so that a twin in
    // a system folder (Recoverable Items) never takes their copy; then, between
    // equal shares, the item the copy matches best: undamaged, then in its folder
    // (twins can differ, received days apart). Otherwise a plan would re-create an
    // item next to its own copy, or replace a sound copy.
    let assign = |sources: &[usize],
                  copy: usize,
                  by_fingerprint: bool,
                  candidates: &mut Vec<Vec<(usize, bool)>>| {
        let chosen = *sources
            .iter()
            .min_by_key(|&&s| {
                (
                    !verify::in_scope(&source[s]),
                    candidates[s].len(),
                    score(&defects(&source[s], &target[copy], by_fingerprint)),
                )
            })
            .expect("keys always have a source item");
        candidates[chosen].push((copy, by_fingerprint));
    };
    for (copy, item) in target.iter().enumerate() {
        if let Some(sources) = message_key(item).and_then(|id| by_id.get(id.as_str())) {
            assign(sources, copy, false, &mut candidates);
            matched[copy] = true;
        }
    }
    for (copy, item) in target.iter().enumerate() {
        if matched[copy] {
            continue;
        }
        if let Some(sources) = by_fallback.get(&fallback_key(item)) {
            assign(
                sources,
                copy,
                item.message_id.is_some() || item.is_message(),
                &mut candidates,
            );
            matched[copy] = true;
        }
    }

    let mut target_roles: Vec<TargetRole> = target
        .iter()
        .map(|item| {
            // New mail is delivered into a folder, never into the top of the
            // mailbox, where only a migration puts items.
            if !item.folder.is_empty() && item.received.is_some_and(|received| received >= cutoff) {
                TargetRole::New
            } else {
                TargetRole::Unmatched
            }
        })
        .collect();
    let mut source_status = Vec::with_capacity(source.len());
    let mut source_copies = Vec::with_capacity(source.len());
    for (index, item) in source.iter().enumerate() {
        let mut assessed: Vec<(usize, Vec<Defect>)> = candidates[index]
            .iter()
            .map(|&(copy, by_fingerprint)| (copy, defects(item, &target[copy], by_fingerprint)))
            .collect();
        assessed.sort_by_key(|(copy, defects)| (score(defects), *copy));
        source_copies.push(assessed.len());
        let mut copies = assessed.into_iter();
        let status = match copies.next() {
            None => SourceStatus::Missing,
            Some((copy, defects)) => {
                let status = match score(&defects) {
                    (true, _, _) => SourceStatus::Degraded,
                    (false, true, _) => SourceStatus::Misplaced,
                    (false, false, _) => SourceStatus::Ok,
                };
                target_roles[copy] = TargetRole::Copy {
                    source: index,
                    defects,
                };
                status
            }
        };
        for (copy, _) in copies {
            target_roles[copy] = TargetRole::Duplicate { source: index };
        }
        source_status.push(status);
    }

    Audit {
        source_status,
        source_copies,
        target_roles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(key: &str, folder: &str, id: &str, received: i64) -> ItemRecord {
        ItemRecord {
            key: key.into(),
            folder: folder.into(),
            class: "IPM.Note".into(),
            message_id: Some(id.into()),
            subject: Some(format!("Subject {id}")),
            sender: Some("a@example.test".into()),
            sent: Some(received - 5),
            received: Some(received),
            has_headers: Some(true),
            read: Some(true),
            ..Default::default()
        }
    }

    const CUTOFF: i64 = 1_000_000;

    #[test]
    fn classifies_ok_misplaced_degraded_missing_and_duplicates() {
        let source = vec![
            message("s1", "Inbox", "<1>", 100),
            message("s2", "Archive", "<2>", 200),
            message("s3", "Inbox", "<3>", 300),
            message("s4", "Inbox", "<4>", 400),
        ];
        let mut broken = message("t5", "", "<3>", CUTOFF - 10);
        broken.sender = None;
        broken.has_headers = Some(false);
        let target = vec![
            message("t1", "Inbox", "<1>", 100),
            message("t2", "", "<2>", 200),
            message("t3", "", "<2>", 200),
            message("t4", "", "<2>", 200),
            broken,
            message("t6", "Inbox", "<new>", CUTOFF + 10),
            message("t7", "Inbox", "<old>", 50),
        ];

        let audit = audit(&source, &target, CUTOFF);

        assert_eq!(
            audit.source_status,
            [
                SourceStatus::Ok,
                SourceStatus::Misplaced,
                SourceStatus::Degraded,
                SourceStatus::Missing
            ]
        );
        assert_eq!(audit.source_copies, [1, 3, 1, 0]);
        assert_eq!(audit.duplicates(), 2);
        assert!(
            matches!(&audit.target_roles[4], TargetRole::Copy { defects, .. }
            if defects == &[Defect::WrongFolder, Defect::DateChanged, Defect::SenderLost, Defect::HeadersLost])
        );
        assert_eq!(audit.target_roles[5], TargetRole::New);
        assert_eq!(audit.target_roles[6], TargetRole::Unmatched);
    }

    #[test]
    fn migration_debris_at_the_top_is_not_new_mail() {
        let debris = message("t1", "", "<x>", CUTOFF + 10);
        let audit = audit(&[], &[debris], CUTOFF);
        assert_eq!(audit.target_roles[0], TargetRole::Unmatched);
    }

    #[test]
    fn the_healthy_copy_wins_over_a_damaged_one() {
        let source = vec![message("s1", "Inbox", "<1>", 100)];
        let mut damaged = message("t1", "Inbox", "<1>", CUTOFF);
        damaged.sender = None;
        let target = vec![damaged, message("t2", "", "<1>", 100)];
        let audit = audit(&source, &target, CUTOFF);
        assert_eq!(audit.source_status, [SourceStatus::Misplaced]);
        assert_eq!(audit.target_roles[0], TargetRole::Duplicate { source: 0 });
    }

    #[test]
    fn a_rewritten_message_id_still_matches_by_fingerprint() {
        let source = vec![message("s1", "Sent", "<1>", 1000)];
        let mut copy = message("t1", "", "<generated@migration>", CUTOFF + 5);
        copy.subject = source[0].subject.clone();
        copy.sent = Some(995);
        let audit = audit(&source, &[copy], CUTOFF);
        assert_eq!(audit.source_status, [SourceStatus::Degraded]);
        assert!(
            matches!(&audit.target_roles[0], TargetRole::Copy { defects, .. }
            if defects.contains(&Defect::MessageIdChanged) && defects.contains(&Defect::DateChanged))
        );
    }

    #[test]
    fn a_message_filed_twice_in_the_source_keeps_two_copies() {
        let source = vec![
            message("s1", "Inbox", "<1>", 100),
            message("s2", "Archive", "<1>", 100),
        ];
        let target = vec![message("t1", "", "<1>", 100), message("t2", "", "<1>", 100)];
        let audit = audit(&source, &target, CUTOFF);
        assert_eq!(audit.source_copies, [1, 1]);
        assert_eq!(audit.duplicates(), 0);
    }

    #[test]
    fn a_system_folder_twin_does_not_take_the_copy_of_a_rebuilt_item() {
        // Sent twice, and kept once more in Recoverable Items: the two copies are
        // the two sent items'.
        let source = vec![
            message(
                "kept",
                &ecm_model::path::join("Recoverable Items", "Purges"),
                "<1>",
                100,
            ),
            message("s1", "Sent Items", "<1>", 100),
            message("s2", "Sent Items", "<1>", 100),
        ];
        let target = vec![
            message("t1", "Sent Items", "<1>", 100),
            message("t2", "Sent Items", "<1>", 100),
        ];
        let audit = audit(&source, &target, CUTOFF);
        assert_eq!(audit.source_copies, [0, 1, 1]);
        assert_eq!(
            audit.source_status[1..],
            [SourceStatus::Ok, SourceStatus::Ok]
        );

        // Migration debris at the top goes to a rebuilt item too.
        let audit = super::audit(&source, &[message("t1", "", "<1>", 100)], CUTOFF);
        assert_eq!(audit.source_copies, [0, 1, 0]);
    }

    #[test]
    fn between_equal_shares_a_copy_goes_to_the_item_in_its_folder() {
        // Sent to oneself: in the inbox and in sent items.
        let source = vec![
            message("s1", "Inbox", "<1>", 100),
            message("s2", "Sent Items", "<1>", 100),
        ];
        let audit = audit(&source, &[message("t1", "Sent Items", "<1>", 100)], CUTOFF);
        assert_eq!(
            audit.source_status,
            [SourceStatus::Missing, SourceStatus::Ok]
        );
    }

    #[test]
    fn an_emptied_body_is_lost_content() {
        let mut source = message("s1", "Inbox", "<1>", 100);
        source.body_empty = Some(false);
        let mut emptied = message("t1", "Inbox", "<1>", 100);
        emptied.body_empty = Some(true);
        assert_eq!(defects(&source, &emptied, false), [Defect::ContentLost]);
        // Unknown on either side, or empty in the source too: nothing lost.
        assert!(defects(&message("s", "Inbox", "<1>", 100), &emptied, false).is_empty());
        source.body_empty = Some(true);
        assert!(defects(&source, &emptied, false).is_empty());
    }

    #[test]
    fn contacts_match_by_class_and_name() {
        let contact = |key: &str| ItemRecord {
            key: key.into(),
            class: "IPM.Contact".into(),
            subject: Some("Kevin".into()),
            ..Default::default()
        };
        let audit = audit(&[contact("s1")], &[contact("t1")], CUTOFF);
        assert_eq!(audit.source_status, [SourceStatus::Ok]);
    }
}
