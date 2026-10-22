//! `ecm plan-graph-pim` and `ecm check-graph-pim`: copy the calendar, contacts
//! and their photos, To Do tasks, inbox rules and categories of an Exchange
//! mailbox, read over EWS, into a mailbox reached through Microsoft Graph, and
//! check the copies against their source (milestone M9b).
//!
//! With `--pst`, the calendar, contacts and tasks come from a PST file instead
//! (milestone M10), turned into what EWS gives for them.
//!
//! With `--test`, everything goes into test containers: the `ecm-test`
//! calendar, contact folder and To Do list, and rules (disabled) and categories
//! named with [`TEST_PREFIX`]; items whose reminder would ring later are left
//! out. Events go without their attendees: Graph would invite them.

use std::collections::{BTreeSet, HashMap};

use base64::Engine as _;
use ecm_audit::plan::{Action, FolderMap};
use ecm_ews::{pim::Category, xml::Element};
use ecm_model::{
    compare::{Difference, PropertyMap, compare_on},
    item::parse_utc,
    path,
};
use serde_json::{Value, json};

use crate::{
    fidelity_cmd::TEST_FOLDER,
    graph_pim_cmd::{
        CATEGORY_COMPARED, CONTACT_COMPARED, EVENT_COMPARED, RULE_COMPARED, TASK_COMPARED,
        TEST_PREFIX, category_properties, ews_contact_properties, ews_event_properties,
        ews_task_properties, graph_color, graph_contact, graph_contact_properties, graph_event,
        graph_event_properties, graph_rule, graph_task, graph_task_properties, has_photo,
        rule_properties,
    },
    pim_cmd::same_subject,
    rules_cmd::FOLDER_PLACEHOLDER,
};

/// Two starts this close, in seconds, are the same event.
const START_TOLERANCE: i64 = 120;

/// An item type copied into a Graph mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PimKind {
    /// Calendar items.
    Event,
    /// Contacts and their photos.
    Contact,
    /// Tasks, into To Do.
    Task,
    /// Inbox rules.
    Rule,
    /// The master category list.
    Category,
}

/// Where the items come from.
#[derive(Debug)]
pub enum PimSource {
    /// An Exchange mailbox, over EWS: every type.
    Ews(crate::EwsArgs),
    /// A PST file: calendar items, contacts and tasks, of the folders named, or
    /// else of its calendar, contact and task folders.
    Pst {
        /// The file.
        path: std::path::PathBuf,
        /// Folder paths to read; empty for the default ones.
        folders: Vec<String>,
    },
}

/// What to copy, and where.
#[derive(Debug, Clone, Copy)]
pub struct PimOptions<'a> {
    /// Into test containers, names prefixed and rules disabled.
    pub test: bool,
    /// Only these types; every type when empty.
    pub only: &'a [PimKind],
}

impl PimOptions<'_> {
    fn wants(&self, kind: PimKind) -> bool {
        self.only.is_empty() || self.only.contains(&kind)
    }

    /// The calendar, contact folder and To Do list items go into, by name:
    /// empty for the default ones.
    fn container(&self) -> &'static str {
        if self.test { TEST_FOLDER } else { "" }
    }

    /// The name a rule or category gets in the target.
    fn name(&self, name: &str) -> String {
        if self.test {
            format!("{TEST_PREFIX}{name}")
        } else {
            name.to_owned()
        }
    }

    /// What is taken off the target's rule and category names to compare them.
    fn prefix(&self) -> &'static str {
        if self.test { TEST_PREFIX } else { "" }
    }
}

/// What the source mailbox holds, read whole.
#[derive(Default)]
pub struct Source {
    /// Calendar items: single ones and series masters.
    pub events: Vec<Element>,
    /// Contacts.
    pub contacts: Vec<Element>,
    /// Tasks.
    pub tasks: Vec<Element>,
    /// Inbox rules, in order.
    pub rules: Vec<Element>,
    /// The master category list.
    pub categories: Vec<Category>,
    /// Folder paths by id, for the rules.
    pub paths: HashMap<String, String>,
    /// What is not copied, and why.
    pub left_out: Vec<String>,
}

/// What the target already holds where the plan writes, to leave it out.
#[derive(Default)]
pub struct Existing {
    /// Events, as compared properties.
    pub events: Vec<PropertyMap>,
    /// Contacts, as compared properties.
    pub contacts: Vec<PropertyMap>,
    /// Tasks, as compared properties.
    pub tasks: Vec<PropertyMap>,
    /// Inbox rule names.
    pub rules: Vec<String>,
    /// The highest rule sequence: copies go after.
    pub last_rule: i64,
    /// Category names.
    pub categories: Vec<String>,
    /// Where well-known source folders go.
    pub map: FolderMap,
    /// Every folder path.
    pub folders: BTreeSet<String>,
}

fn field(item: &Element, name: &str) -> Option<String> {
    item.child_text(name)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A subject or name in quotes, for notes.
fn quoted(text: &Option<String>) -> String {
    format!("{:?}", text.as_deref().unwrap_or_default())
}

/// Whether two events start together: within [`START_TOLERANCE`], or on the
/// same day for all-day events.
fn same_start(a: &PropertyMap, b: &PropertyMap) -> bool {
    let start = |p: &PropertyMap| p.get("start").and_then(|s| parse_utc(s));
    match (start(a), start(b)) {
        (Some(x), Some(y)) => (x - y).abs() <= START_TOLERANCE,
        _ => a.get("start").is_some() && a.get("start") == b.get("start"),
    }
}

pub(crate) fn same_event(a: &PropertyMap, b: &PropertyMap) -> bool {
    same_start(a, b)
        && match (a.get("subject"), b.get("subject")) {
            (None, None) => true,
            (a, b) => same_subject(a.map(String::as_str), b.map(String::as_str)),
        }
}

/// Like [`same_event`], with the very same subject.
pub(crate) fn same_event_strictly(a: &PropertyMap, b: &PropertyMap) -> bool {
    same_start(a, b) && a.get("subject") == b.get("subject")
}

/// A contact's name: its display name, else what it is filed as, else its
/// company (a company's card often has no display name).
fn contact_name(p: &PropertyMap) -> Option<String> {
    ["display_name", "file_as", "company"]
        .iter()
        .find_map(|key| p.get(*key))
        .map(|name| name.to_lowercase())
}

/// Contacts with the same name.
pub(crate) fn same_contact_strictly(a: &PropertyMap, b: &PropertyMap) -> bool {
    matches!((contact_name(a), contact_name(b)), (Some(x), Some(y)) if x == y)
}

/// Contacts with the same name, an address in common, or the same mobile
/// phone: a server can rebuild a display name from the name's parts.
pub(crate) fn same_contact(a: &PropertyMap, b: &PropertyMap) -> bool {
    let emails = |p: &PropertyMap| -> BTreeSet<String> {
        p.get("emails")
            .map(|e| e.split(';').map(str::to_owned).collect())
            .unwrap_or_default()
    };
    !emails(a).is_disjoint(&emails(b))
        || same_contact_strictly(a, b)
        || matches!((a.get("mobile_phone"), b.get("mobile_phone")), (Some(x), Some(y)) if x == y)
}

pub(crate) fn same_task(a: &PropertyMap, b: &PropertyMap) -> bool {
    a.get("subject") == b.get("subject") && a.get("due") == b.get("due")
}

fn same_name(a: &PropertyMap, b: &PropertyMap) -> bool {
    matches!((a.get("name"), b.get("name")), (Some(x), Some(y)) if x.to_lowercase() == y.to_lowercase())
}

/// Tells whether a source item and a copy are the same item.
pub type Matcher = fn(&PropertyMap, &PropertyMap) -> bool;

/// Pairs each source item with a copy, each copy with one source item at most:
/// first those `strict` matches, then those `same` matches, the pairs that
/// differ on the fewest of `names` first. Source items that share a subject and
/// a start, or an address, then find their own copy.
pub fn pair(
    sources: &[PropertyMap],
    copies: &[PropertyMap],
    names: &[&str],
    strict: Matcher,
    same: Matcher,
) -> Vec<Option<usize>> {
    let mut taken = vec![false; copies.len()];
    let mut pairs = vec![None; sources.len()];
    for matcher in [strict, same] {
        let mut candidates = Vec::new();
        for (i, source) in sources.iter().enumerate() {
            if pairs[i].is_some() {
                continue;
            }
            for (j, copy) in copies.iter().enumerate() {
                if !taken[j] && matcher(source, copy) {
                    candidates.push((compare_on(names, source, copy).len(), i, j));
                }
            }
        }
        candidates.sort_unstable();
        for (_, i, j) in candidates {
            if pairs[i].is_none() && !taken[j] {
                pairs[i] = Some(j);
                taken[j] = true;
            }
        }
    }
    pairs
}

/// The folders to create before a step that uses `path`, top first: those of
/// its path that neither exist nor are planned yet.
fn folders_to_create(
    path: &str,
    existing: &BTreeSet<String>,
    planned: &mut BTreeSet<String>,
) -> Vec<Action> {
    let mut missing = Vec::new();
    let mut current = Some(path.to_owned());
    while let Some(path) = current.filter(|p| !p.is_empty()) {
        if existing.contains(&path) || planned.contains(&path) {
            break;
        }
        missing.push(path.clone());
        current = path::parent(&path);
    }
    missing
        .into_iter()
        .rev()
        .map(|path| {
            planned.insert(path.clone());
            Action::CreateFolder { path }
        })
        .collect()
}

/// The target path of a rule's source folder, by the folder's EWS id. In a
/// test, a folder the target lacks is replaced by `ecm-test`; `fallback` then
/// names it.
fn rule_folder(
    id: &str,
    source: &Source,
    existing: &Existing,
    test: bool,
    fallback: &mut Vec<String>,
) -> Option<String> {
    let path = existing.map.target(source.paths.get(id)?);
    if test && !existing.folders.contains(&path) {
        fallback.push(path);
        return Some(TEST_FOLDER.to_owned());
    }
    Some(path)
}

/// Reads a source contact's photo.
pub type PhotoReader<'a> = dyn FnMut(&Element) -> Result<Option<Vec<u8>>, String> + 'a;

/// The plan's steps for what the target lacks, and what the plan says about
/// what it leaves out. `photo` reads a source contact's photo.
pub fn plan_steps(
    source: &Source,
    existing: &Existing,
    options: PimOptions,
    photo: &mut PhotoReader,
) -> Result<(Vec<Action>, Vec<String>), String> {
    let mut actions = Vec::new();
    let mut notes = source.left_out.clone();
    let to = options.container().to_owned();
    let present = |what: &str, count: usize, notes: &mut Vec<String>| {
        if count > 0 {
            notes.push(format!("{count} {what} already in the target"));
        }
    };

    let props: Vec<PropertyMap> = source.events.iter().map(ews_event_properties).collect();
    let pairs = pair(
        &props,
        &existing.events,
        EVENT_COMPARED,
        same_event_strictly,
        same_event,
    );
    let mut found = 0;
    for (item, paired) in source.events.iter().zip(pairs) {
        if paired.is_some() {
            found += 1;
            continue;
        }
        let (json, lost) = graph_event(item);
        let subject = field(item, "Subject");
        notes.extend(
            lost.iter()
                .map(|l| format!("event {}: {l}", quoted(&subject))),
        );
        actions.push(Action::CreateGraph {
            kind: "event".into(),
            to: to.clone(),
            subject,
            item: json.to_string(),
        });
    }
    present("events", found, &mut notes);

    let props: Vec<PropertyMap> = source
        .contacts
        .iter()
        .map(|c| ews_contact_properties(c, None))
        .collect();
    let pairs = pair(
        &props,
        &existing.contacts,
        CONTACT_COMPARED,
        same_contact_strictly,
        same_contact,
    );
    let mut found = 0;
    for (item, paired) in source.contacts.iter().zip(pairs) {
        if paired.is_some() {
            found += 1;
            continue;
        }
        let (json, lost) = graph_contact(item);
        let subject = field(item, "DisplayName");
        notes.extend(
            lost.iter()
                .map(|l| format!("contact {}: {l}", quoted(&subject))),
        );
        let step = actions.len();
        actions.push(Action::CreateGraph {
            kind: "contact".into(),
            to: to.clone(),
            subject: subject.clone(),
            item: json.to_string(),
        });
        if has_photo(item)
            && let Some(bytes) = photo(item)?
        {
            actions.push(Action::SetPhoto {
                requires: step,
                to: to.clone(),
                subject,
                photo: base64::engine::general_purpose::STANDARD.encode(bytes),
            });
        }
    }
    present("contacts", found, &mut notes);

    let props: Vec<PropertyMap> = source.tasks.iter().map(ews_task_properties).collect();
    let pairs = pair(&props, &existing.tasks, TASK_COMPARED, same_task, same_task);
    let mut found = 0;
    for (item, paired) in source.tasks.iter().zip(pairs) {
        if paired.is_some() {
            found += 1;
            continue;
        }
        let (json, lost) = graph_task(item);
        let subject = field(item, "Subject");
        notes.extend(
            lost.iter()
                .map(|l| format!("task {}: {l}", quoted(&subject))),
        );
        actions.push(Action::CreateGraph {
            kind: "task".into(),
            to: to.clone(),
            subject,
            item: json.to_string(),
        });
    }
    present("tasks", found, &mut notes);

    let mut found = 0;
    let mut planned = BTreeSet::new();
    for (n, rule) in source.rules.iter().enumerate() {
        let name = field(rule, "DisplayName").unwrap_or_default();
        let target_name = options.name(&name);
        if existing
            .rules
            .iter()
            .any(|r| r.to_lowercase() == target_name.to_lowercase())
        {
            found += 1;
            continue;
        }
        let mut fallback = Vec::new();
        let mut paths = Vec::new();
        let (mut json, lost) = graph_rule(rule, &mut |id| {
            let path = rule_folder(id, source, existing, options.test, &mut fallback)?;
            paths.push(path.clone());
            Some(format!("{FOLDER_PLACEHOLDER}{path}"))
        });
        for path in &paths {
            actions.extend(folders_to_create(path, &existing.folders, &mut planned));
        }
        for path in fallback {
            notes.push(format!(
                "rule {name:?}: the target has no folder {path}: the test rule uses {TEST_FOLDER}"
            ));
        }
        notes.extend(lost.iter().map(|l| format!("rule {name:?}: {l}")));
        json["displayName"] = json!(target_name);
        if options.test {
            json["isEnabled"] = json!(false);
        }
        json["sequence"] = json!(existing.last_rule + 1 + n as i64);
        actions.push(Action::CreateGraph {
            kind: "rule".into(),
            to: String::new(),
            subject: Some(name),
            item: json.to_string(),
        });
    }
    present("inbox rules", found, &mut notes);

    let mut found = 0;
    for category in &source.categories {
        let name = options.name(&category.name);
        if existing
            .categories
            .iter()
            .any(|c| c.to_lowercase() == name.to_lowercase())
        {
            found += 1;
            continue;
        }
        actions.push(Action::CreateGraph {
            kind: "category".into(),
            to: String::new(),
            subject: Some(category.name.clone()),
            item: json!({ "displayName": name, "color": graph_color(category.color) }).to_string(),
        });
    }
    present("categories", found, &mut notes);
    Ok((actions, notes))
}

/// How the copies of one item type compare with their source.
#[derive(Debug, Default)]
pub struct Checked {
    /// Source items.
    pub source: usize,
    /// Copies equal to their source on every compared property.
    pub equal: usize,
    /// Copies that differ, and how.
    pub different: Vec<(String, Vec<Difference>)>,
    /// Source items without a copy, with what copies of the same name start
    /// when, to tell why.
    pub missing: Vec<String>,
    /// Copies read.
    pub copies: usize,
}

/// Pairs each source item with its copy ([`pair`]) and compares them on
/// `names`.
pub fn check_kind(
    names: &[&str],
    sources: &[(String, PropertyMap)],
    copies: &[PropertyMap],
    strict: Matcher,
    same: Matcher,
) -> Checked {
    let props: Vec<PropertyMap> = sources.iter().map(|(_, p)| p.clone()).collect();
    let mut checked = Checked {
        source: sources.len(),
        copies: copies.len(),
        ..Checked::default()
    };
    for ((label, source), paired) in sources
        .iter()
        .zip(pair(&props, copies, names, strict, same))
    {
        match paired {
            None => {
                let named = |p: &PropertyMap| {
                    ["subject", "display_name", "name"]
                        .iter()
                        .find_map(|key| p.get(*key))
                        .cloned()
                };
                let starts: Vec<&str> = copies
                    .iter()
                    .filter(|copy| named(copy).is_some() && named(copy) == named(source))
                    .map(|copy| copy.get("start").map_or("-", String::as_str))
                    .collect();
                let start = source.get("start").map_or("-", String::as_str);
                checked.missing.push(if starts.is_empty() {
                    format!("{label:?} ({start}): no copy of that name")
                } else {
                    format!(
                        "{label:?} ({start}): copies of that name start {}",
                        starts.join(", ")
                    )
                });
            }
            Some(j) => {
                let differences = compare_on(names, source, &copies[j]);
                if differences.is_empty() {
                    checked.equal += 1;
                } else {
                    checked.different.push((label.clone(), differences));
                }
            }
        }
    }
    checked
}

pub(crate) fn print_checked(what: &str, checked: &Checked) {
    println!(
        "{what}: {} in the source, {} copies read; {} equal, {} different, {} missing",
        checked.source,
        checked.copies,
        checked.equal,
        checked.different.len(),
        checked.missing.len()
    );
    for (label, differences) in &checked.different {
        for difference in differences {
            let short = |value: &Option<String>| -> String {
                value.as_deref().unwrap_or("-").chars().take(60).collect()
            };
            println!(
                "  different {label:?}: {} {:?} in the source, {:?} in the copy",
                difference.name,
                short(&difference.expected),
                short(&difference.actual)
            );
        }
    }
    for line in &checked.missing {
        println!("  missing {line}");
    }
}

#[cfg(windows)]
pub use online::{check_graph_pim_command, plan_graph_pim_command};
#[cfg(windows)]
pub(crate) use online::{photo_of, read_any};

#[cfg(windows)]
mod online {
    use std::{fs, path::Path};

    use ecm_graph::{GraphClient, Transport};

    use super::*;
    use crate::{
        audit_cmd::GraphArgs,
        connect,
        graph_pim_cmd::rings_later,
        graph_plan_cmd::{GraphFolders, graph_folder_map},
    };

    /// What the target holds where a plan writes, as Graph returns it.
    struct Target {
        /// The contact folder written to, `None` for the default one.
        contact_folder: Option<String>,
        events: Vec<Value>,
        contacts: Vec<Value>,
        tasks: Vec<Value>,
        rules: Vec<Value>,
        categories: Vec<Value>,
        folders: Option<GraphFolders>,
    }

    /// Leaves out cancelled items, and in test containers those whose reminder
    /// would ring later, saying so in `left_out`; then stores them in `source`.
    fn keep(
        source: &mut Source,
        kind: PimKind,
        mut items: Vec<Element>,
        what: &str,
        options: PimOptions,
    ) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let before = items.len();
        items.retain(|item| item.child_text("IsCancelled") != Some("true"));
        if items.len() < before {
            source
                .left_out
                .push(format!("{} cancelled {what} items", before - items.len()));
        }
        if options.test && kind != PimKind::Contact {
            let before = items.len();
            items.retain(|item| !rings_later(item, now));
            if items.len() < before {
                source.left_out.push(format!(
                    "{} {what} items whose reminder would ring later (a test copy would ring too)",
                    before - items.len()
                ));
            }
        }
        match kind {
            PimKind::Event => source.events = items,
            PimKind::Contact => source.contacts = items,
            _ => source.tasks = items,
        }
    }

    /// The source's items, and the Exchange client that reads its contacts'
    /// photos (`None` for a PST, whose photos are in the items).
    pub(crate) fn read_any(
        from: &PimSource,
        options: PimOptions,
    ) -> Result<(Source, Option<ecm_ews::EwsClient<ecm_http::HttpClient>>), String> {
        match from {
            PimSource::Ews(ews) => {
                let client = connect(ews)?;
                let source = read_source(&client, options)?;
                Ok((source, Some(client)))
            }
            PimSource::Pst { path, folders } => {
                let pim = crate::pst_pim::read_pst(path, folders)?;
                let mut source = Source {
                    left_out: pim.left_out,
                    ..Source::default()
                };
                for (kind, items, what) in [
                    (PimKind::Event, pim.events, "calendar"),
                    (PimKind::Contact, pim.contacts, "contacts"),
                    (PimKind::Task, pim.tasks, "tasks"),
                ] {
                    if options.wants(kind) {
                        keep(&mut source, kind, items, what, options);
                    }
                }
                for (kind, what) in [
                    (PimKind::Rule, "inbox rules"),
                    (PimKind::Category, "categories"),
                ] {
                    if options.wants(kind) {
                        source
                            .left_out
                            .push(format!("{what}: a PST is not read for them"));
                    }
                }
                Ok((source, None))
            }
        }
    }

    /// A contact's photo: read over EWS, or carried by the PST contact.
    pub(crate) fn photo_of(
        client: Option<&ecm_ews::EwsClient<ecm_http::HttpClient>>,
        item: &Element,
    ) -> Result<Option<Vec<u8>>, String> {
        match client {
            Some(client) => client.contact_photo(item).map_err(|e| e.to_string()),
            None => Ok(crate::pst_pim::photo(item)),
        }
    }

    fn read_source<T: ecm_ews::Transport>(
        client: &ecm_ews::EwsClient<T>,
        options: PimOptions,
    ) -> Result<Source, String> {
        let mut source = Source::default();
        for (kind, distinguished) in [
            (PimKind::Event, "calendar"),
            (PimKind::Contact, "contacts"),
            (PimKind::Task, "tasks"),
        ] {
            if !options.wants(kind) {
                continue;
            }
            let folder = client
                .get_distinguished_folder(distinguished)
                .map_err(|e| e.to_string())?;
            let items = client.full_items(&folder.id).map_err(|e| e.to_string())?;
            keep(&mut source, kind, items, distinguished, options);
        }
        if options.wants(PimKind::Rule) {
            source.rules = client.inbox_rule_elements().map_err(|e| e.to_string())?;
            source.paths = client
                .folder_tree("msgfolderroot")
                .map_err(|e| e.to_string())?
                .with_paths()
                .into_iter()
                .map(|(path, folder)| (folder.id.clone(), path))
                .collect();
        }
        if options.wants(PimKind::Category) {
            source.categories = client.master_categories().map_err(|e| e.to_string())?;
        }
        Ok(source)
    }

    fn read_target<T: Transport>(
        graph: &GraphClient<T>,
        options: PimOptions,
    ) -> Result<Target, String> {
        let error = |e: ecm_graph::Error| e.to_string();
        // A test container not created yet holds nothing.
        let container = |collection: &str, field: &str| -> Result<Option<Option<String>>, String> {
            if options.test {
                Ok(graph
                    .find_named(collection, field, TEST_FOLDER)
                    .map_err(error)?
                    .map(Some))
            } else {
                Ok(Some(None))
            }
        };
        let mut target = Target {
            contact_folder: None,
            events: Vec::new(),
            contacts: Vec::new(),
            tasks: Vec::new(),
            rules: Vec::new(),
            categories: Vec::new(),
            folders: None,
        };
        if options.wants(PimKind::Event)
            && let Some(calendar) = container("/me/calendars", "name")?
        {
            target.events = graph.events_in(calendar.as_deref()).map_err(error)?;
        }
        if options.wants(PimKind::Contact)
            && let Some(folder) = container("/me/contactFolders", "displayName")?
        {
            target.contacts = graph.contacts_in(folder.as_deref()).map_err(error)?;
            target.contact_folder = folder;
        }
        if options.wants(PimKind::Task) {
            let list = match container("/me/todo/lists", "displayName")? {
                Some(Some(list)) => Some(list),
                Some(None) => graph.default_todo_list().map_err(error)?,
                None => None,
            };
            if let Some(list) = list {
                target.tasks = graph.todo_tasks(&list).map_err(error)?;
            }
        }
        if options.wants(PimKind::Rule) {
            target.rules = graph.inbox_rules().map_err(error)?;
            target.folders = Some(graph_folder_map(graph)?);
        }
        if options.wants(PimKind::Category) {
            target.categories = graph.master_categories().map_err(error)?;
        }
        Ok(target)
    }

    fn existing(target: &Target) -> Existing {
        let names = |values: &[Value], field: &str| -> Vec<String> {
            values
                .iter()
                .filter_map(|v| v[field].as_str())
                .map(str::to_owned)
                .collect()
        };
        Existing {
            events: target.events.iter().map(graph_event_properties).collect(),
            contacts: target
                .contacts
                .iter()
                .map(|c| graph_contact_properties(c, None))
                .collect(),
            tasks: target.tasks.iter().map(graph_task_properties).collect(),
            rules: names(&target.rules, "displayName"),
            last_rule: target
                .rules
                .iter()
                .filter_map(|r| r["sequence"].as_i64())
                .max()
                .unwrap_or(0),
            categories: names(&target.categories, "displayName"),
            map: target
                .folders
                .as_ref()
                .map(|f| f.map.clone())
                .unwrap_or_default(),
            folders: target
                .folders
                .as_ref()
                .map(|f| f.existing.clone())
                .unwrap_or_default(),
        }
    }

    fn sign_in(graph: &GraphArgs) -> Result<GraphClient<ecm_graph::WinHttpTransport>, String> {
        let transport = ecm_graph::WinHttpTransport::default();
        let token = ecm_graph::auth::sign_in_with(
            &transport,
            &graph.client_id,
            &graph.tenant,
            "MailboxSettings.Read",
            &mut |prompt| {
                eprintln!();
                eprintln!(
                    "Sign in to Outlook.com (to read it): open {} and enter the code {}",
                    prompt.verification_uri, prompt.user_code
                );
                eprintln!("(Waiting for you to sign in...)");
            },
        )
        .map_err(|e| e.to_string())?;
        Ok(GraphClient::new(transport, token))
    }

    /// Writes a plan that copies into the Graph mailbox what of the source's
    /// calendar, contacts, tasks, inbox rules and categories it lacks. Only
    /// reads the source and the target.
    pub fn plan_graph_pim_command(
        from: &PimSource,
        graph: &GraphArgs,
        options: PimOptions,
        out: &Path,
    ) -> Result<(), String> {
        let (source, client) = read_any(from, options)?;
        let graph = sign_in(graph)?;
        let target = read_target(&graph, options)?;
        let (actions, notes) = plan_steps(&source, &existing(&target), options, &mut |item| {
            photo_of(client.as_ref(), item)
        })?;
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
            "Nothing was changed. Review the plan, then run `ecm apply-graph --plan {} --dry-run`.",
            out.display()
        );
        Ok(())
    }

    /// Compares each source item of the Exchange mailbox with its copy in the
    /// Graph mailbox, and lists those that differ or have none. Only reads.
    /// The ids of the events and contacts the plans' `create-graph` steps
    /// created, from their journals.
    fn created_ids(plans: &[std::path::PathBuf]) -> Result<HashMap<String, Vec<String>>, String> {
        let mut ids: HashMap<String, Vec<String>> = HashMap::new();
        for plan in plans {
            let actions = crate::plan_cmd::read_plan(plan)?;
            let done = crate::plan_cmd::read_journal(&crate::plan_cmd::journal_path(plan))?;
            for (step, action) in actions.iter().enumerate() {
                if let (Action::CreateGraph { kind, .. }, Some(id)) =
                    (action, done.get(&step).and_then(|line| line.id.clone()))
                {
                    ids.entry(kind.clone()).or_default().push(id);
                }
            }
        }
        Ok(ids)
    }

    /// Adds to `copies` those of `ids` it lacks, read one by one at `url(id)`:
    /// copies the server moved out of their container. Returns how many.
    fn read_moved<T: Transport>(
        graph: &GraphClient<T>,
        copies: &mut Vec<Value>,
        ids: &[String],
        url: &dyn Fn(&str) -> String,
    ) -> Result<usize, String> {
        let listed: BTreeSet<String> = copies
            .iter()
            .filter_map(|c| c["id"].as_str().map(str::to_owned))
            .collect();
        let mut moved = 0;
        for id in ids.iter().filter(|id| !listed.contains(*id)) {
            match graph.get(&url(id)) {
                Ok(copy) => {
                    copies.push(copy);
                    moved += 1;
                }
                // Deleted since.
                Err(ecm_graph::Error::Graph { status: 404, .. }) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(moved)
    }

    pub fn check_graph_pim_command(
        from: &PimSource,
        graph: &GraphArgs,
        options: PimOptions,
        plans: &[std::path::PathBuf],
    ) -> Result<(), String> {
        let created = created_ids(plans)?;
        let (source, client) = read_any(from, options)?;
        let graph = sign_in(graph)?;
        let mut target = read_target(&graph, options)?;
        let error = |e: ecm_graph::Error| e.to_string();
        for line in &source.left_out {
            println!("Left out of the check: {line}");
        }
        let listed_contacts = target.contacts.len();
        for (kind, copies, url) in [
            (
                "event",
                &mut target.events,
                &(|id: &str| format!("/me/events/{}", ecm_graph::encode(id)))
                    as &dyn Fn(&str) -> String,
            ),
            ("contact", &mut target.contacts, &|id: &str| {
                format!("/me/contacts/{}", ecm_graph::encode(id))
            }),
        ] {
            let ids = created.get(kind).map(Vec::as_slice).unwrap_or_default();
            let moved = read_moved(&graph, copies, ids, url)?;
            if moved > 0 {
                println!(
                    "{moved} {kind} copies the plans created are not in their container any more: read by id"
                );
            }
        }

        if options.wants(PimKind::Event) {
            let sources: Vec<(String, PropertyMap)> = source
                .events
                .iter()
                .map(|e| {
                    (
                        field(e, "Subject").unwrap_or_default(),
                        ews_event_properties(e),
                    )
                })
                .collect();
            let copies: Vec<PropertyMap> =
                target.events.iter().map(graph_event_properties).collect();
            print_checked(
                "events",
                &check_kind(
                    EVENT_COMPARED,
                    &sources,
                    &copies,
                    same_event_strictly,
                    same_event,
                ),
            );
        }
        if options.wants(PimKind::Contact) {
            let mut sources = Vec::new();
            for contact in &source.contacts {
                let photo = if has_photo(contact) {
                    photo_of(client.as_ref(), contact)?
                } else {
                    None
                };
                sources.push((
                    field(contact, "DisplayName").unwrap_or_default(),
                    ews_contact_properties(contact, photo.as_deref()),
                ));
            }
            let mut copies = Vec::new();
            for (n, contact) in target.contacts.iter().enumerate() {
                let id = contact["id"].as_str().unwrap_or_default();
                // A copy read by id is out of its folder: its photo too.
                let folder = target
                    .contact_folder
                    .as_deref()
                    .filter(|_| n < listed_contacts);
                let photo = graph.contact_photo_in(folder, id).map_err(error)?;
                copies.push(graph_contact_properties(contact, photo.as_deref()));
            }
            print_checked(
                "contacts",
                &check_kind(
                    CONTACT_COMPARED,
                    &sources,
                    &copies,
                    same_contact_strictly,
                    same_contact,
                ),
            );
        }
        if options.wants(PimKind::Task) {
            let sources: Vec<(String, PropertyMap)> = source
                .tasks
                .iter()
                .map(|t| {
                    (
                        field(t, "Subject").unwrap_or_default(),
                        ews_task_properties(t),
                    )
                })
                .collect();
            let copies: Vec<PropertyMap> = target.tasks.iter().map(graph_task_properties).collect();
            print_checked(
                "tasks",
                &check_kind(TASK_COMPARED, &sources, &copies, same_task, same_task),
            );
        }
        if options.wants(PimKind::Rule) {
            let existing = existing(&target);
            let paths: HashMap<String, String> = target
                .folders
                .as_ref()
                .map(|f| {
                    f.ids
                        .iter()
                        .map(|(p, id)| (id.clone(), p.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let path_of = |id: &str| paths.get(id).cloned().unwrap_or_else(|| id.to_owned());
            let sources: Vec<(String, PropertyMap)> = source
                .rules
                .iter()
                .map(|rule| {
                    let (json, _) = graph_rule(rule, &mut |id| {
                        rule_folder(id, &source, &existing, options.test, &mut Vec::new())
                    });
                    (
                        field(rule, "DisplayName").unwrap_or_default(),
                        rule_properties(&json, "", &|path| path.to_owned()),
                    )
                })
                .collect();
            let copies: Vec<PropertyMap> = target
                .rules
                .iter()
                .map(|rule| rule_properties(rule, options.prefix(), &path_of))
                .collect();
            let mut names = RULE_COMPARED.to_vec();
            if !options.test {
                names.push("enabled");
            }
            print_checked(
                "inbox rules",
                &check_kind(&names, &sources, &copies, same_name, same_name),
            );
        }
        if options.wants(PimKind::Category) {
            let sources: Vec<(String, PropertyMap)> = source
                .categories
                .iter()
                .map(|c| {
                    (
                        c.name.clone(),
                        category_properties(&c.name, &graph_color(c.color), ""),
                    )
                })
                .collect();
            let copies: Vec<PropertyMap> = target
                .categories
                .iter()
                .map(|c| {
                    category_properties(
                        c["displayName"].as_str().unwrap_or_default(),
                        c["color"].as_str().unwrap_or_default(),
                        options.prefix(),
                    )
                })
                .collect();
            print_checked(
                "categories",
                &check_kind(CATEGORY_COMPARED, &sources, &copies, same_name, same_name),
            );
        }
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn plan_graph_pim_command(
    _: &PimSource,
    _: &crate::audit_cmd::GraphArgs,
    _: PimOptions,
    _: &std::path::Path,
) -> Result<(), String> {
    Err("Exchange and Microsoft Graph access need Windows for now (WinHTTP)".into())
}

#[cfg(not(windows))]
pub fn check_graph_pim_command(
    _: &PimSource,
    _: &crate::audit_cmd::GraphArgs,
    _: PimOptions,
    _: &[std::path::PathBuf],
) -> Result<(), String> {
    Err("Exchange and Microsoft Graph access need Windows for now (WinHTTP)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(xml: &str) -> Element {
        let document = format!(
            r#"<r xmlns:t="http://schemas.microsoft.com/exchange/services/2006/types">{xml}</r>"#
        );
        ecm_ews::xml::parse(document.as_bytes())
            .unwrap()
            .children
            .remove(0)
    }

    fn source() -> Source {
        Source {
            events: vec![
                element(
                    "<t:CalendarItem><t:Subject>Dentist</t:Subject><t:Start>2024-05-06T08:00:00Z</t:Start><t:End>2024-05-06T08:30:00Z</t:End></t:CalendarItem>",
                ),
                element(
                    "<t:CalendarItem><t:Subject>Lunch</t:Subject><t:Start>2024-05-07T10:00:00Z</t:Start><t:End>2024-05-07T11:00:00Z</t:End></t:CalendarItem>",
                ),
            ],
            contacts: vec![element(concat!(
                r#"<t:Contact><t:DisplayName>Ann</t:DisplayName><t:EmailAddresses><t:Entry Key="EmailAddress1">ann@example.test</t:Entry></t:EmailAddresses>"#,
                "<t:Attachments><t:FileAttachment><t:AttachmentId Id=\"A1\"/><t:IsContactPhoto>true</t:IsContactPhoto></t:FileAttachment></t:Attachments></t:Contact>"
            ))],
            tasks: vec![element(
                "<t:Task><t:Subject>Renew passport</t:Subject><t:Status>NotStarted</t:Status></t:Task>",
            )],
            rules: vec![element(concat!(
                "<t:Rule><t:DisplayName>Bank</t:DisplayName><t:Priority>1</t:Priority><t:IsEnabled>true</t:IsEnabled>",
                "<t:Conditions><t:ContainsSenderStrings><t:String>bank</t:String></t:ContainsSenderStrings></t:Conditions>",
                r#"<t:Actions><t:MoveToFolder><t:FolderId Id="F1"/></t:MoveToFolder></t:Actions></t:Rule>"#
            ))],
            categories: vec![
                Category {
                    name: "Famille".into(),
                    color: 9,
                },
                Category {
                    name: "Amis".into(),
                    color: 2,
                },
            ],
            paths: [("F1".to_owned(), "Archive/Banque".to_owned())].into(),
            left_out: vec!["1 cancelled calendar items".into()],
        }
    }

    fn existing() -> Existing {
        let mut lunch = PropertyMap::new();
        lunch.insert("subject".into(), "Lunch".into());
        lunch.insert("start".into(), "2024-05-07T10:01:00Z".into());
        Existing {
            events: vec![lunch],
            categories: vec!["ecm-test Amis".into()],
            rules: vec!["My own rule".into()],
            last_rule: 4,
            folders: ["Archive".to_owned()].into(),
            ..Existing::default()
        }
    }

    #[test]
    fn plans_what_the_test_containers_lack() {
        let options = PimOptions {
            test: true,
            only: &[],
        };
        let (actions, notes) = plan_steps(&source(), &existing(), options, &mut |_| {
            Ok(Some(b"JPEG".to_vec()))
        })
        .unwrap();
        let kinds: Vec<&str> = actions.iter().map(Action::kind).collect();
        assert_eq!(
            kinds,
            [
                "create-event",
                "create-contact",
                "contact-photo",
                "create-task",
                "create-folder",
                "create-rule",
                "create-category"
            ]
        );
        assert!(
            matches!(&actions[0], Action::CreateGraph { to, subject, .. }
            if to == TEST_FOLDER && subject.as_deref() == Some("Dentist"))
        );
        assert!(
            matches!(&actions[2], Action::SetPhoto { requires: 1, photo, .. } if photo == "SlBFRw==")
        );
        // The target lacks Archive/Banque: the test rule moves to ecm-test.
        assert!(matches!(&actions[4], Action::CreateFolder { path } if path == TEST_FOLDER));
        let Action::CreateGraph { item, .. } = &actions[5] else {
            panic!("not a rule");
        };
        let rule: Value = serde_json::from_str(item).unwrap();
        assert_eq!(rule["displayName"], "ecm-test Bank");
        assert_eq!(rule["isEnabled"], false);
        assert_eq!(rule["sequence"], 5);
        assert_eq!(rule["actions"]["moveToFolder"], "ecm-folder:ecm-test");
        let Action::CreateGraph { item, .. } = &actions[6] else {
            panic!("not a category");
        };
        assert_eq!(
            serde_json::from_str::<Value>(item).unwrap(),
            json!({"displayName": "ecm-test Famille", "color": "preset9"})
        );
        assert_eq!(
            notes,
            [
                "1 cancelled calendar items",
                "1 events already in the target",
                "rule \"Bank\": the target has no folder Archive/Banque: the test rule uses ecm-test",
                "1 categories already in the target"
            ]
        );
    }

    #[test]
    fn plans_the_real_containers_and_their_folders() {
        let options = PimOptions {
            test: false,
            only: &[PimKind::Rule, PimKind::Category],
        };
        let mut source = source();
        source.events.clear();
        source.contacts.clear();
        source.tasks.clear();
        let (actions, _) = plan_steps(&source, &existing(), options, &mut |_| Ok(None)).unwrap();
        assert!(matches!(&actions[0], Action::CreateFolder { path } if path == "Archive/Banque"));
        let Action::CreateGraph { item, .. } = &actions[1] else {
            panic!("not a rule");
        };
        let rule: Value = serde_json::from_str(item).unwrap();
        assert_eq!(rule["displayName"], "Bank");
        assert_eq!(rule["isEnabled"], true);
        assert_eq!(rule["actions"]["moveToFolder"], "ecm-folder:Archive/Banque");
        // Neither category exists under its own name.
        assert_eq!(actions.len(), 4);
    }

    #[test]
    fn checks_copies_and_lists_the_missing_and_different() {
        let map = |pairs: &[(&str, &str)]| -> PropertyMap {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let sources = [
            (
                "Dentist".to_owned(),
                map(&[
                    ("subject", "Dentist"),
                    ("start", "2024-05-06T08:00:00Z"),
                    ("location", "Paris"),
                ]),
            ),
            (
                "Lunch".to_owned(),
                map(&[("subject", "Lunch"), ("start", "2024-05-07T10:00:00Z")]),
            ),
            (
                "Gym".to_owned(),
                map(&[("subject", "Gym"), ("start", "2024-05-08T10:00:00Z")]),
            ),
        ];
        let copies = [
            map(&[
                ("subject", "Dentist"),
                ("start", "2024-05-06T08:00:00Z"),
                ("location", "Lyon"),
            ]),
            map(&[("subject", "Lunch"), ("start", "2024-05-07T10:00:00Z")]),
        ];
        let checked = check_kind(
            EVENT_COMPARED,
            &sources,
            &copies,
            same_event_strictly,
            same_event,
        );
        assert_eq!(checked.source, 3);
        assert_eq!(checked.copies, 2);
        assert_eq!(checked.equal, 1);
        assert_eq!(checked.different[0].0, "Dentist");
        assert_eq!(checked.different[0].1[0].name, "location");
        assert_eq!(
            checked.missing,
            ["\"Gym\" (2024-05-08T10:00:00Z): no copy of that name"]
        );
    }

    #[test]
    fn pairs_each_source_item_with_its_own_copy() {
        let map = |pairs: &[(&str, &str)]| -> PropertyMap {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        // Two events at the same time with the same subject, one more category
        // on one of them; one event without a subject; an all-day event.
        let car = |categories: &str| {
            map(&[
                ("subject", "Car pool"),
                ("start", "2013-07-12T16:30:00Z"),
                ("categories", categories),
            ])
        };
        let sources = [
            car("Holiday;Pool;Vacation"),
            car("Holiday;Pool"),
            map(&[("start", "2018-12-11T17:00:00Z")]),
            map(&[
                ("subject", "Ride"),
                ("start", "2026-09-27"),
                ("all_day", "true"),
            ]),
        ];
        let copies = [
            car("Holiday;Pool"),
            map(&[("start", "2018-12-11T17:00:00Z")]),
            map(&[
                ("subject", "Ride"),
                ("start", "2026-09-27"),
                ("all_day", "true"),
            ]),
            car("Holiday;Pool;Vacation"),
        ];
        assert_eq!(
            pair(
                &sources,
                &copies,
                EVENT_COMPARED,
                same_event_strictly,
                same_event
            ),
            [Some(3), Some(0), Some(1), Some(2)]
        );
        // A contact sharing an address with another finds its own copy by name.
        let sources = [
            map(&[("display_name", "Ann"), ("emails", "family@x.test")]),
            map(&[("display_name", "Bob"), ("emails", "family@x.test")]),
        ];
        let copies = [
            map(&[("display_name", "Bob"), ("emails", "family@x.test")]),
            map(&[("display_name", "Ann"), ("emails", "family@x.test")]),
        ];
        assert_eq!(
            pair(
                &sources,
                &copies,
                CONTACT_COMPARED,
                same_contact_strictly,
                same_contact
            ),
            [Some(1), Some(0)]
        );
        // Two contacts of the same name and one copy: the copy goes to the one
        // it equals, not to the first.
        let sources = [
            map(&[("display_name", "Ann"), ("categories", "Friends")]),
            map(&[("display_name", "Ann"), ("categories", "Family")]),
        ];
        let copies = [map(&[("display_name", "Ann"), ("categories", "Family")])];
        assert_eq!(
            pair(
                &sources,
                &copies,
                CONTACT_COMPARED,
                same_contact_strictly,
                same_contact
            ),
            [None, Some(0)]
        );
    }

    #[test]
    fn matches_company_cards_and_renamed_contacts() {
        let card = |pairs: &[(&str, &str)]| -> PropertyMap {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let company = card(&[("company", "Acme Bank")]);
        assert!(same_contact_strictly(
            &company,
            &card(&[("company", "ACME BANK"), ("job_title", "Desk")])
        ));
        assert!(same_contact_strictly(
            &card(&[("file_as", "Acme Rouen"), ("company", "Acme")]),
            &card(&[("file_as", "Acme Rouen")])
        ));
        let renamed = card(&[("display_name", "Dan"), ("mobile_phone", "+1 555 0100")]);
        let rebuilt = card(&[
            ("display_name", "Dan Example"),
            ("mobile_phone", "+1 555 0100"),
        ]);
        assert!(!same_contact_strictly(&renamed, &rebuilt));
        assert!(same_contact(&renamed, &rebuilt));
        assert!(!same_contact(&company, &renamed));
    }

    #[test]
    fn matches_contacts_by_address_or_name_and_tasks_by_due_date() {
        let map = |pairs: &[(&str, &str)]| -> PropertyMap {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        assert!(same_contact(
            &map(&[("emails", "a@x.test;b@x.test"), ("display_name", "Ann")]),
            &map(&[("emails", "b@x.test"), ("display_name", "Ann B")])
        ));
        assert!(same_contact(
            &map(&[("display_name", "Ann")]),
            &map(&[("display_name", "ANN")])
        ));
        assert!(!same_contact(
            &map(&[("emails", "a@x.test")]),
            &map(&[("emails", "c@x.test")])
        ));
        assert!(same_task(
            &map(&[("subject", "Pay"), ("due", "2024-02-28")]),
            &map(&[("subject", "Pay"), ("due", "2024-02-28")])
        ));
        assert!(!same_task(
            &map(&[("subject", "Pay"), ("due", "2024-02-28")]),
            &map(&[("subject", "Pay"), ("due", "2024-03-28")])
        ));
    }
}
