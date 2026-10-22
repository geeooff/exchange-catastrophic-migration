//! `ecm plan-pim --pst` and `ecm check-pim`: copy a PST's calendar items,
//! contacts and their photos, and tasks into an Exchange mailbox over EWS, and
//! check the copies against the PST (milestone M10).
//!
//! With `--test`, everything goes into folders named
//! [`TEST_FOLDER`](crate::fidelity_cmd::TEST_FOLDER) under the target's
//! calendar, contacts and tasks folders, created with their class; items whose
//! reminder would ring later are left out. Meetings keep their attendees only
//! when the user organized them, and `CreateItem` invites no one.

use base64::Engine as _;
use ecm_audit::plan::Action;
use ecm_ews::xml::Element;
use ecm_model::compare::PropertyMap;

use crate::{
    graph_pim_cmd::{
        CONTACT_COMPARED, EVENT_COMPARED, TASK_COMPARED, ews_contact_properties,
        ews_event_properties, ews_task_properties, has_photo,
    },
    graph_pim_plan_cmd::{
        Matcher, PimKind, pair, same_contact, same_contact_strictly, same_event,
        same_event_strictly, same_task,
    },
    pst_pim::{create_xml, photo},
};

/// One item type of the plan: where its items go, and what is already there.
pub struct KindPlan {
    /// The item type.
    pub kind: PimKind,
    /// The target folder path.
    pub to: String,
    /// The folder's class, when the plan creates it.
    pub create: Option<&'static str>,
    /// The source items.
    pub items: Vec<Element>,
    /// What the target folder holds, as compared properties.
    pub existing: Vec<PropertyMap>,
}

/// The plan's name for an item type, its compared properties and matchers.
pub(crate) fn kind_info(
    kind: PimKind,
) -> (&'static str, &'static [&'static str], Matcher, Matcher) {
    match kind {
        PimKind::Event => ("event", EVENT_COMPARED, same_event_strictly, same_event),
        PimKind::Contact => (
            "contact",
            CONTACT_COMPARED,
            same_contact_strictly,
            same_contact,
        ),
        _ => ("task", TASK_COMPARED, same_task, same_task),
    }
}

/// The compared properties of a source item; a contact's with its photo.
pub(crate) fn properties(kind: PimKind, item: &Element, with_photo: bool) -> PropertyMap {
    match kind {
        PimKind::Event => ews_event_properties(item),
        PimKind::Contact => {
            let photo = with_photo.then(|| photo(item)).flatten();
            ews_contact_properties(item, photo.as_deref())
        }
        _ => ews_task_properties(item),
    }
}

/// The plan's steps for what the target lacks, and what the plan says about
/// what it leaves out.
pub fn plan_steps(kinds: &[KindPlan]) -> (Vec<Action>, Vec<String>) {
    let mut actions = Vec::new();
    let mut notes = Vec::new();
    for plan in kinds {
        let (name, compared, strict, same) = kind_info(plan.kind);
        let props: Vec<PropertyMap> = plan
            .items
            .iter()
            .map(|item| properties(plan.kind, item, false))
            .collect();
        let pairs = pair(&props, &plan.existing, compared, strict, same);
        let found = pairs.iter().filter(|p| p.is_some()).count();
        if found > 0 {
            notes.push(format!("{found} {name}s already in {}", plan.to));
        }
        let mut folder_planned = false;
        for (item, paired) in plan.items.iter().zip(pairs) {
            if paired.is_some() {
                continue;
            }
            if let (Some(class), false) = (plan.create, folder_planned) {
                actions.push(Action::CreateTypedFolder {
                    path: plan.to.clone(),
                    class: class.into(),
                });
                folder_planned = true;
            }
            let subject = ["Subject", "DisplayName"]
                .iter()
                .find_map(|n| item.child_text(n))
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            let (xml, lost) = create_xml(item);
            let label = format!("{:?}", subject.as_deref().unwrap_or_default());
            notes.extend(lost.iter().map(|l| format!("{name} {label}: {l}")));
            let step = actions.len();
            actions.push(Action::Create {
                kind: name.into(),
                to: plan.to.clone(),
                subject: subject.clone(),
                item: xml,
            });
            if plan.kind == PimKind::Contact
                && let Some(bytes) = photo(item)
            {
                actions.push(Action::ContactPhoto {
                    item: String::new(),
                    requires: Some(step),
                    subject,
                    photo: base64::engine::general_purpose::STANDARD.encode(bytes),
                });
            }
        }
    }
    (actions, notes)
}

#[cfg(windows)]
pub use online::{check_pim_command, plan_pst_pim_command};

#[cfg(windows)]
mod online {
    use std::{collections::HashMap, fs, path::Path};

    use super::*;
    use crate::{
        EwsArgs, connect,
        fidelity_cmd::TEST_FOLDER,
        graph_pim_plan_cmd::{PimOptions, PimSource, check_kind, print_checked, read_any},
    };

    /// An item type's target folder: its path, its id when it exists, and its
    /// class.
    type TargetFolder = (PimKind, String, Option<String>, &'static str);

    /// The target folder of each wanted item type.
    fn target_folders<T: ecm_ews::Transport>(
        client: &ecm_ews::EwsClient<T>,
        options: PimOptions,
    ) -> Result<Vec<TargetFolder>, String> {
        let tree = client
            .folder_tree("msgfolderroot")
            .map_err(|e| e.to_string())?;
        let by_path: HashMap<String, String> = tree
            .with_paths()
            .into_iter()
            .map(|(path, folder)| (path, folder.id.clone()))
            .collect();
        let mut folders = Vec::new();
        for (kind, distinguished, class) in [
            (PimKind::Event, "calendar", "IPF.Appointment"),
            (PimKind::Contact, "contacts", "IPF.Contact"),
            (PimKind::Task, "tasks", "IPF.Task"),
        ] {
            if !(options.only.is_empty() || options.only.contains(&kind)) {
                continue;
            }
            let folder = client
                .get_distinguished_folder(distinguished)
                .map_err(|e| e.to_string())?;
            let path = by_path
                .iter()
                .find(|(_, id)| **id == folder.id)
                .map(|(path, _)| path.clone())
                .ok_or_else(|| {
                    format!("the {distinguished} folder is not under the mailbox top")
                })?;
            if options.test {
                let path = ecm_model::path::join(&path, TEST_FOLDER);
                let id = by_path.get(&path).cloned();
                folders.push((kind, path, id, class));
            } else {
                folders.push((kind, path, Some(folder.id), class));
            }
        }
        Ok(folders)
    }

    fn items_of(source: &mut crate::graph_pim_plan_cmd::Source, kind: PimKind) -> Vec<Element> {
        match kind {
            PimKind::Event => std::mem::take(&mut source.events),
            PimKind::Contact => std::mem::take(&mut source.contacts),
            _ => std::mem::take(&mut source.tasks),
        }
    }

    /// Writes a plan that creates in the Exchange mailbox the PST's calendar
    /// items, contacts (and their photos) and tasks it lacks. Only reads both.
    pub fn plan_pst_pim_command(
        ews: &EwsArgs,
        pst: &PimSource,
        options: PimOptions,
        out: &Path,
    ) -> Result<(), String> {
        let (mut source, _) = read_any(pst, options)?;
        let client = connect(ews)?;
        let mut kinds = Vec::new();
        for (kind, to, id, class) in target_folders(&client, options)? {
            let existing = match &id {
                Some(id) => client
                    .full_items(id)
                    .map_err(|e| e.to_string())?
                    .iter()
                    .map(|item| properties_of_copy(&client, kind, item, false))
                    .collect::<Result<Vec<_>, String>>()?,
                None => Vec::new(),
            };
            kinds.push(KindPlan {
                kind,
                to,
                create: id.is_none().then_some(class),
                items: items_of(&mut source, kind),
                existing,
            });
        }
        let (actions, mut notes) = plan_steps(&kinds);
        notes.splice(0..0, source.left_out);
        let mut text = String::new();
        for action in &actions {
            text.push_str(&serde_json::to_string(action).map_err(|e| e.to_string())?);
            text.push('\n');
        }
        fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;
        println!("Plan: {} steps into {}", actions.len(), out.display());
        for (kind, n) in ecm_audit::plan::counts(&actions) {
            println!("  {kind:<16} {n:>7}");
        }
        if !notes.is_empty() {
            println!();
            println!("Left out:");
            for line in &notes {
                println!("  {line}");
            }
        }
        println!();
        println!(
            "Nothing was changed. Review the plan, then run `ecm apply --plan {} --dry-run`.",
            out.display()
        );
        Ok(())
    }

    /// The compared properties of a target item; a contact's with its photo
    /// when `with_photo`.
    fn properties_of_copy<T: ecm_ews::Transport>(
        client: &ecm_ews::EwsClient<T>,
        kind: PimKind,
        item: &Element,
        with_photo: bool,
    ) -> Result<PropertyMap, String> {
        Ok(match kind {
            PimKind::Contact => {
                let photo = if with_photo && has_photo(item) {
                    client.contact_photo(item).map_err(|e| e.to_string())?
                } else {
                    None
                };
                ews_contact_properties(item, photo.as_deref())
            }
            _ => properties(kind, item, false),
        })
    }

    /// Compares each of the PST's calendar items, contacts and tasks with its
    /// copy in the Exchange mailbox, and lists those that differ or have none.
    /// Only reads.
    pub fn check_pim_command(
        ews: &EwsArgs,
        pst: &PimSource,
        options: PimOptions,
    ) -> Result<(), String> {
        let (mut source, _) = read_any(pst, options)?;
        for line in &source.left_out {
            println!("Left out of the check: {line}");
        }
        let client = connect(ews)?;
        for (kind, to, id, _) in target_folders(&client, options)? {
            let (name, compared, strict, same) = kind_info(kind);
            let sources: Vec<(String, PropertyMap)> = items_of(&mut source, kind)
                .iter()
                .map(|item| {
                    let label = ["Subject", "DisplayName"]
                        .iter()
                        .find_map(|n| item.child_text(n))
                        .unwrap_or_default()
                        .to_owned();
                    (label, properties(kind, item, true))
                })
                .collect();
            let copies = match &id {
                Some(id) => client
                    .full_items(id)
                    .map_err(|e| e.to_string())?
                    .iter()
                    .map(|item| properties_of_copy(&client, kind, item, true))
                    .collect::<Result<Vec<_>, String>>()?,
                None => Vec::new(),
            };
            print_checked(
                &format!("{name}s in {to}"),
                &check_kind(compared, &sources, &copies, strict, same),
            );
        }
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn plan_pst_pim_command(
    _: &crate::EwsArgs,
    _: &crate::graph_pim_plan_cmd::PimSource,
    _: crate::graph_pim_plan_cmd::PimOptions,
    _: &std::path::Path,
) -> Result<(), String> {
    Err("Exchange access needs Windows for now (WinHTTP)".into())
}

#[cfg(not(windows))]
pub fn check_pim_command(
    _: &crate::EwsArgs,
    _: &crate::graph_pim_plan_cmd::PimSource,
    _: crate::graph_pim_plan_cmd::PimOptions,
) -> Result<(), String> {
    Err("Exchange access needs Windows for now (WinHTTP)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(name: &str, children: Vec<(&str, &str)>) -> Element {
        Element {
            name: name.into(),
            children: children
                .into_iter()
                .map(|(n, t)| Element {
                    name: n.into(),
                    text: t.into(),
                    ..Element::default()
                })
                .collect(),
            ..Element::default()
        }
    }

    #[test]
    fn plans_the_missing_items_their_folder_and_their_photo() {
        let present = element(
            "CalendarItem",
            vec![
                ("Subject", "Dentist"),
                ("Start", "2024-07-15T08:30:00Z"),
                ("End", "2024-07-15T09:00:00Z"),
            ],
        );
        let missing = element(
            "CalendarItem",
            vec![
                ("Subject", "Gym"),
                ("Start", "2024-07-16T08:30:00Z"),
                ("End", "2024-07-16T09:00:00Z"),
            ],
        );
        let mut contact = element("Contact", vec![("DisplayName", "Ann")]);
        contact.children.insert(
            0,
            Element {
                name: "Attachments".into(),
                children: vec![element(
                    "FileAttachment",
                    vec![("IsContactPhoto", "true"), ("Content", "/9j/")],
                )],
                ..Element::default()
            },
        );
        let kinds = [
            KindPlan {
                kind: PimKind::Event,
                to: "Calendrier".into(),
                create: None,
                items: vec![present.clone(), missing],
                existing: vec![ews_event_properties(&present)],
            },
            KindPlan {
                kind: PimKind::Contact,
                to: "Contacts/ecm-test".into(),
                create: Some("IPF.Contact"),
                items: vec![contact],
                existing: Vec::new(),
            },
        ];
        let (actions, notes) = plan_steps(&kinds);
        assert_eq!(notes, ["1 events already in Calendrier"]);
        assert_eq!(actions.len(), 4);
        assert!(
            matches!(&actions[0], Action::Create { kind, to, subject: Some(s), item }
            if kind == "event" && to == "Calendrier" && s == "Gym" && item.contains("<t:Subject>Gym</t:Subject>"))
        );
        assert!(
            matches!(&actions[1], Action::CreateTypedFolder { path, class }
            if path == "Contacts/ecm-test" && class == "IPF.Contact")
        );
        assert!(matches!(&actions[2], Action::Create { kind, item, .. }
            if kind == "contact" && !item.contains("Attachments")));
        assert!(
            matches!(&actions[3], Action::ContactPhoto { requires: Some(2), photo, .. }
            if photo == "/9j/")
        );
    }
}
