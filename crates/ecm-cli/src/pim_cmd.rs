//! `ecm plan-pim`: plan copying the calendar, contacts and To Do tasks of
//! Outlook.com into the target, skipping what the target already holds.
//!
//! The plan carries each item as EWS XML, converted from its Graph JSON, so that
//! `ecm apply` only has to create it.

use std::{collections::HashMap, fs, path::Path};

use ecm_audit::plan::{Action, counts};
use ecm_ews::xml::escape;
use ecm_model::item::{normalize_subject, parse_utc};
use serde_json::Value;

use crate::{EwsArgs, audit_cmd::GraphArgs, connect};

/// Two starts this close, in seconds, are the same event.
const START_TOLERANCE: i64 = 120;

/// Writes a plan that creates, in the target's default calendar, contacts and
/// tasks folders, the Outlook.com items they miss. Only reads both mailboxes.
#[cfg(windows)]
pub fn plan_pim_command(ews: &EwsArgs, graph: &GraphArgs, out: &Path) -> Result<(), String> {
    let client = connect(ews)?;
    let tree = client
        .folder_tree("msgfolderroot")
        .map_err(|e| e.to_string())?;
    let paths: HashMap<String, String> = tree
        .with_paths()
        .into_iter()
        .map(|(path, folder)| (folder.id.clone(), path))
        .collect();
    let folder = |distinguished: &str| -> Result<(String, String), String> {
        let folder = client
            .get_distinguished_folder(distinguished)
            .map_err(|e| e.to_string())?;
        let path = paths
            .get(&folder.id)
            .cloned()
            .ok_or_else(|| format!("the {distinguished} folder is not under the mailbox top"))?;
        Ok((folder.id, path))
    };
    let (calendar_id, calendar) = folder("calendar")?;
    let (contacts_id, contacts_path) = folder("contacts")?;
    let (_, tasks_path) = folder("tasks")?;

    let graph = sign_in(graph)?;
    let mut actions = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    // Calendar.
    let existing = client
        .calendar_briefs(&calendar_id)
        .map_err(|e| e.to_string())?;
    for event in graph.calendar_events().map_err(|e| e.to_string())? {
        let subject = event["subject"].as_str().map(str::to_owned);
        if event["isCancelled"].as_bool() == Some(true) {
            skipped.push(format!("cancelled event {subject:?}"));
            continue;
        }
        let start = graph_time(&event["start"]);
        let same = existing.iter().any(|brief| {
            brief.uid.is_some() && brief.uid.as_deref() == event["iCalUId"].as_str()
                || matches!((brief.start, start), (Some(a), Some(b)) if (a - b).abs() <= START_TOLERANCE)
                    && same_subject(brief.subject.as_deref(), subject.as_deref())
        });
        if same {
            skipped.push(format!("event already in the target {subject:?}"));
            continue;
        }
        let zone = event["originalStartTimeZone"]
            .as_str()
            .and_then(windows_zone);
        let event = match (&zone, event["id"].as_str()) {
            (Some(zone), Some(id)) => graph.event_in(id, zone).map_err(|e| e.to_string())?,
            _ => event,
        };
        actions.push(Action::Create {
            kind: "event".into(),
            to: calendar.clone(),
            subject,
            item: event_xml(&event, zone.as_deref()),
        });
    }

    // Contacts.
    let existing = client
        .contact_briefs(&contacts_id)
        .map_err(|e| e.to_string())?;
    for contact in graph.contacts().map_err(|e| e.to_string())? {
        let name = contact["displayName"].as_str().map(str::to_owned);
        let emails: Vec<String> = contact["emailAddresses"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["address"].as_str())
            .map(str::to_lowercase)
            .collect();
        let same = existing.iter().any(|brief| {
            brief
                .email
                .as_ref()
                .is_some_and(|email| emails.contains(email))
                || brief.display_name.as_deref().map(str::to_lowercase)
                    == name.as_deref().map(str::to_lowercase)
                    && name.is_some()
        });
        if same {
            skipped.push(format!("contact already in the target {name:?}"));
            continue;
        }
        actions.push(Action::Create {
            kind: "contact".into(),
            to: contacts_path.clone(),
            subject: name,
            item: contact_xml(&contact),
        });
    }
    for (folder, count) in graph.contact_folders().map_err(|e| e.to_string())? {
        if count > 0 {
            skipped.push(format!(
                "{count} contacts in the Outlook.com contact folder {folder:?} (not copied)"
            ));
        }
    }

    // To Do tasks. The target has none of its own to match.
    for list in graph.todo_lists().map_err(|e| e.to_string())? {
        let well_known = list["wellknownListName"].as_str().unwrap_or_default();
        let name = list["displayName"].as_str().unwrap_or_default().to_owned();
        if well_known == ecm_graph::pim::FLAGGED_EMAILS_LIST {
            continue;
        }
        let category = (well_known != "defaultList").then_some(name.as_str());
        let id = list["id"].as_str().unwrap_or_default();
        for task in graph.todo_tasks(id).map_err(|e| e.to_string())? {
            actions.push(Action::Create {
                kind: "task".into(),
                to: tasks_path.clone(),
                subject: task["title"].as_str().map(str::to_owned),
                item: task_xml(&task, category),
            });
        }
    }

    let mut text = String::new();
    for action in &actions {
        text.push_str(&serde_json::to_string(action).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("Plan: {} steps into {}", actions.len(), out.display());
    for (kind, n) in counts(&actions) {
        println!("  {kind:<14} {n:>7}");
    }
    if !skipped.is_empty() {
        println!();
        println!("Left out:");
        for line in &skipped {
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

/// Writes a plan that sets, on the target's contacts without a photo, the photo
/// of their Outlook.com counterpart. Only reads both mailboxes.
#[cfg(windows)]
pub fn plan_contact_photos_command(
    ews: &EwsArgs,
    graph: &GraphArgs,
    out: &Path,
) -> Result<(), String> {
    use base64::Engine as _;

    let client = connect(ews)?;
    let folder = client
        .get_distinguished_folder("contacts")
        .map_err(|e| e.to_string())?;
    let targets = client
        .contact_briefs(&folder.id)
        .map_err(|e| e.to_string())?;
    let graph = sign_in(graph)?;

    let mut actions = Vec::new();
    let mut unmatched = 0;
    let mut with_photo = 0;
    for contact in graph.contacts().map_err(|e| e.to_string())? {
        let Some(id) = contact["id"].as_str() else {
            continue;
        };
        let Some(photo) = graph.contact_photo(id).map_err(|e| e.to_string())? else {
            continue;
        };
        with_photo += 1;
        let name = contact["displayName"].as_str().map(str::to_lowercase);
        let emails: Vec<String> = contact["emailAddresses"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["address"].as_str())
            .map(str::to_lowercase)
            .collect();
        let target = targets
            .iter()
            .find(|t| t.email.as_ref().is_some_and(|e| emails.contains(e)))
            .or_else(|| {
                targets.iter().find(|t| {
                    name.is_some() && t.display_name.as_deref().map(str::to_lowercase) == name
                })
            });
        match target {
            Some(target) if target.has_picture => {}
            Some(target) => actions.push(Action::ContactPhoto {
                item: target.id.clone(),
                subject: contact["displayName"].as_str().map(str::to_owned),
                photo: base64::engine::general_purpose::STANDARD.encode(photo),
            }),
            None => unmatched += 1,
        }
    }

    let mut text = String::new();
    for action in &actions {
        text.push_str(&serde_json::to_string(action).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "{with_photo} Outlook.com contacts have a photo. Plan: {} photos to set into {} ({unmatched} contacts not found in the target).",
        actions.len(),
        out.display()
    );
    println!(
        "Nothing was changed. Review the plan, then run `ecm apply --plan {} --dry-run`.",
        out.display()
    );
    Ok(())
}

#[cfg(not(windows))]
pub fn plan_contact_photos_command(_: &EwsArgs, _: &GraphArgs, _: &Path) -> Result<(), String> {
    Err("Microsoft Graph access needs Windows for now (WinHTTP)".into())
}

#[cfg(windows)]
pub fn sign_in(
    graph: &GraphArgs,
) -> Result<ecm_graph::GraphClient<ecm_graph::WinHttpTransport>, String> {
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
    Ok(ecm_graph::GraphClient::new(transport, token))
}

#[cfg(not(windows))]
pub fn plan_pim_command(_: &EwsArgs, _: &GraphArgs, _: &Path) -> Result<(), String> {
    Err("Microsoft Graph access needs Windows for now (WinHTTP)".into())
}

/// Whether two event subjects name the same event: equal, or one holding the
/// other, once reduced to ASCII letters and digits (a client may have mangled
/// accents into `?`, or prefixed the organizer's name).
pub(crate) fn same_subject(a: Option<&str>, b: Option<&str>) -> bool {
    let squash = |text: &str| -> String {
        normalize_subject(text)
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect()
    };
    match (a.map(squash), b.map(squash)) {
        (Some(a), Some(b)) if a == b => true,
        (Some(a), Some(b)) if a.len().min(b.len()) >= 8 => a.contains(&b) || b.contains(&a),
        _ => false,
    }
}

/// A Graph `dateTimeTimeZone` in UTC, as Unix seconds.
fn graph_time(value: &Value) -> Option<i64> {
    let text = value["dateTime"].as_str()?;
    parse_utc(&format!("{}Z", text.get(..19)?))
}

/// The Windows time zone id EWS needs for a Graph time zone name, when known.
/// UTC and custom zones give `None`: times then stay in UTC.
fn windows_zone(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case("utc") || name.starts_with("tzone://") {
        return None;
    }
    if !name.contains('/') {
        return Some(name.to_owned());
    }
    let windows = match name {
        "Europe/Paris" | "Europe/Brussels" | "Europe/Madrid" | "Europe/Copenhagen" => {
            "Romance Standard Time"
        }
        "Europe/Berlin" | "Europe/Amsterdam" | "Europe/Rome" | "Europe/Zurich"
        | "Europe/Vienna" | "Europe/Stockholm" | "Europe/Oslo" | "Europe/Luxembourg" => {
            "W. Europe Standard Time"
        }
        "Europe/London" | "Europe/Dublin" | "Europe/Lisbon" => "GMT Standard Time",
        "America/New_York" | "America/Toronto" | "America/Montreal" | "America/Detroit" => {
            "Eastern Standard Time"
        }
        "America/Chicago" | "America/Winnipeg" => "Central Standard Time",
        "America/Denver" | "America/Edmonton" => "Mountain Standard Time",
        "America/Los_Angeles" | "America/Vancouver" => "Pacific Standard Time",
        _ => return None,
    };
    Some(windows.to_owned())
}

/// An EWS `dateTime` from a Graph `dateTimeTimeZone`: local when `local`, else
/// UTC.
fn ews_time(value: &Value, local: bool) -> Option<String> {
    let text = value["dateTime"].as_str()?.get(..19)?;
    Some(if local {
        text.to_owned()
    } else {
        format!("{text}Z")
    })
}

fn element(name: &str, value: Option<&str>) -> String {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => format!("<t:{name}>{}</t:{name}>", escape(value)),
        None => String::new(),
    }
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn body(value: &Value) -> String {
    let content = value["content"].as_str().unwrap_or_default();
    if content.trim().is_empty() {
        return String::new();
    }
    let kind = if value["contentType"].as_str() == Some("html") {
        "HTML"
    } else {
        "Text"
    };
    format!(r#"<t:Body BodyType="{kind}">{}</t:Body>"#, escape(content))
}

fn categories(value: &Value, extra: Option<&str>) -> String {
    let mut names: Vec<&str> = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    names.extend(extra);
    if names.is_empty() {
        return String::new();
    }
    let strings: String = names
        .iter()
        .map(|name| format!("<t:String>{}</t:String>", escape(name)))
        .collect();
    format!("<t:Categories>{strings}</t:Categories>")
}

fn importance(value: &Value) -> String {
    element("Importance", value.as_str().map(capitalized).as_deref())
}

/// The EWS `<t:CalendarItem>` for a Graph event. With `zone`, the event's times
/// are local to that Windows zone (read with `outlook.timezone`); without, UTC.
pub fn event_xml(event: &Value, zone: Option<&str>) -> String {
    let local = zone.is_some();
    let mut xml = String::from("<t:CalendarItem>");
    xml += &element("Subject", event["subject"].as_str());
    xml += &element(
        "Sensitivity",
        event["sensitivity"].as_str().map(capitalized).as_deref(),
    );
    xml += &body(&event["body"]);
    xml += &categories(&event["categories"], None);
    xml += &importance(&event["importance"]);
    let reminder = event["isReminderOn"].as_bool().unwrap_or(false);
    xml += &format!("<t:ReminderIsSet>{reminder}</t:ReminderIsSet>");
    if reminder && let Some(minutes) = event["reminderMinutesBeforeStart"].as_i64() {
        xml += &format!("<t:ReminderMinutesBeforeStart>{minutes}</t:ReminderMinutesBeforeStart>");
    }
    xml += &element("UID", event["iCalUId"].as_str());
    xml += &element("Start", ews_time(&event["start"], local).as_deref());
    xml += &element("End", ews_time(&event["end"], local).as_deref());
    if event["isAllDay"].as_bool() == Some(true) {
        xml += "<t:IsAllDayEvent>true</t:IsAllDayEvent>";
    }
    let free_busy = match event["showAs"].as_str() {
        Some("free") => Some("Free"),
        Some("tentative") => Some("Tentative"),
        Some("busy") => Some("Busy"),
        Some("oof") => Some("OOF"),
        Some("workingElsewhere") => Some("WorkingElsewhere"),
        _ => None,
    };
    xml += &element("LegacyFreeBusyStatus", free_busy);
    xml += &element("Location", event["location"]["displayName"].as_str());
    // Attendees only for the user's own meetings: on an invitation the user
    // received, they would make the user its organizer.
    if event["isOrganizer"].as_bool() == Some(true) {
        for (kind, element_name) in [
            ("required", "RequiredAttendees"),
            ("optional", "OptionalAttendees"),
            ("resource", "Resources"),
        ] {
            let attendees: String = event["attendees"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| a["type"].as_str() == Some(kind))
                .filter_map(|a| {
                    let address = a["emailAddress"]["address"].as_str()?;
                    Some(format!(
                        "<t:Attendee><t:Mailbox>{}<t:EmailAddress>{}</t:EmailAddress></t:Mailbox></t:Attendee>",
                        element("Name", a["emailAddress"]["name"].as_str()),
                        escape(address)
                    ))
                })
                .collect();
            if !attendees.is_empty() {
                xml += &format!("<t:{element_name}>{attendees}</t:{element_name}>");
            }
        }
    }
    xml += &recurrence(&event["recurrence"]);
    let zone = zone.unwrap_or("UTC");
    xml += &format!(
        r#"<t:StartTimeZone Id="{0}"/><t:EndTimeZone Id="{0}"/>"#,
        escape(zone)
    );
    xml += "</t:CalendarItem>";
    xml
}

fn recurrence(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    let pattern = &value["pattern"];
    let interval = pattern["interval"].as_u64().unwrap_or(1).max(1);
    let days: String = pattern["daysOfWeek"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(capitalized)
        .collect::<Vec<_>>()
        .join(" ");
    let index = pattern["index"].as_str().map(capitalized);
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let month = pattern["month"]
        .as_u64()
        .and_then(|m| MONTHS.get((m as usize).wrapping_sub(1)))
        .copied();
    let day_of_month = pattern["dayOfMonth"].as_u64().unwrap_or(1);
    let first_day = pattern["firstDayOfWeek"]
        .as_str()
        .map(capitalized)
        .unwrap_or_else(|| "Monday".into());
    let pattern = match pattern["type"].as_str() {
        Some("daily") => {
            format!("<t:DailyRecurrence><t:Interval>{interval}</t:Interval></t:DailyRecurrence>")
        }
        Some("weekly") => format!(
            "<t:WeeklyRecurrence><t:Interval>{interval}</t:Interval><t:DaysOfWeek>{days}</t:DaysOfWeek><t:FirstDayOfWeek>{first_day}</t:FirstDayOfWeek></t:WeeklyRecurrence>"
        ),
        Some("absoluteMonthly") => format!(
            "<t:AbsoluteMonthlyRecurrence><t:Interval>{interval}</t:Interval><t:DayOfMonth>{day_of_month}</t:DayOfMonth></t:AbsoluteMonthlyRecurrence>"
        ),
        Some("relativeMonthly") => format!(
            "<t:RelativeMonthlyRecurrence><t:Interval>{interval}</t:Interval><t:DaysOfWeek>{days}</t:DaysOfWeek><t:DayOfWeekIndex>{}</t:DayOfWeekIndex></t:RelativeMonthlyRecurrence>",
            index.as_deref().unwrap_or("First")
        ),
        Some("absoluteYearly") => format!(
            "<t:AbsoluteYearlyRecurrence><t:DayOfMonth>{day_of_month}</t:DayOfMonth><t:Month>{}</t:Month></t:AbsoluteYearlyRecurrence>",
            month.unwrap_or("January")
        ),
        Some("relativeYearly") => format!(
            "<t:RelativeYearlyRecurrence><t:DaysOfWeek>{days}</t:DaysOfWeek><t:DayOfWeekIndex>{}</t:DayOfWeekIndex><t:Month>{}</t:Month></t:RelativeYearlyRecurrence>",
            index.as_deref().unwrap_or("First"),
            month.unwrap_or("January")
        ),
        _ => return String::new(),
    };
    let range = &value["range"];
    let start = range["startDate"].as_str().unwrap_or_default();
    let range = match range["type"].as_str() {
        Some("endDate") => format!(
            "<t:EndDateRecurrence><t:StartDate>{start}</t:StartDate><t:EndDate>{}</t:EndDate></t:EndDateRecurrence>",
            range["endDate"].as_str().unwrap_or(start)
        ),
        Some("numbered") => format!(
            "<t:NumberedRecurrence><t:StartDate>{start}</t:StartDate><t:NumberOfOccurrences>{}</t:NumberOfOccurrences></t:NumberedRecurrence>",
            range["numberOfOccurrences"].as_u64().unwrap_or(1)
        ),
        _ => format!("<t:NoEndRecurrence><t:StartDate>{start}</t:StartDate></t:NoEndRecurrence>"),
    };
    format!("<t:Recurrence>{pattern}{range}</t:Recurrence>")
}

/// The EWS `<t:Contact>` for a Graph contact.
pub fn contact_xml(contact: &Value) -> String {
    let text = |name: &str| contact[name].as_str();
    let mut xml = String::from("<t:Contact>");
    xml += &body(&serde_json::json!({
        "contentType": "text",
        "content": contact["personalNotes"],
    }));
    xml += &categories(&contact["categories"], None);
    xml += &element("FileAs", text("fileAs"));
    xml += &element("DisplayName", text("displayName"));
    xml += &element("GivenName", text("givenName"));
    xml += &element("Initials", text("initials"));
    xml += &element("MiddleName", text("middleName"));
    xml += &element("Nickname", text("nickName"));
    xml += &element("CompanyName", text("companyName"));
    let emails: String = contact["emailAddresses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["address"].as_str())
        .filter(|a| !a.trim().is_empty())
        .take(3)
        .enumerate()
        .map(|(i, address)| {
            format!(
                r#"<t:Entry Key="EmailAddress{}">{}</t:Entry>"#,
                i + 1,
                escape(address.trim())
            )
        })
        .collect();
    if !emails.is_empty() {
        xml += &format!("<t:EmailAddresses>{emails}</t:EmailAddresses>");
    }
    let addresses: String = [
        ("homeAddress", "Home"),
        ("businessAddress", "Business"),
        ("otherAddress", "Other"),
    ]
    .iter()
    .filter_map(|(field, key)| {
        let address = &contact[*field];
        let inner: String = [
            ("street", "Street"),
            ("city", "City"),
            ("state", "State"),
            ("countryOrRegion", "CountryOrRegion"),
            ("postalCode", "PostalCode"),
        ]
        .iter()
        .map(|(from, to)| element(to, address[*from].as_str()))
        .collect();
        (!inner.is_empty()).then(|| format!(r#"<t:Entry Key="{key}">{inner}</t:Entry>"#))
    })
    .collect();
    if !addresses.is_empty() {
        xml += &format!("<t:PhysicalAddresses>{addresses}</t:PhysicalAddresses>");
    }
    let list = |name: &str| -> Vec<String> {
        contact[name]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut phones: Vec<(&str, String)> = Vec::new();
    for (phone, key) in list("homePhones")
        .into_iter()
        .zip(["HomePhone", "HomePhone2"])
    {
        phones.push((key, phone));
    }
    for (phone, key) in list("businessPhones")
        .into_iter()
        .zip(["BusinessPhone", "BusinessPhone2"])
    {
        phones.push((key, phone));
    }
    if let Some(mobile) = text("mobilePhone").map(str::trim).filter(|m| !m.is_empty()) {
        phones.push(("MobilePhone", mobile.to_owned()));
    }
    if !phones.is_empty() {
        let entries: String = phones
            .iter()
            .map(|(key, phone)| format!(r#"<t:Entry Key="{key}">{}</t:Entry>"#, escape(phone)))
            .collect();
        xml += &format!("<t:PhoneNumbers>{entries}</t:PhoneNumbers>");
    }
    xml += &element("AssistantName", text("assistantName"));
    xml += &element("Birthday", text("birthday"));
    xml += &element("BusinessHomePage", text("businessHomePage"));
    let children = list("children");
    if !children.is_empty() {
        let strings: String = children
            .iter()
            .map(|c| format!("<t:String>{}</t:String>", escape(c)))
            .collect();
        xml += &format!("<t:Children>{strings}</t:Children>");
    }
    xml += &element("Department", text("department"));
    xml += &element("Generation", text("generation"));
    let im: String = list("imAddresses")
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, address)| {
            format!(
                r#"<t:Entry Key="ImAddress{}">{}</t:Entry>"#,
                i + 1,
                escape(address)
            )
        })
        .collect();
    if !im.is_empty() {
        xml += &format!("<t:ImAddresses>{im}</t:ImAddresses>");
    }
    xml += &element("JobTitle", text("jobTitle"));
    xml += &element("Manager", text("manager"));
    xml += &element("OfficeLocation", text("officeLocation"));
    xml += &element("Profession", text("profession"));
    xml += &element("SpouseName", text("spouseName"));
    xml += &element("Surname", text("surname"));
    xml += "</t:Contact>";
    xml
}

/// The EWS `<t:Task>` for a To Do task. `list` names a list other than the
/// default one, kept as a category.
pub fn task_xml(task: &Value, list: Option<&str>) -> String {
    let utc = |value: &Value| ews_time(value, false);
    let mut xml = String::from("<t:Task>");
    xml += &element("Subject", task["title"].as_str());
    xml += &body(&task["body"]);
    xml += &categories(&task["categories"], list);
    xml += &importance(&task["importance"]);
    let reminder = task["isReminderOn"].as_bool().unwrap_or(false);
    if reminder {
        xml += &element("ReminderDueBy", utc(&task["reminderDateTime"]).as_deref());
    }
    xml += &format!("<t:ReminderIsSet>{reminder}</t:ReminderIsSet>");
    let status = task["status"].as_str().unwrap_or("notStarted");
    if status == "completed" {
        xml += &element("CompleteDate", utc(&task["completedDateTime"]).as_deref());
    }
    xml += &element("DueDate", utc(&task["dueDateTime"]).as_deref());
    xml += &element("StartDate", utc(&task["startDateTime"]).as_deref());
    xml += &element("Status", Some(&capitalized(status)));
    xml += "</t:Task>";
    xml
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn maps_time_zones_to_windows_ids() {
        assert_eq!(
            windows_zone("Romance Standard Time").as_deref(),
            Some("Romance Standard Time")
        );
        assert_eq!(
            windows_zone("America/Toronto").as_deref(),
            Some("Eastern Standard Time")
        );
        assert_eq!(windows_zone("UTC"), None);
        assert_eq!(windows_zone("tzone://Microsoft/Custom"), None);
        assert_eq!(windows_zone("Asia/Unknown_City"), None);
    }

    #[test]
    fn converts_a_recurring_event_in_its_own_time_zone() {
        let event = json!({
            "subject": "Yoga & stretching",
            "body": {"contentType": "html", "content": "<p>Mat</p>"},
            "importance": "normal",
            "sensitivity": "private",
            "isReminderOn": true,
            "reminderMinutesBeforeStart": 15,
            "iCalUId": "040000008200E00074C5B7101A82E008",
            "start": {"dateTime": "2024-05-06T18:30:00.0000000", "timeZone": "Romance Standard Time"},
            "end": {"dateTime": "2024-05-06T19:30:00.0000000", "timeZone": "Romance Standard Time"},
            "isAllDay": false,
            "showAs": "busy",
            "location": {"displayName": "Studio"},
            "isOrganizer": false,
            "attendees": [{"type": "required", "emailAddress": {"name": "Ann", "address": "ann@example.test"}}],
            "recurrence": {
                "pattern": {"type": "weekly", "interval": 1, "daysOfWeek": ["monday", "thursday"], "firstDayOfWeek": "monday"},
                "range": {"type": "endDate", "startDate": "2024-05-06", "endDate": "2024-06-27"}
            }
        });
        let xml = event_xml(&event, Some("Romance Standard Time"));
        assert_eq!(
            xml,
            concat!(
                "<t:CalendarItem><t:Subject>Yoga &amp; stretching</t:Subject><t:Sensitivity>Private</t:Sensitivity>",
                r#"<t:Body BodyType="HTML">&lt;p&gt;Mat&lt;/p&gt;</t:Body><t:Importance>Normal</t:Importance>"#,
                "<t:ReminderIsSet>true</t:ReminderIsSet><t:ReminderMinutesBeforeStart>15</t:ReminderMinutesBeforeStart>",
                "<t:UID>040000008200E00074C5B7101A82E008</t:UID><t:Start>2024-05-06T18:30:00</t:Start><t:End>2024-05-06T19:30:00</t:End>",
                "<t:LegacyFreeBusyStatus>Busy</t:LegacyFreeBusyStatus><t:Location>Studio</t:Location>",
                "<t:Recurrence><t:WeeklyRecurrence><t:Interval>1</t:Interval><t:DaysOfWeek>Monday Thursday</t:DaysOfWeek><t:FirstDayOfWeek>Monday</t:FirstDayOfWeek></t:WeeklyRecurrence>",
                "<t:EndDateRecurrence><t:StartDate>2024-05-06</t:StartDate><t:EndDate>2024-06-27</t:EndDate></t:EndDateRecurrence></t:Recurrence>",
                r#"<t:StartTimeZone Id="Romance Standard Time"/><t:EndTimeZone Id="Romance Standard Time"/></t:CalendarItem>"#
            )
        );
    }

    #[test]
    fn keeps_attendees_of_own_meetings_and_utc_without_a_zone() {
        let event = json!({
            "subject": "Lunch",
            "start": {"dateTime": "2024-05-06T10:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2024-05-06T11:00:00.0000000", "timeZone": "UTC"},
            "isAllDay": true,
            "isOrganizer": true,
            "attendees": [
                {"type": "required", "emailAddress": {"name": "Ann", "address": "ann@example.test"}},
                {"type": "optional", "emailAddress": {"address": "bob@example.test"}}
            ],
            "recurrence": null
        });
        let xml = event_xml(&event, None);
        assert!(xml.contains("<t:Start>2024-05-06T10:00:00Z</t:Start>"));
        assert!(xml.contains("<t:IsAllDayEvent>true</t:IsAllDayEvent>"));
        assert!(xml.contains("<t:RequiredAttendees><t:Attendee><t:Mailbox><t:Name>Ann</t:Name><t:EmailAddress>ann@example.test</t:EmailAddress></t:Mailbox></t:Attendee></t:RequiredAttendees><t:OptionalAttendees><t:Attendee><t:Mailbox><t:EmailAddress>bob@example.test</t:EmailAddress>"));
        assert!(xml.contains(r#"<t:StartTimeZone Id="UTC"/>"#));
        assert!(!xml.contains("Recurrence"));
    }

    #[test]
    fn converts_yearly_and_numbered_recurrences() {
        let xml = recurrence(&json!({
            "pattern": {"type": "relativeYearly", "interval": 1, "month": 11, "daysOfWeek": ["thursday"], "index": "fourth"},
            "range": {"type": "numbered", "startDate": "2024-11-28", "numberOfOccurrences": 5}
        }));
        assert_eq!(
            xml,
            "<t:Recurrence><t:RelativeYearlyRecurrence><t:DaysOfWeek>Thursday</t:DaysOfWeek><t:DayOfWeekIndex>Fourth</t:DayOfWeekIndex><t:Month>November</t:Month></t:RelativeYearlyRecurrence><t:NumberedRecurrence><t:StartDate>2024-11-28</t:StartDate><t:NumberOfOccurrences>5</t:NumberOfOccurrences></t:NumberedRecurrence></t:Recurrence>"
        );
    }

    #[test]
    fn converts_a_contact_in_schema_order() {
        let contact = json!({
            "displayName": "Ann Example",
            "givenName": "Ann",
            "surname": "Example",
            "companyName": "Acme",
            "personalNotes": "Met at the fair",
            "emailAddresses": [{"address": "ann@example.test"}, {"address": "ann@work.test"}],
            "homePhones": ["+33 1 00 00 00 00"],
            "businessPhones": [],
            "mobilePhone": "+33 6 00 00 00 00",
            "homeAddress": {"street": "1 rue X", "city": "Paris", "postalCode": "75001"},
            "businessAddress": {},
            "birthday": "1980-05-12T11:59:00Z",
            "children": ["Leo"],
            "categories": ["Famille"]
        });
        assert_eq!(
            contact_xml(&contact),
            concat!(
                r#"<t:Contact><t:Body BodyType="Text">Met at the fair</t:Body><t:Categories><t:String>Famille</t:String></t:Categories>"#,
                "<t:DisplayName>Ann Example</t:DisplayName><t:GivenName>Ann</t:GivenName><t:CompanyName>Acme</t:CompanyName>",
                r#"<t:EmailAddresses><t:Entry Key="EmailAddress1">ann@example.test</t:Entry><t:Entry Key="EmailAddress2">ann@work.test</t:Entry></t:EmailAddresses>"#,
                r#"<t:PhysicalAddresses><t:Entry Key="Home"><t:Street>1 rue X</t:Street><t:City>Paris</t:City><t:PostalCode>75001</t:PostalCode></t:Entry></t:PhysicalAddresses>"#,
                r#"<t:PhoneNumbers><t:Entry Key="HomePhone">+33 1 00 00 00 00</t:Entry><t:Entry Key="MobilePhone">+33 6 00 00 00 00</t:Entry></t:PhoneNumbers>"#,
                "<t:Birthday>1980-05-12T11:59:00Z</t:Birthday><t:Children><t:String>Leo</t:String></t:Children>",
                "<t:Surname>Example</t:Surname></t:Contact>"
            )
        );
    }

    #[test]
    fn converts_a_completed_task_of_another_list() {
        let task = json!({
            "title": "Renew passport",
            "body": {"contentType": "text", "content": ""},
            "importance": "high",
            "status": "completed",
            "isReminderOn": false,
            "completedDateTime": {"dateTime": "2024-03-01T00:00:00.0000000", "timeZone": "UTC"},
            "dueDateTime": {"dateTime": "2024-02-28T00:00:00.0000000", "timeZone": "UTC"}
        });
        assert_eq!(
            task_xml(&task, Some("Family")),
            concat!(
                "<t:Task><t:Subject>Renew passport</t:Subject><t:Categories><t:String>Family</t:String></t:Categories>",
                "<t:Importance>High</t:Importance><t:ReminderIsSet>false</t:ReminderIsSet>",
                "<t:CompleteDate>2024-03-01T00:00:00Z</t:CompleteDate><t:DueDate>2024-02-28T00:00:00Z</t:DueDate>",
                "<t:Status>Completed</t:Status></t:Task>"
            )
        );
    }

    #[test]
    fn matches_subjects_a_client_mangled_or_prefixed() {
        assert!(same_subject(
            Some("L'offre de service de la modalité Suivi"),
            Some("France Travail : L'offre de service de la modalit? Suivi")
        ));
        assert!(same_subject(Some("RE: Dentist"), Some("Dentist")));
        assert!(!same_subject(Some("Vélo"), Some("Sortie Vélo")));
        assert!(!same_subject(Some("Dentist"), None));
    }

    #[test]
    fn reads_graph_times_as_utc() {
        assert_eq!(
            graph_time(&json!({"dateTime": "2024-05-06T10:00:00.0000000", "timeZone": "UTC"})),
            parse_utc("2024-05-06T10:00:00Z")
        );
    }
}
