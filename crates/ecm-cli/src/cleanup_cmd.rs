//! `ecm report-cleanup`: sort the messages of an Outlook.com mailbox into those
//! to keep, those safe to delete because the target has them, and those to look
//! at. Only reads: the deletion itself is left to the user, in Outlook.com.

use std::{collections::BTreeMap, collections::HashSet, path::Path};

use ecm_graph::mail::Addressing;
use ecm_model::item::format_utc;
use ecm_store::{read_inventory, write_csv};

use crate::{audit_cmd::GraphArgs, verify_cmd::VERIFIED_INVENTORY};

/// What to do with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    /// It involves one of the addresses to keep.
    Keep,
    /// It involves only the moved domain, and the target has it.
    Delete,
    /// Anything else: decide by hand.
    Review,
}

impl Decision {
    fn name(self) -> &'static str {
        match self {
            Decision::Keep => "keep",
            Decision::Delete => "delete",
            Decision::Review => "review",
        }
    }
}

/// How messages are sorted.
pub struct Rules<'a> {
    /// Addresses the mailbox keeps, lower-cased.
    pub own: &'a [String],
    /// Domain that moved to the target.
    pub domain: &'a str,
    /// Sender domains whose mail went to the moved domain, though their
    /// messages do not show it (e.g. notifications to a no-reply address).
    pub moved_senders: &'a [String],
}

/// Sorts one message. `on_target` tells whether the target has a live copy with
/// the same `Message-ID`.
pub fn decide(
    message: &Addressing,
    rules: &Rules<'_>,
    on_target: bool,
) -> (Decision, &'static str) {
    let all: Vec<&String> = message
        .from
        .iter()
        .chain(&message.recipients)
        .chain(&message.delivered_to)
        .collect();
    let suffix = format!("@{}", rules.domain);
    if all.iter().any(|a| rules.own.contains(a)) {
        return (Decision::Keep, "involves an address to keep");
    }
    // A Message-ID in the domain was made by a client sending from it.
    let made_in_domain = message
        .message_id
        .as_deref()
        .is_some_and(|id| id.trim_end_matches('>').to_lowercase().ends_with(&suffix));
    let moved_sender = message.from.iter().any(|a| {
        rules
            .moved_senders
            .iter()
            .any(|d| a.ends_with(&format!("@{d}")) || a.ends_with(&format!(".{d}")))
    });
    if !made_in_domain && !moved_sender && !all.iter().any(|a| a.ends_with(&suffix)) {
        return (
            Decision::Review,
            "involves neither the domain nor an address to keep",
        );
    }
    if on_target {
        (
            Decision::Delete,
            "involves the domain, and the target has it",
        )
    } else {
        (
            Decision::Review,
            "involves the domain, but the target has no copy with its Message-ID",
        )
    }
}

/// Reads every message's addressing on Outlook.com and writes
/// `outlook-cleanup.csv` in `work`. Only reads.
#[cfg(windows)]
pub fn report_cleanup_command(
    graph: &GraphArgs,
    work: &Path,
    own: &[String],
    domain: &str,
    moved_senders: &[String],
) -> Result<(), String> {
    let own: Vec<String> = own.iter().map(|a| a.trim().to_lowercase()).collect();
    let domain = domain.trim().trim_start_matches('@').to_lowercase();
    let moved_senders: Vec<String> = moved_senders
        .iter()
        .map(|d| d.trim().trim_start_matches('@').to_lowercase())
        .collect();
    let rules = Rules {
        own: &own,
        domain: &domain,
        moved_senders: &moved_senders,
    };
    let target = read_inventory(&work.join(VERIFIED_INVENTORY)).map_err(|e| e.to_string())?;
    let on_target: HashSet<String> = target
        .iter()
        .filter(|item| !ecm_audit::verify::is_parked(&item.folder))
        .filter_map(|item| item.message_id.clone())
        .collect();

    let transport = ecm_graph::WinHttpTransport::default();
    let token =
        ecm_graph::auth::sign_in(&transport, &graph.client_id, &graph.tenant, &mut |prompt| {
            eprintln!();
            eprintln!(
                "Sign in to Outlook.com: open {} and enter the code {}",
                prompt.verification_uri, prompt.user_code
            );
            eprintln!("(Waiting for you to sign in…)");
        })
        .map_err(|e| e.to_string())?;
    let client = ecm_graph::GraphClient::new(transport, token);
    let tree = client.mail_folders().map_err(|e| e.to_string())?;

    let mut rows = Vec::new();
    let mut summary: BTreeMap<(String, Decision), usize> = BTreeMap::new();
    let mut reasons: BTreeMap<&'static str, usize> = BTreeMap::new();
    for folder in tree.flatten().into_iter().skip(1) {
        if folder.total == 0 {
            continue;
        }
        let messages = client.addressing(folder).map_err(|e| e.to_string())?;
        eprintln!("  outlook.com {}: {} messages", folder.path, messages.len());
        for message in &messages {
            let present = message
                .message_id
                .as_ref()
                .is_some_and(|id| on_target.contains(id));
            let (decision, reason) = decide(message, &rules, present);
            *summary.entry((folder.path.clone(), decision)).or_default() += 1;
            *reasons.entry(reason).or_default() += 1;
            rows.push(vec![
                decision.name().into(),
                reason.into(),
                folder.path.clone(),
                message.sent.map(format_utc).unwrap_or_default(),
                message.from.join(" "),
                message.recipients.join(" "),
                message.delivered_to.join(" "),
                message.subject.clone().unwrap_or_default(),
                message.message_id.clone().unwrap_or_default(),
                message.id.clone(),
            ]);
        }
    }
    let path = work.join("outlook-cleanup.csv");
    write_csv(
        &path,
        &[
            "decision",
            "reason",
            "folder",
            "sent",
            "from",
            "recipients",
            "delivered_to",
            "subject",
            "message_id",
            "graph_id",
        ],
        &rows,
    )
    .map_err(|e| e.to_string())?;

    println!("{} messages read. Outlook.com was not changed.", rows.len());
    println!();
    println!(
        "{:<40} {:>7} {:>7} {:>7}",
        "Folder", "keep", "delete", "review"
    );
    let mut folders: BTreeMap<&str, [usize; 3]> = BTreeMap::new();
    for ((folder, decision), n) in &summary {
        folders.entry(folder.as_str()).or_default()[*decision as usize] += n;
    }
    for (folder, [keep, delete, review]) in folders {
        println!("{folder:<40} {keep:>7} {delete:>7} {review:>7}");
    }
    println!();
    for (reason, n) in reasons {
        println!("  {n:>7}  {reason}");
    }
    println!();
    println!("Details: {}", path.display());
    Ok(())
}

#[cfg(not(windows))]
pub fn report_cleanup_command(
    _: &GraphArgs,
    _: &Path,
    _: &[String],
    _: &str,
    _: &[String],
) -> Result<(), String> {
    Err("Microsoft Graph access needs Windows for now (WinHTTP)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(from: &[&str], to: &[&str], delivered: &[&str]) -> Addressing {
        let list = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect();
        Addressing {
            id: "M".into(),
            folder: "Inbox".into(),
            subject: None,
            message_id: Some("<1@x.test>".into()),
            sent: None,
            from: list(from),
            recipients: list(to),
            delivered_to: list(delivered),
        }
    }

    fn rules<'a>(own: &'a [String], moved: &'a [String]) -> Rules<'a> {
        Rules {
            own,
            domain: "moved.test",
            moved_senders: moved,
        }
    }

    #[test]
    fn keeps_what_involves_an_address_to_keep() {
        let own = vec!["me@outlook.test".to_owned()];
        let mixed = message(&["a@x.test"], &["me@outlook.test", "me@moved.test"], &[]);
        assert_eq!(decide(&mixed, &rules(&own, &[]), true).0, Decision::Keep);
    }

    #[test]
    fn deletes_only_what_the_target_has() {
        let own = vec!["me@outlook.test".to_owned()];
        let list_mail = message(
            &["news@list.test"],
            &["members@list.test"],
            &["webmaster@moved.test"],
        );
        assert_eq!(
            decide(&list_mail, &rules(&own, &[]), true).0,
            Decision::Delete
        );
        assert_eq!(
            decide(&list_mail, &rules(&own, &[]), false).0,
            Decision::Review
        );
        let unknown = message(&["news@list.test"], &["members@list.test"], &[]);
        assert_eq!(
            decide(&unknown, &rules(&own, &[]), true).0,
            Decision::Review
        );
    }

    #[test]
    fn counts_sent_mail_and_moved_senders_as_the_domain() {
        let own = vec!["me@outlook.test".to_owned()];
        let mut sent = message(&[], &["friend@x.test"], &[]);
        sent.message_id = Some("<abc@moved.test>".into());
        assert_eq!(decide(&sent, &rules(&own, &[]), true).0, Decision::Delete);
        let notification = message(
            &["notifications@github.test"],
            &["x@noreply.github.test"],
            &[],
        );
        let moved = vec!["github.test".to_owned()];
        assert_eq!(
            decide(&notification, &rules(&own, &moved), true).0,
            Decision::Delete
        );
        assert_eq!(
            decide(&notification, &rules(&own, &[]), true).0,
            Decision::Review
        );
    }
}
