//! `ecm fidelity-graph-pim`: copy a few calendar items, contacts, tasks, inbox
//! rules and categories of an Exchange mailbox, read over EWS, into test
//! containers of a mailbox reached through Microsoft Graph, read each copy back
//! and compare it with its source property by property (milestone M9b).
//!
//! It only creates items: in the calendar, the contact folder and the To Do list
//! named [`TEST_FOLDER`](crate::fidelity_cmd::TEST_FOLDER), and rules (disabled)
//! and categories named with [`TEST_PREFIX`]; nothing else in the mailbox is
//! changed. Events are created without their attendees: Graph would send them
//! invitations.

use std::collections::BTreeSet;

use ecm_ews::xml::Element;
use ecm_model::{
    compare::{PropertyMap, address_list, compare_on},
    item::{format_iso, parse_utc},
};
use serde_json::{Map, Value, json};

use crate::content_cmd::{collapse_spaces, html_text};

/// Calendar item properties compared.
pub const EVENT_COMPARED: &[&str] = &[
    "subject",
    "start",
    "end",
    "time_zone",
    "all_day",
    "show_as",
    "sensitivity",
    "importance",
    "categories",
    "reminder",
    "location",
    "body",
    "recurrence",
];

/// Contact properties compared.
pub const CONTACT_COMPARED: &[&str] = &[
    "display_name",
    "file_as",
    "given_name",
    "middle_name",
    "surname",
    "initials",
    "nickname",
    "generation",
    "company",
    "department",
    "job_title",
    "profession",
    "manager",
    "assistant",
    "office_location",
    "home_page",
    "spouse",
    "children",
    "emails",
    "im_addresses",
    "home_phones",
    "business_phones",
    "mobile_phone",
    "home_address",
    "business_address",
    "other_address",
    "birthday",
    "notes",
    "categories",
    "photo",
];

/// Task properties compared.
pub const TASK_COMPARED: &[&str] = &[
    "subject",
    "body",
    "importance",
    "status",
    "start",
    "due",
    "completed",
    "reminder",
    "categories",
    "recurrence",
];

/// Contact fields copied as they are: EWS element, Graph property, compared
/// property.
const CONTACT_FIELDS: &[(&str, &str, &str)] = &[
    ("DisplayName", "displayName", "display_name"),
    ("FileAs", "fileAs", "file_as"),
    ("GivenName", "givenName", "given_name"),
    ("MiddleName", "middleName", "middle_name"),
    ("Surname", "surname", "surname"),
    ("Initials", "initials", "initials"),
    ("Nickname", "nickName", "nickname"),
    ("Generation", "generation", "generation"),
    ("CompanyName", "companyName", "company"),
    ("Department", "department", "department"),
    ("JobTitle", "jobTitle", "job_title"),
    ("Profession", "profession", "profession"),
    ("Manager", "manager", "manager"),
    ("AssistantName", "assistantName", "assistant"),
    ("OfficeLocation", "officeLocation", "office_location"),
    ("BusinessHomePage", "businessHomePage", "home_page"),
    ("SpouseName", "spouseName", "spouse"),
];

/// Phone numbers Graph holds, by EWS key.
const HOME_PHONES: [&str; 2] = ["HomePhone", "HomePhone2"];
const BUSINESS_PHONES: [&str; 2] = ["BusinessPhone", "BusinessPhone2"];

/// Postal addresses: EWS key, Graph property, compared property.
const ADDRESSES: [(&str, &str, &str); 3] = [
    ("Home", "homeAddress", "home_address"),
    ("Business", "businessAddress", "business_address"),
    ("Other", "otherAddress", "other_address"),
];

/// Parts of a postal address: EWS element, Graph property.
const ADDRESS_PARTS: [(&str, &str); 5] = [
    ("Street", "street"),
    ("City", "city"),
    ("State", "state"),
    ("PostalCode", "postalCode"),
    ("CountryOrRegion", "countryOrRegion"),
];

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

const WEEK: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

/// Trimmed text of a child, when not empty.
fn text(item: &Element, name: &str) -> Option<String> {
    item.child_text(name)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// `WorkingElsewhere` → `workingElsewhere`: EWS enumerations as Graph writes them.
fn lower_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A Windows time zone id as Graph takes it: EWS also names UTC
/// `tzone://Microsoft/Utc`.
fn zone_id(id: &str) -> String {
    if id.eq_ignore_ascii_case("tzone://Microsoft/Utc") {
        "UTC".into()
    } else {
        id.to_owned()
    }
}

/// A time for Graph and its zone: the wall clock time in the item's own zone
/// (Exchange 2016 and later), else the UTC time.
fn local_time(item: &Element, wall_clock: &str, utc: &str, zone: &str) -> Option<(String, String)> {
    if let (Some(zone), Some(wall)) = (text(item, zone), text(item, wall_clock))
        && let Some(local) = wall.get(..19)
    {
        return Some((local.to_owned(), zone_id(&zone)));
    }
    Some((text(item, utc)?.get(..19)?.to_owned(), "UTC".into()))
}

/// An EWS UTC time as ISO 8601 UTC.
fn utc(item: &Element, name: &str) -> Option<String> {
    text(item, name).and_then(|t| parse_utc(&t)).map(format_iso)
}

/// A Graph `dateTimeTimeZone` read in UTC, as ISO 8601 UTC.
fn graph_utc(value: &Value) -> Option<String> {
    let time = value["dateTime"].as_str()?;
    parse_utc(&format!("{}Z", time.get(..19)?)).map(format_iso)
}

/// The day of a Graph `dateTimeTimeZone`, as written: an all-day event's days
/// are the same in every zone, and Graph gives them as they were set.
fn day(value: &Value) -> Option<String> {
    value["dateTime"].as_str()?.get(..10).map(str::to_owned)
}

/// The text a body shows, spaces collapsed.
fn shown_text(content: &str, html: bool) -> String {
    if html {
        html_text(content.as_bytes())
    } else {
        collapse_spaces(content)
    }
}

fn ews_body(item: &Element) -> Option<Value> {
    let body = item.child("Body")?;
    if body.text.trim().is_empty() {
        return None;
    }
    let kind = if body.attribute("BodyType") == Some("HTML") {
        "html"
    } else {
        "text"
    };
    Some(json!({ "contentType": kind, "content": body.text }))
}

fn ews_body_text(item: &Element) -> String {
    item.child("Body")
        .map(|b| shown_text(&b.text, b.attribute("BodyType") == Some("HTML")))
        .unwrap_or_default()
}

fn graph_body_text(body: &Value) -> String {
    shown_text(
        body["content"].as_str().unwrap_or_default(),
        body["contentType"].as_str() == Some("html"),
    )
}

/// The `t:String` values of a list element (categories, children).
fn strings(item: &Element, name: &str) -> Vec<String> {
    item.child(name)
        .map(|list| {
            list.children_named("String")
                .map(|s| s.text.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn json_strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn sorted(mut values: Vec<String>) -> String {
    values.sort();
    values.join(";")
}

/// Sets `key` when there is a value.
fn set(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(key.into(), Value::String(value));
    }
}

/// Sets a compared property when it is not empty: an empty one is not compared.
fn put(props: &mut PropertyMap, name: &str, value: Option<String>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        props.insert(name.into(), value);
    }
}

/// The entries of an EWS dictionary (`EmailAddresses`, `PhoneNumbers`…), by key.
fn entries<'a>(item: &'a Element, name: &str) -> Vec<(&'a str, &'a Element)> {
    item.child(name)
        .map(|list| {
            list.children_named("Entry")
                .filter_map(|e| Some((e.attribute("Key")?, e)))
                .collect()
        })
        .unwrap_or_default()
}

fn entry_text(item: &Element, name: &str, key: &str) -> Option<String> {
    entries(item, name)
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, e)| e.text.trim().to_owned())
        .filter(|t| !t.is_empty())
}

/// SHA-256 of a photo, short.
fn digest(data: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(data)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

// Calendar items.

/// The Graph event for an EWS calendar item, and what of it Graph does not get.
pub fn graph_event(item: &Element) -> (Value, Vec<String>) {
    let mut event = Map::new();
    let mut left_out = Vec::new();
    set(&mut event, "subject", text(item, "Subject"));
    if let Some(body) = ews_body(item) {
        event.insert("body".into(), body);
    }
    let start = local_time(item, "StartWallClock", "Start", "StartTimeZoneId");
    for (name, time) in [
        ("start", &start),
        (
            "end",
            &local_time(item, "EndWallClock", "End", "EndTimeZoneId"),
        ),
    ] {
        if let Some((time, zone)) = time {
            event.insert(name.into(), json!({ "dateTime": time, "timeZone": zone }));
        }
    }
    event.insert(
        "isAllDay".into(),
        json!(text(item, "IsAllDayEvent").as_deref() == Some("true")),
    );
    let show_as = text(item, "LegacyFreeBusyStatus").map(|status| match status.as_str() {
        "NoData" => "unknown".into(),
        "OOF" => "oof".into(),
        other => lower_first(other),
    });
    set(&mut event, "showAs", show_as);
    set(
        &mut event,
        "sensitivity",
        text(item, "Sensitivity").map(|s| s.to_lowercase()),
    );
    set(
        &mut event,
        "importance",
        text(item, "Importance").map(|s| s.to_lowercase()),
    );
    let categories = strings(item, "Categories");
    if !categories.is_empty() {
        event.insert("categories".into(), json!(categories));
    }
    let reminder = text(item, "ReminderIsSet").as_deref() == Some("true");
    event.insert("isReminderOn".into(), json!(reminder));
    if reminder
        && let Some(minutes) =
            text(item, "ReminderMinutesBeforeStart").and_then(|m| m.parse::<i64>().ok())
    {
        event.insert("reminderMinutesBeforeStart".into(), json!(minutes));
    }
    if let Some(location) = text(item, "Location") {
        event.insert("location".into(), json!({ "displayName": location }));
    }
    if let Some(recurrence) = item.child("Recurrence") {
        let zone = start.as_ref().map_or("UTC", |(_, zone)| zone.as_str());
        match graph_recurrence(recurrence, zone) {
            Some(recurrence) => {
                event.insert("recurrence".into(), recurrence);
            }
            None => left_out.push("its recurrence (a pattern Graph does not have)".into()),
        }
    }
    if text(item, "IsOrganizer").as_deref() == Some("false") {
        left_out.push("its organizer: the copy is the user's own event".into());
    }
    let attendees = attendees(item).len();
    if attendees > 0 {
        left_out.push(format!(
            "its attendees ({attendees}): Graph would send them invitations"
        ));
    }
    for (name, what) in [
        ("ModifiedOccurrences", "modified occurrences"),
        ("DeletedOccurrences", "deleted occurrences"),
    ] {
        let count = item.child(name).map_or(0, |l| l.children.len());
        if count > 0 {
            left_out.push(format!("{count} {what} of the series"));
        }
    }
    (Value::Object(event), left_out)
}

/// The addresses of an event's attendees, but its organizer's: a client often
/// lists the organizer too, and the copy is the organizer's own event.
fn attendees(item: &Element) -> Vec<String> {
    let organizer = item
        .path(&["Organizer", "Mailbox", "EmailAddress"])
        .map(|e| e.text.trim().to_lowercase());
    ["RequiredAttendees", "OptionalAttendees", "Resources"]
        .iter()
        .filter_map(|name| item.child(name))
        .flat_map(|list| list.children_named("Attendee"))
        .filter_map(|a| a.path(&["Mailbox", "EmailAddress"]))
        .map(|e| e.text.trim().to_owned())
        .filter(|a| !a.is_empty() && Some(a.to_lowercase()) != organizer)
        .collect()
}

/// Graph's `patternedRecurrence` for an EWS recurrence, in `zone`; `None` for a
/// pattern Graph does not have (a task regenerating after its completion).
fn graph_recurrence(recurrence: &Element, zone: &str) -> Option<Value> {
    let mut pattern = None;
    let mut range = None;
    for part in &recurrence.children {
        let number = |name: &str, default: u64| {
            text(part, name)
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(default)
        };
        let interval = number("Interval", 1);
        let days = || days_of_week(text(part, "DaysOfWeek").as_deref().unwrap_or_default());
        let index = || text(part, "DayOfWeekIndex").map_or("first".into(), |i| i.to_lowercase());
        let month = || {
            text(part, "Month")
                .and_then(|m| MONTHS.iter().position(|name| *name == m))
                .map_or(1, |i| i + 1)
        };
        let date = |name: &str| text(part, name).and_then(|d| d.get(..10).map(str::to_owned));
        match part.name.as_str() {
            "DailyRecurrence" => pattern = Some(json!({ "type": "daily", "interval": interval })),
            "WeeklyRecurrence" => {
                pattern = Some(json!({
                    "type": "weekly",
                    "interval": interval,
                    "daysOfWeek": days(),
                    "firstDayOfWeek": text(part, "FirstDayOfWeek")
                        .map_or("sunday".into(), |d| d.to_lowercase()),
                }));
            }
            "AbsoluteMonthlyRecurrence" => {
                pattern = Some(json!({
                    "type": "absoluteMonthly",
                    "interval": interval,
                    "dayOfMonth": number("DayOfMonth", 1),
                }));
            }
            "RelativeMonthlyRecurrence" => {
                pattern = Some(json!({
                    "type": "relativeMonthly",
                    "interval": interval,
                    "daysOfWeek": days(),
                    "index": index(),
                }));
            }
            "AbsoluteYearlyRecurrence" => {
                pattern = Some(json!({
                    "type": "absoluteYearly",
                    "interval": 1,
                    "dayOfMonth": number("DayOfMonth", 1),
                    "month": month(),
                }));
            }
            "RelativeYearlyRecurrence" => {
                pattern = Some(json!({
                    "type": "relativeYearly",
                    "interval": 1,
                    "daysOfWeek": days(),
                    "index": index(),
                    "month": month(),
                }));
            }
            "NoEndRecurrence" => {
                range = Some(json!({ "type": "noEnd", "startDate": date("StartDate") }));
            }
            "EndDateRecurrence" => {
                range = Some(json!({
                    "type": "endDate",
                    "startDate": date("StartDate"),
                    "endDate": date("EndDate"),
                }));
            }
            "NumberedRecurrence" => {
                range = Some(json!({
                    "type": "numbered",
                    "startDate": date("StartDate"),
                    "numberOfOccurrences": number("NumberOfOccurrences", 1),
                }));
            }
            _ => return None,
        }
    }
    let mut range = range?;
    range["recurrenceTimeZone"] = json!(zone);
    Some(json!({ "pattern": pattern?, "range": range }))
}

/// EWS days of the week (`Monday Thursday`, or `Day`, `Weekday`, `WeekendDay`)
/// as Graph's.
fn days_of_week(days: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    for day in days.split_whitespace() {
        match day {
            "Day" => out.extend(WEEK),
            "Weekday" => out.extend(&WEEK[1..6]),
            "WeekendDay" => out.extend([WEEK[0], WEEK[6]]),
            other => out.extend(WEEK.iter().find(|w| w.eq_ignore_ascii_case(other))),
        }
    }
    out
}

/// A recurrence in words, the same for the Graph JSON sent and read back: only
/// what its pattern and range types use.
fn recurrence_text(recurrence: &Value) -> String {
    if recurrence.is_null() {
        return String::new();
    }
    let pattern = &recurrence["pattern"];
    let range = &recurrence["range"];
    let kind = pattern["type"].as_str().unwrap_or_default();
    let days = sorted(json_strings(&pattern["daysOfWeek"]));
    let index = pattern["index"].as_str().unwrap_or_default();
    let mut text = format!("{kind} every {}", pattern["interval"].as_u64().unwrap_or(1));
    match kind {
        "weekly" => text += &format!(" on {days}"),
        "absoluteMonthly" => text += &format!(" on day {}", pattern["dayOfMonth"]),
        "relativeMonthly" => text += &format!(" on the {index} {days}"),
        "absoluteYearly" => {
            text += &format!(" on {}/{}", pattern["month"], pattern["dayOfMonth"]);
        }
        "relativeYearly" => {
            text += &format!(" on the {index} {days} of month {}", pattern["month"]);
        }
        _ => {}
    }
    text += &format!(" from {}", range["startDate"].as_str().unwrap_or_default());
    match range["type"].as_str() {
        Some("endDate") => {
            text += &format!(" to {}", range["endDate"].as_str().unwrap_or_default())
        }
        Some("numbered") => text += &format!(", {} times", range["numberOfOccurrences"]),
        _ => {}
    }
    text
}

fn reminder_text(event: &Value) -> String {
    if event["isReminderOn"].as_bool() == Some(true) {
        format!(
            "{} min before",
            event["reminderMinutesBeforeStart"].as_i64().unwrap_or(0)
        )
    } else {
        "off".into()
    }
}

/// The compared properties of an EWS calendar item.
pub fn ews_event_properties(item: &Element) -> PropertyMap {
    let (event, _) = graph_event(item);
    let mut props = PropertyMap::new();
    put(&mut props, "subject", text(item, "Subject"));
    // An all-day event's days are the same in every zone: Graph may keep it
    // in UTC.
    if event["isAllDay"].as_bool() == Some(true) {
        put(&mut props, "start", day(&event["start"]));
        put(&mut props, "end", day(&event["end"]));
    } else {
        put(&mut props, "start", utc(item, "Start"));
        put(&mut props, "end", utc(item, "End"));
        put(
            &mut props,
            "time_zone",
            event["start"]["timeZone"].as_str().map(str::to_owned),
        );
    }
    put(&mut props, "all_day", Some(event["isAllDay"].to_string()));
    for (name, field) in [
        ("show_as", "showAs"),
        ("sensitivity", "sensitivity"),
        ("importance", "importance"),
    ] {
        put(&mut props, name, event[field].as_str().map(str::to_owned));
    }
    put(
        &mut props,
        "categories",
        Some(sorted(strings(item, "Categories"))),
    );
    put(&mut props, "reminder", Some(reminder_text(&event)));
    put(&mut props, "location", text(item, "Location"));
    put(&mut props, "body", Some(ews_body_text(item)));
    put(
        &mut props,
        "recurrence",
        Some(recurrence_text(&event["recurrence"])),
    );
    put(
        &mut props,
        "attendees",
        Some(address_list(attendees(item).iter().map(String::as_str))),
    );
    props
}

/// The compared properties of a Graph event, read in UTC.
pub fn graph_event_properties(event: &Value) -> PropertyMap {
    let mut props = PropertyMap::new();
    let string = |field: &str| event[field].as_str().map(str::to_owned);
    put(&mut props, "subject", string("subject"));
    if event["isAllDay"].as_bool() == Some(true) {
        put(&mut props, "start", day(&event["start"]));
        put(&mut props, "end", day(&event["end"]));
    } else {
        put(&mut props, "start", graph_utc(&event["start"]));
        put(&mut props, "end", graph_utc(&event["end"]));
        put(
            &mut props,
            "time_zone",
            event["originalStartTimeZone"].as_str().map(zone_id),
        );
    }
    put(&mut props, "all_day", Some(event["isAllDay"].to_string()));
    put(&mut props, "show_as", string("showAs"));
    put(&mut props, "sensitivity", string("sensitivity"));
    put(&mut props, "importance", string("importance"));
    put(
        &mut props,
        "categories",
        Some(sorted(json_strings(&event["categories"]))),
    );
    put(&mut props, "reminder", Some(reminder_text(event)));
    put(
        &mut props,
        "location",
        event["location"]["displayName"].as_str().map(str::to_owned),
    );
    put(&mut props, "body", Some(graph_body_text(&event["body"])));
    put(
        &mut props,
        "recurrence",
        Some(recurrence_text(&event["recurrence"])),
    );
    let attendees: Vec<&str> = event["attendees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["emailAddress"]["address"].as_str())
        .collect();
    put(&mut props, "attendees", Some(address_list(attendees)));
    props
}

// Contacts.

/// The Graph contact for an EWS contact, and what of it Graph does not get. The
/// photo goes separately.
pub fn graph_contact(item: &Element) -> (Value, Vec<String>) {
    let mut contact = Map::new();
    let mut left_out = Vec::new();
    for (ews, graph, _) in CONTACT_FIELDS {
        set(&mut contact, graph, text(item, ews));
    }
    set(&mut contact, "birthday", text(item, "Birthday"));
    for (ews, graph) in [("Children", "children"), ("Categories", "categories")] {
        let values = strings(item, ews);
        if !values.is_empty() {
            contact.insert(graph.into(), json!(values));
        }
    }
    let emails: Vec<Value> = (1..=3)
        .filter_map(|n| {
            let key = format!("EmailAddress{n}");
            let (_, entry) = entries(item, "EmailAddresses")
                .into_iter()
                .find(|(k, _)| *k == key)?;
            let address = entry.text.trim();
            if address.is_empty() {
                return None;
            }
            let mut email = json!({ "address": address });
            if let Some(name) = entry.attribute("Name").filter(|n| !n.trim().is_empty()) {
                email["name"] = json!(name);
            }
            Some(email)
        })
        .collect();
    if !emails.is_empty() {
        contact.insert("emailAddresses".into(), json!(emails));
    }
    let im: Vec<String> = (1..=3)
        .filter_map(|n| entry_text(item, "ImAddresses", &format!("ImAddress{n}")))
        .collect();
    if !im.is_empty() {
        contact.insert("imAddresses".into(), json!(im));
    }
    for (keys, graph) in [
        (HOME_PHONES, "homePhones"),
        (BUSINESS_PHONES, "businessPhones"),
    ] {
        let phones: Vec<String> = keys
            .iter()
            .filter_map(|key| entry_text(item, "PhoneNumbers", key))
            .collect();
        if !phones.is_empty() {
            contact.insert(graph.into(), json!(phones));
        }
    }
    set(
        &mut contact,
        "mobilePhone",
        entry_text(item, "PhoneNumbers", "MobilePhone"),
    );
    for (key, entry) in entries(item, "PhoneNumbers") {
        let known =
            HOME_PHONES.contains(&key) || BUSINESS_PHONES.contains(&key) || key == "MobilePhone";
        if !known && !entry.text.trim().is_empty() {
            left_out.push(format!("its {key} number (no such field in Graph)"));
        }
    }
    for (key, graph, _) in ADDRESSES {
        let Some((_, entry)) = entries(item, "PhysicalAddresses")
            .into_iter()
            .find(|(k, _)| *k == key)
        else {
            continue;
        };
        let mut address = Map::new();
        for (ews, part) in ADDRESS_PARTS {
            set(&mut address, part, text(entry, ews));
        }
        if !address.is_empty() {
            contact.insert(graph.into(), Value::Object(address));
        }
    }
    if let Some(body) = item.child("Body").filter(|b| !b.text.trim().is_empty()) {
        let notes = if body.attribute("BodyType") == Some("HTML") {
            html_text(body.text.as_bytes())
        } else {
            body.text.clone()
        };
        contact.insert("personalNotes".into(), json!(notes));
    }
    (Value::Object(contact), left_out)
}

/// Whether an EWS contact has a photo.
pub fn has_photo(item: &Element) -> bool {
    item.child("Attachments").is_some_and(|attachments| {
        attachments
            .children_named("FileAttachment")
            .any(|a| a.child_text("IsContactPhoto") == Some("true"))
    })
}

fn address_text(parts: [Option<String>; 5]) -> String {
    parts
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect::<Vec<_>>()
        .join(" | ")
        .trim_matches([' ', '|'])
        .to_owned()
}

/// The compared properties of an EWS contact, with its photo.
pub fn ews_contact_properties(item: &Element, photo: Option<&[u8]>) -> PropertyMap {
    let mut props = PropertyMap::new();
    for (ews, _, name) in CONTACT_FIELDS {
        put(&mut props, name, text(item, ews));
    }
    put(
        &mut props,
        "children",
        Some(sorted(strings(item, "Children"))),
    );
    put(
        &mut props,
        "categories",
        Some(sorted(strings(item, "Categories"))),
    );
    let emails: Vec<String> = entries(item, "EmailAddresses")
        .into_iter()
        .map(|(_, e)| e.text.clone())
        .collect();
    put(
        &mut props,
        "emails",
        Some(address_list(emails.iter().map(String::as_str))),
    );
    let im: Vec<String> = entries(item, "ImAddresses")
        .into_iter()
        .map(|(_, e)| e.text.trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    put(&mut props, "im_addresses", Some(sorted(im)));
    for (keys, name) in [
        (HOME_PHONES, "home_phones"),
        (BUSINESS_PHONES, "business_phones"),
    ] {
        let phones: Vec<String> = keys
            .iter()
            .filter_map(|key| entry_text(item, "PhoneNumbers", key))
            .collect();
        put(&mut props, name, Some(phones.join(";")));
    }
    put(
        &mut props,
        "mobile_phone",
        entry_text(item, "PhoneNumbers", "MobilePhone"),
    );
    for (key, _, name) in ADDRESSES {
        if let Some((_, entry)) = entries(item, "PhysicalAddresses")
            .into_iter()
            .find(|(k, _)| *k == key)
        {
            put(
                &mut props,
                name,
                Some(address_text(ADDRESS_PARTS.map(|(ews, _)| text(entry, ews)))),
            );
        }
    }
    put(
        &mut props,
        "birthday",
        text(item, "Birthday").and_then(|b| b.get(..10).map(str::to_owned)),
    );
    put(&mut props, "notes", Some(ews_body_text(item)));
    put(&mut props, "photo", photo.map(digest));
    props
}

/// The compared properties of a Graph contact, with its photo.
pub fn graph_contact_properties(contact: &Value, photo: Option<&[u8]>) -> PropertyMap {
    let mut props = PropertyMap::new();
    for (_, graph, name) in CONTACT_FIELDS {
        put(&mut props, name, contact[graph].as_str().map(str::to_owned));
    }
    put(
        &mut props,
        "children",
        Some(sorted(json_strings(&contact["children"]))),
    );
    put(
        &mut props,
        "categories",
        Some(sorted(json_strings(&contact["categories"]))),
    );
    let emails: Vec<&str> = contact["emailAddresses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["address"].as_str())
        .collect();
    put(&mut props, "emails", Some(address_list(emails)));
    put(
        &mut props,
        "im_addresses",
        Some(sorted(json_strings(&contact["imAddresses"]))),
    );
    put(
        &mut props,
        "home_phones",
        Some(json_strings(&contact["homePhones"]).join(";")),
    );
    put(
        &mut props,
        "business_phones",
        Some(json_strings(&contact["businessPhones"]).join(";")),
    );
    put(
        &mut props,
        "mobile_phone",
        contact["mobilePhone"].as_str().map(str::to_owned),
    );
    for (_, graph, name) in ADDRESSES {
        let address = &contact[graph];
        put(
            &mut props,
            name,
            Some(address_text(ADDRESS_PARTS.map(|(_, part)| {
                address[part]
                    .as_str()
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
            }))),
        );
    }
    put(
        &mut props,
        "birthday",
        contact["birthday"]
            .as_str()
            .and_then(|b| b.get(..10).map(str::to_owned)),
    );
    put(
        &mut props,
        "notes",
        contact["personalNotes"].as_str().map(collapse_spaces),
    );
    put(&mut props, "photo", photo.map(digest));
    props
}

// Tasks.

/// The To Do task for an EWS task, and what of it To Do does not get.
pub fn graph_task(item: &Element) -> (Value, Vec<String>) {
    let mut task = Map::new();
    let mut left_out = Vec::new();
    set(&mut task, "title", text(item, "Subject"));
    if let Some(body) = ews_body(item) {
        task.insert("body".into(), body);
    }
    set(
        &mut task,
        "importance",
        text(item, "Importance").map(|s| s.to_lowercase()),
    );
    let status = text(item, "Status").map(|s| lower_first(&s));
    let completed = status.as_deref() == Some("completed");
    set(&mut task, "status", status);
    let categories = strings(item, "Categories");
    if !categories.is_empty() {
        task.insert("categories".into(), json!(categories));
    }
    let mut times = vec![("StartDate", "startDateTime"), ("DueDate", "dueDateTime")];
    if completed {
        times.push(("CompleteDate", "completedDateTime"));
    }
    let reminder = text(item, "ReminderIsSet").as_deref() == Some("true");
    if reminder {
        times.push(("ReminderDueBy", "reminderDateTime"));
    }
    task.insert("isReminderOn".into(), json!(reminder));
    for (ews, graph) in times {
        if let Some(time) = utc(item, ews) {
            task.insert(
                graph.into(),
                json!({ "dateTime": time.trim_end_matches('Z'), "timeZone": "UTC" }),
            );
        }
    }
    if let Some(recurrence) = item.child("Recurrence") {
        match graph_recurrence(recurrence, "UTC") {
            Some(recurrence) => {
                task.insert("recurrence".into(), recurrence);
            }
            None => {
                left_out.push("its recurrence (regenerating: To Do has no such pattern)".into())
            }
        }
    }
    (Value::Object(task), left_out)
}

/// The compared properties of an EWS task.
pub fn ews_task_properties(item: &Element) -> PropertyMap {
    let (task, _) = graph_task(item);
    let mut props = PropertyMap::new();
    put(&mut props, "subject", text(item, "Subject"));
    put(&mut props, "body", Some(ews_body_text(item)));
    for name in ["importance", "status"] {
        put(&mut props, name, task[name].as_str().map(str::to_owned));
    }
    task_times(&task, &mut props);
    put(
        &mut props,
        "categories",
        Some(sorted(strings(item, "Categories"))),
    );
    put(
        &mut props,
        "recurrence",
        Some(recurrence_text(&task["recurrence"])),
    );
    props
}

/// The compared properties of a To Do task.
pub fn graph_task_properties(task: &Value) -> PropertyMap {
    let mut props = PropertyMap::new();
    put(
        &mut props,
        "subject",
        task["title"].as_str().map(str::to_owned),
    );
    put(&mut props, "body", Some(graph_body_text(&task["body"])));
    for name in ["importance", "status"] {
        put(&mut props, name, task[name].as_str().map(str::to_owned));
    }
    task_times(task, &mut props);
    put(
        &mut props,
        "categories",
        Some(sorted(json_strings(&task["categories"]))),
    );
    put(
        &mut props,
        "recurrence",
        Some(recurrence_text(&task["recurrence"])),
    );
    props
}

/// A task's dates (days: To Do keeps no time of day for them) and its reminder
/// (to the minute).
fn task_times(task: &Value, props: &mut PropertyMap) {
    for (field, name) in [
        ("startDateTime", "start"),
        ("dueDateTime", "due"),
        ("completedDateTime", "completed"),
    ] {
        put(
            props,
            name,
            graph_utc(&task[field]).and_then(|t| t.get(..10).map(str::to_owned)),
        );
    }
    let reminder = if task["isReminderOn"].as_bool() == Some(true) {
        graph_utc(&task["reminderDateTime"]).and_then(|t| t.get(..16).map(str::to_owned))
    } else {
        Some("off".into())
    };
    put(props, "reminder", reminder);
}

// Inbox rules.

/// Rule predicates holding strings: EWS element, Graph property.
const STRING_PREDICATES: [(&str, &str); 7] = [
    ("Categories", "categories"),
    ("ContainsBodyStrings", "bodyContains"),
    ("ContainsHeaderStrings", "headerContains"),
    ("ContainsRecipientStrings", "recipientContains"),
    ("ContainsSenderStrings", "senderContains"),
    ("ContainsSubjectOrBodyStrings", "bodyOrSubjectContains"),
    ("ContainsSubjectStrings", "subjectContains"),
];

/// Rule predicates that are true or false: EWS element, Graph property.
const FLAG_PREDICATES: [(&str, &str); 17] = [
    ("HasAttachments", "hasAttachments"),
    ("IsApprovalRequest", "isApprovalRequest"),
    ("IsAutomaticForward", "isAutomaticForward"),
    ("IsAutomaticReply", "isAutomaticReply"),
    ("IsEncrypted", "isEncrypted"),
    ("IsMeetingRequest", "isMeetingRequest"),
    ("IsMeetingResponse", "isMeetingResponse"),
    ("IsNDR", "isNonDeliveryReport"),
    ("IsPermissionControlled", "isPermissionControlled"),
    ("IsReadReceipt", "isReadReceipt"),
    ("IsSigned", "isSigned"),
    ("IsVoicemail", "isVoicemail"),
    ("NotSentToMe", "notSentToMe"),
    ("SentCcMe", "sentCcMe"),
    ("SentOnlyToMe", "sentOnlyToMe"),
    ("SentToMe", "sentToMe"),
    ("SentToOrCcMe", "sentToOrCcMe"),
];

/// Rule elements holding addresses: EWS element, Graph property.
const ADDRESS_LISTS: [(&str, &str); 5] = [
    ("FromAddresses", "fromAddresses"),
    ("SentToAddresses", "sentToAddresses"),
    ("ForwardAsAttachmentToRecipients", "forwardAsAttachmentTo"),
    ("ForwardToRecipients", "forwardTo"),
    ("RedirectToRecipients", "redirectTo"),
];

/// Rule actions that are true or false: EWS element, Graph property.
const FLAG_ACTIONS: [(&str, &str); 4] = [
    ("Delete", "delete"),
    ("MarkAsRead", "markAsRead"),
    ("PermanentDelete", "permanentDelete"),
    ("StopProcessingRules", "stopProcessingRules"),
];

/// The `t:String` values of an element.
fn own_strings(element: &Element) -> Vec<String> {
    element
        .children_named("String")
        .map(|s| s.text.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// EWS `t:Address` elements as Graph recipients.
fn recipients(element: &Element) -> Value {
    Value::Array(
        element
            .children_named("Address")
            .filter_map(|address| {
                let mut email = json!({ "address": text(address, "EmailAddress")? });
                if let Some(name) = text(address, "Name") {
                    email["name"] = json!(name);
                }
                Some(json!({ "emailAddress": email }))
            })
            .collect(),
    )
}

fn is_true(element: &Element) -> bool {
    element.text.trim().eq_ignore_ascii_case("true")
}

/// The Graph name for an EWS one in a table.
fn graph_name(table: &[(&str, &'static str)], ews: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(name, _)| *name == ews)
        .map(|(_, graph)| *graph)
}

/// EWS rule predicates (conditions or exceptions) as Graph's; what Graph has no
/// predicate for goes into `left_out`.
fn graph_predicates(predicates: &Element, what: &str, left_out: &mut Vec<String>) -> Value {
    let mut out = Map::new();
    for part in &predicates.children {
        let name = part.name.as_str();
        if let Some(key) = graph_name(&STRING_PREDICATES, name) {
            out.insert(key.into(), json!(own_strings(part)));
        } else if let Some(key) = graph_name(&FLAG_PREDICATES, name) {
            out.insert(key.into(), json!(is_true(part)));
        } else if let Some(key) = graph_name(&ADDRESS_LISTS, name) {
            out.insert(key.into(), recipients(part));
        } else {
            match name {
                "Importance" | "Sensitivity" => {
                    out.insert(lower_first(name), json!(part.text.trim().to_lowercase()));
                }
                "FlaggedForAction" => {
                    let flag = match part.text.trim() {
                        "FYI" => "fyi".to_owned(),
                        other => lower_first(other),
                    };
                    out.insert("messageActionFlag".into(), json!(flag));
                }
                "WithinSizeRange" => {
                    let mut range = Map::new();
                    for (ews, graph) in [
                        ("MinimumSize", "minimumSize"),
                        ("MaximumSize", "maximumSize"),
                    ] {
                        if let Some(size) = text(part, ews).and_then(|s| s.parse::<i64>().ok()) {
                            range.insert(graph.into(), json!(size));
                        }
                    }
                    out.insert("withinSizeRange".into(), Value::Object(range));
                }
                _ => left_out.push(format!("its {what} {name} (no such predicate in Graph)")),
            }
        }
    }
    Value::Object(out)
}

/// The Graph message rule for an EWS inbox rule, and what of it Graph does not
/// get. `folder` maps an EWS folder id to the target's folder id.
pub fn graph_rule(
    rule: &Element,
    folder: &mut dyn FnMut(&str) -> Option<String>,
) -> (Value, Vec<String>) {
    let mut left_out = Vec::new();
    let mut out = Map::new();
    set(&mut out, "displayName", text(rule, "DisplayName"));
    if let Some(priority) = text(rule, "Priority").and_then(|p| p.parse::<i64>().ok()) {
        out.insert("sequence".into(), json!(priority));
    }
    out.insert(
        "isEnabled".into(),
        json!(rule.child("IsEnabled").is_some_and(is_true)),
    );
    for (ews, graph, what) in [
        ("Conditions", "conditions", "condition"),
        ("Exceptions", "exceptions", "exception"),
    ] {
        if let Some(predicates) = rule.child(ews) {
            let predicates = graph_predicates(predicates, what, &mut left_out);
            if predicates.as_object().is_some_and(|p| !p.is_empty()) {
                out.insert(graph.into(), predicates);
            }
        }
    }
    let mut actions = Map::new();
    for part in rule
        .child("Actions")
        .map(|a| a.children.as_slice())
        .unwrap_or_default()
    {
        let name = part.name.as_str();
        if let Some(key) = graph_name(&FLAG_ACTIONS, name) {
            actions.insert(key.into(), json!(is_true(part)));
        } else if let Some(key) = graph_name(&ADDRESS_LISTS, name) {
            actions.insert(key.into(), recipients(part));
        } else {
            match name {
                "AssignCategories" => {
                    actions.insert("assignCategories".into(), json!(own_strings(part)));
                }
                "MarkImportance" => {
                    actions.insert(
                        "markImportance".into(),
                        json!(part.text.trim().to_lowercase()),
                    );
                }
                "MoveToFolder" | "CopyToFolder" => {
                    let key = lower_first(name);
                    let id = part.child("FolderId").and_then(|f| f.attribute("Id"));
                    match id.and_then(&mut *folder) {
                        Some(target) => {
                            actions.insert(key, json!(target));
                        }
                        None => left_out.push(format!("its {key} action (folder not found)")),
                    }
                }
                _ => left_out.push(format!("its {name} action (no such action in Graph)")),
            }
        }
    }
    out.insert("actions".into(), Value::Object(actions));
    (Value::Object(out), left_out)
}

/// Rule predicates or actions in words, the same for the Graph JSON sent and
/// read back: what is set, sorted, with folders named by `folder`.
fn rule_part_text(part: &Value, folder: &dyn Fn(&str) -> String) -> String {
    let Some(part) = part.as_object() else {
        return String::new();
    };
    let mut words = Vec::new();
    for (key, value) in part {
        let value = match value {
            Value::Null | Value::Bool(false) => continue,
            Value::Bool(true) => "true".to_owned(),
            Value::String(id) if key == "moveToFolder" || key == "copyToFolder" => folder(id),
            Value::String(text) => text.clone(),
            Value::Array(items) if items.is_empty() => continue,
            Value::Array(items) if items.iter().all(Value::is_string) => {
                sorted(json_strings(value))
            }
            Value::Array(items) => address_list(
                items
                    .iter()
                    .filter_map(|r| r["emailAddress"]["address"].as_str()),
            ),
            other => other.to_string(),
        };
        words.push(format!("{key}={value}"));
    }
    words.sort();
    words.join(", ")
}

/// Rule properties compared. Test rules are created disabled: `enabled` is
/// shown, not compared.
pub const RULE_COMPARED: &[&str] = &["name", "conditions", "exceptions", "actions"];

/// The compared properties of a Graph message rule; `prefix` is taken off its
/// name (test rules have one).
pub fn rule_properties(rule: &Value, prefix: &str, folder: &dyn Fn(&str) -> String) -> PropertyMap {
    let mut props = PropertyMap::new();
    put(
        &mut props,
        "name",
        rule["displayName"]
            .as_str()
            .map(|name| name.strip_prefix(prefix).unwrap_or(name).to_owned()),
    );
    for part in ["conditions", "exceptions", "actions"] {
        put(&mut props, part, Some(rule_part_text(&rule[part], folder)));
    }
    put(&mut props, "enabled", Some(rule["isEnabled"].to_string()));
    props
}

// Categories.

/// Category properties compared.
pub const CATEGORY_COMPARED: &[&str] = &["name", "color"];

/// Graph's color for an Outlook color index: `preset0` to `preset24`, else
/// `none`.
pub fn graph_color(color: i32) -> String {
    if (0..=24).contains(&color) {
        format!("preset{color}")
    } else {
        "none".into()
    }
}

/// The compared properties of a category: its name, `prefix` taken off, and
/// its Graph color.
pub fn category_properties(name: &str, color: &str, prefix: &str) -> PropertyMap {
    let mut props = PropertyMap::new();
    put(
        &mut props,
        "name",
        Some(name.strip_prefix(prefix).unwrap_or(name).to_owned()),
    );
    put(&mut props, "color", Some(color.to_owned()));
    props
}

// The trial.

/// Whether an item would ring a reminder after `now` (Unix seconds): a trial
/// copy would then ring too, so the trial leaves it out.
pub(crate) fn rings_later(item: &Element, now: i64) -> bool {
    if text(item, "ReminderIsSet").as_deref() != Some("true") {
        return false;
    }
    let later = |name: &str| {
        text(item, name)
            .and_then(|t| parse_utc(&t))
            .is_some_and(|t| t > now)
    };
    if let Some(recurrence) = item.child("Recurrence") {
        let end = recurrence
            .child("EndDateRecurrence")
            .and_then(|r| text(r, "EndDate"))
            .and_then(|d| parse_utc(&format!("{}T23:59:59Z", d.get(..10)?)));
        return recurrence.child("EndDateRecurrence").is_none() || end.is_some_and(|e| e > now);
    }
    later("End") || later("ReminderDueBy")
}

/// What an item shows, to pick a few that together show the most.
fn features(props: &PropertyMap) -> BTreeSet<String> {
    let mut features: BTreeSet<String> = props.keys().cloned().collect();
    for name in ["time_zone", "all_day", "show_as", "sensitivity", "status"] {
        if let Some(value) = props.get(name) {
            features.insert(format!("{name}={value}"));
        }
    }
    if let Some(kind) = props.get("recurrence").and_then(|r| r.split(' ').next()) {
        features.insert(format!("recurrence={kind}"));
    }
    features
}

/// Items picked one by one, each adding the most features not shown yet, until
/// every feature is shown and at least `count` are picked.
fn pick(features: &[BTreeSet<String>], count: usize) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let mut picked = Vec::new();
    while let Some((new, best)) = (0..features.len())
        .filter(|i| !picked.contains(i))
        .map(|i| (features[i].difference(&seen).count(), i))
        .max_by_key(|&(new, i)| (new, std::cmp::Reverse(i)))
    {
        if new == 0 && picked.len() >= count {
            break;
        }
        seen.extend(features[best].iter().cloned());
        picked.push(best);
    }
    picked
}

/// Prints a source and its copy side by side, marking each compared property.
fn print_comparison(names: &[&str], source: &PropertyMap, copy: &PropertyMap) -> usize {
    let short = |value: Option<&String>| {
        let value = value.map(String::as_str).unwrap_or("-");
        let mut chars = value.chars();
        let head: String = chars.by_ref().take(44).collect();
        if chars.next().is_some() {
            format!("{head}…")
        } else {
            head
        }
    };
    let differences = compare_on(names, source, copy);
    println!("  {:<17} {:<46}   copy", "property", "source");
    for &name in names.iter().chain(["attendees"].iter()) {
        if !source.contains_key(name) && !copy.contains_key(name) {
            continue;
        }
        let mark = if !names.contains(&name) {
            ' '
        } else if differences.iter().any(|d| d.name == name) {
            '✗'
        } else {
            '✓'
        };
        println!(
            "  {name:<17} {:<46} {mark} {}",
            short(source.get(name)),
            short(copy.get(name))
        );
    }
    differences.len()
}

/// What test rules and categories are named with: the user finds and deletes
/// them by it.
pub const TEST_PREFIX: &str = "ecm-test ";

/// The categories the picked items use, then others, up to `count` when they
/// use fewer.
fn pick_categories(
    categories: &[ecm_ews::pim::Category],
    used: &BTreeSet<String>,
    count: usize,
) -> Vec<ecm_ews::pim::Category> {
    let (mut picked, others): (Vec<_>, Vec<_>) = categories
        .iter()
        .cloned()
        .partition(|category| used.contains(&category.name));
    let missing = count.saturating_sub(picked.len());
    picked.extend(others.into_iter().take(missing));
    picked
}

/// What a rule does and tests, to pick a few that together do the most.
fn rule_features(rule: &Value) -> BTreeSet<String> {
    let mut features = BTreeSet::new();
    for part in ["conditions", "exceptions", "actions"] {
        for key in rule[part].as_object().into_iter().flat_map(|p| p.keys()) {
            features.insert(format!("{part}.{key}"));
        }
    }
    features
}

#[cfg(windows)]
pub use trial::fidelity_graph_pim;

#[cfg(windows)]
mod trial {
    use std::collections::HashMap;

    use ecm_ews::pim::Category;
    use ecm_graph::encode;
    use serde_json::Value;

    use super::*;
    use crate::{
        EwsArgs, audit_cmd::GraphArgs, connect, fidelity_cmd::TEST_FOLDER,
        graph_plan_cmd::graph_folder_map,
    };

    /// One type of item and the items picked to copy.
    struct Kind {
        name: &'static str,
        items: Vec<Element>,
    }

    /// The trial's tally.
    #[derive(Default)]
    struct Tally {
        copied: usize,
        differing: usize,
        failed: usize,
    }

    impl Tally {
        fn record(&mut self, names: &[&str], result: Result<(PropertyMap, PropertyMap), String>) {
            match result {
                Ok((source, copy)) => {
                    self.copied += 1;
                    if print_comparison(names, &source, &copy) > 0 {
                        self.differing += 1;
                    }
                }
                Err(error) => {
                    self.failed += 1;
                    println!("  failed: {error}");
                }
            }
        }
    }

    /// Copies a few items of each type from the Exchange mailbox into the
    /// `ecm-test` containers of the Graph mailbox, and compares each copy with
    /// its source: at least `count`, more until every property the source has
    /// shows in one of them (categories: `count`). With `dry_run`, only prints
    /// what it would create.
    pub fn fidelity_graph_pim(
        ews: &EwsArgs,
        graph: &GraphArgs,
        count: usize,
        dry_run: bool,
    ) -> Result<(), String> {
        let client = connect(ews)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let mut kinds = Vec::new();
        for (name, distinguished) in [
            ("event", "calendar"),
            ("contact", "contacts"),
            ("task", "tasks"),
        ] {
            let folder = client
                .get_distinguished_folder(distinguished)
                .map_err(|e| e.to_string())?;
            let all = client.full_items(&folder.id).map_err(|e| e.to_string())?;
            let (ringing, items): (Vec<Element>, Vec<Element>) = all
                .into_iter()
                .partition(|item| name != "contact" && rings_later(item, now));
            let props: Vec<BTreeSet<String>> = items
                .iter()
                .map(|item| {
                    let mut props = match name {
                        "event" => ews_event_properties(item),
                        "contact" => ews_contact_properties(item, None),
                        _ => ews_task_properties(item),
                    };
                    if has_photo(item) {
                        props.insert("photo".into(), "yes".into());
                    }
                    features(&props)
                })
                .collect();
            let picked: Vec<Element> = pick(&props, count)
                .into_iter()
                .map(|i| items[i].clone())
                .collect();
            println!(
                "{distinguished}: {} items, {} picked{}",
                items.len() + ringing.len(),
                picked.len(),
                if ringing.is_empty() {
                    String::new()
                } else {
                    format!(
                        " ({} left out: their reminder would ring later)",
                        ringing.len()
                    )
                }
            );
            kinds.push(Kind {
                name,
                items: picked,
            });
        }
        let rules = client.inbox_rule_elements().map_err(|e| e.to_string())?;
        let features: Vec<BTreeSet<String>> = rules
            .iter()
            .map(|rule| rule_features(&graph_rule(rule, &mut |id| Some(id.to_owned())).0))
            .collect();
        let rules: Vec<Element> = pick(&features, count)
            .into_iter()
            .map(|i| rules[i].clone())
            .collect();
        println!("inbox rules: {} picked", rules.len());
        let used: BTreeSet<String> = kinds
            .iter()
            .flat_map(|kind| &kind.items)
            .flat_map(|item| strings(item, "Categories"))
            .collect();
        let all_categories = client.master_categories().map_err(|e| e.to_string())?;
        let categories = pick_categories(&all_categories, &used, count);
        println!(
            "categories: {}, {} picked",
            all_categories.len(),
            categories.len()
        );
        let source_paths: HashMap<String, String> = client
            .folder_tree("msgfolderroot")
            .map_err(|e| e.to_string())?
            .with_paths()
            .into_iter()
            .map(|(path, folder)| (folder.id.clone(), path))
            .collect();

        if dry_run {
            for kind in &kinds {
                for item in &kind.items {
                    let (json, left_out) = convert(kind.name, item);
                    print_planned(kind.name, &json, &left_out)?;
                }
            }
            for rule in &rules {
                let (json, left_out) = graph_rule(rule, &mut |id| source_paths.get(id).cloned());
                print_planned("disabled rule (folders by path)", &json, &left_out)?;
            }
            for category in &categories {
                println!();
                println!(
                    "== category {:?}: color {} ==",
                    category.name,
                    graph_color(category.color)
                );
            }
            println!();
            println!("Dry run: nothing was created.");
            return Ok(());
        }

        let transport = ecm_graph::WinHttpTransport::default();
        let tokens = ecm_graph::auth::sign_in_with_scopes(
            &transport,
            &graph.client_id,
            &graph.tenant,
            ecm_graph::auth::PIM_WRITE_SCOPES,
            &mut |prompt| {
                eprintln!();
                eprintln!(
                    "Sign in to Outlook.com (to write in the {TEST_FOLDER} calendar, contact folder and To Do list, and disabled rules and categories named {TEST_PREFIX}..., only): open {} and enter the code {}",
                    prompt.verification_uri, prompt.user_code
                );
                eprintln!("(Waiting for you to sign in...)");
            },
        )
        .map_err(|e| e.to_string())?;
        let target = ecm_graph::GraphClient::new(transport, tokens.access);

        let mut tally = Tally::default();
        for kind in &kinds {
            if kind.items.is_empty() {
                continue;
            }
            let container = match kind.name {
                "event" => target.ensure_calendar(TEST_FOLDER),
                "contact" => target.ensure_contact_folder(TEST_FOLDER),
                _ => target.ensure_todo_list(TEST_FOLDER),
            }
            .map_err(|e| e.to_string())?;
            for item in &kind.items {
                let (json, left_out) = convert(kind.name, item);
                println!();
                println!(
                    "== {} {:?} ==",
                    kind.name,
                    text(item, "Subject").unwrap_or_default()
                );
                let (names, result) = copy(&client, &target, kind.name, &container, item, &json);
                tally.record(names, result);
                for line in left_out {
                    println!("  not copied: {line}");
                }
            }
        }
        if !rules.is_empty() {
            copy_rules(&target, &rules, &source_paths, &mut tally)?;
        }
        if !categories.is_empty() {
            copy_categories(&target, &categories, &mut tally)?;
        }
        println!();
        println!(
            "{} items copied into {TEST_FOLDER} containers, {} with differences, {} failed.",
            tally.copied, tally.differing, tally.failed
        );
        Ok(())
    }

    fn print_planned(kind: &str, json: &Value, left_out: &[String]) -> Result<(), String> {
        println!();
        println!("== {kind} ==");
        println!(
            "{}",
            serde_json::to_string_pretty(json).map_err(|e| e.to_string())?
        );
        for line in left_out {
            println!("  not copied: {line}");
        }
        Ok(())
    }

    fn convert(kind: &str, item: &Element) -> (Value, Vec<String>) {
        match kind {
            "event" => graph_event(item),
            "contact" => graph_contact(item),
            _ => graph_task(item),
        }
    }

    type Copied = Result<(PropertyMap, PropertyMap), String>;

    /// Creates one item and reads it back: compared names, and source and copy.
    fn copy<E: ecm_ews::Transport, G: ecm_graph::Transport>(
        client: &ecm_ews::EwsClient<E>,
        target: &ecm_graph::GraphClient<G>,
        kind: &str,
        container: &str,
        item: &Element,
        json: &Value,
    ) -> (&'static [&'static str], Copied) {
        let error = |e: ecm_graph::Error| e.to_string();
        match kind {
            "event" => (
                EVENT_COMPARED,
                target
                    .create_event(Some(container), json)
                    .and_then(|id| target.get(&format!("/me/events/{}", encode(&id))))
                    .map_err(error)
                    .map(|copy| (ews_event_properties(item), graph_event_properties(&copy))),
            ),
            "contact" => (
                CONTACT_COMPARED,
                copy_contact(client, target, container, item, json),
            ),
            _ => (
                TASK_COMPARED,
                target
                    .create_task(container, json)
                    .and_then(|id| {
                        target.get(&format!(
                            "/me/todo/lists/{}/tasks/{}",
                            encode(container),
                            encode(&id)
                        ))
                    })
                    .map_err(error)
                    .map(|copy| (ews_task_properties(item), graph_task_properties(&copy))),
            ),
        }
    }

    /// Creates a contact and its photo, and reads them back.
    fn copy_contact<E: ecm_ews::Transport, G: ecm_graph::Transport>(
        client: &ecm_ews::EwsClient<E>,
        target: &ecm_graph::GraphClient<G>,
        container: &str,
        item: &Element,
        json: &Value,
    ) -> Copied {
        let error = |e: ecm_graph::Error| e.to_string();
        let photo = client.contact_photo(item).map_err(|e| e.to_string())?;
        let id = target
            .create_contact(Some(container), json)
            .map_err(error)?;
        if let Some(photo) = &photo {
            target
                .set_contact_photo(Some(container), &id, photo)
                .map_err(error)?;
        }
        let url = format!(
            "/me/contactFolders/{}/contacts/{}",
            encode(container),
            encode(&id)
        );
        let copy = target.get(&url).map_err(error)?;
        let copy_photo = match target.get_raw(&format!("{url}/photo/$value")) {
            Ok(bytes) if !bytes.is_empty() => Some(bytes),
            Ok(_) | Err(ecm_graph::Error::Graph { status: 404, .. }) => None,
            Err(e) => return Err(e.to_string()),
        };
        Ok((
            ews_contact_properties(item, photo.as_deref()),
            graph_contact_properties(&copy, copy_photo.as_deref()),
        ))
    }

    /// Creates each rule disabled, named with [`TEST_PREFIX`] and after the
    /// mailbox's own rules, its folders mapped by path (to `ecm-test` when the
    /// target lacks one), and compares it with its source.
    fn copy_rules<G: ecm_graph::Transport>(
        target: &ecm_graph::GraphClient<G>,
        rules: &[Element],
        source_paths: &HashMap<String, String>,
        tally: &mut Tally,
    ) -> Result<(), String> {
        let folders = graph_folder_map(target)?;
        let test_folder = target
            .ensure_folder(None, TEST_FOLDER)
            .map_err(|e| e.to_string())?;
        let mut paths: HashMap<String, String> = folders
            .ids
            .iter()
            .map(|(path, id)| (id.clone(), path.clone()))
            .collect();
        paths.insert(test_folder.clone(), TEST_FOLDER.to_owned());
        let path_of = |id: &str| paths.get(id).cloned().unwrap_or_else(|| id.to_owned());
        let last = target
            .inbox_rules()
            .map_err(|e| e.to_string())?
            .iter()
            .filter_map(|rule| rule["sequence"].as_i64())
            .max()
            .unwrap_or(0);
        for (n, rule) in rules.iter().enumerate() {
            let mut notes = Vec::new();
            let (json, left_out) = graph_rule(rule, &mut |id| {
                let path = folders.map.target(source_paths.get(id)?);
                Some(match folders.ids.get(&path) {
                    Some(id) => id.clone(),
                    None => {
                        notes.push(format!(
                            "{path} is not in the target: the test rule uses {TEST_FOLDER}"
                        ));
                        test_folder.clone()
                    }
                })
            });
            let name = json["displayName"].as_str().unwrap_or_default().to_owned();
            println!();
            println!("== inbox rule {name:?} ==");
            let mut test = json.clone();
            test["displayName"] = json!(format!("{TEST_PREFIX}{name}"));
            test["isEnabled"] = json!(false);
            test["sequence"] = json!(last + 1 + n as i64);
            let result = target
                .create_inbox_rule(&test)
                .and_then(|id| {
                    target.get(&format!(
                        "/me/mailFolders/inbox/messageRules/{}",
                        encode(&id)
                    ))
                })
                .map_err(|e| e.to_string())
                .map(|copy| {
                    (
                        rule_properties(&json, "", &path_of),
                        rule_properties(&copy, TEST_PREFIX, &path_of),
                    )
                });
            tally.record(RULE_COMPARED, result);
            for line in notes {
                println!("  note: {line}");
            }
            for line in left_out {
                println!("  not copied: {line}");
            }
        }
        Ok(())
    }

    /// Creates each category named with [`TEST_PREFIX`] (or reads it back when
    /// an earlier trial created it), and compares it with its source.
    fn copy_categories<G: ecm_graph::Transport>(
        target: &ecm_graph::GraphClient<G>,
        categories: &[Category],
        tally: &mut Tally,
    ) -> Result<(), String> {
        let existing = target.master_categories().map_err(|e| e.to_string())?;
        for category in categories {
            let name = format!("{TEST_PREFIX}{}", category.name);
            let color = graph_color(category.color);
            println!();
            println!("== category {:?} ==", category.name);
            let copy = match existing
                .iter()
                .find(|c| c["displayName"].as_str() == Some(name.as_str()))
            {
                Some(copy) => Ok(copy.clone()),
                None => target.create_category(&name, &color).and_then(|id| {
                    target.get(&format!("/me/outlook/masterCategories/{}", encode(&id)))
                }),
            };
            let result = copy.map_err(|e| e.to_string()).map(|copy| {
                (
                    category_properties(&category.name, &color, ""),
                    category_properties(
                        copy["displayName"].as_str().unwrap_or_default(),
                        copy["color"].as_str().unwrap_or_default(),
                        TEST_PREFIX,
                    ),
                )
            });
            tally.record(CATEGORY_COMPARED, result);
        }
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn fidelity_graph_pim(
    _: &crate::EwsArgs,
    _: &crate::audit_cmd::GraphArgs,
    _: usize,
    _: bool,
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

    const RECURRING: &str = concat!(
        "<t:CalendarItem><t:Subject>Yoga &amp; stretching</t:Subject><t:Sensitivity>Private</t:Sensitivity>",
        r#"<t:Body BodyType="HTML">&lt;html&gt;&lt;head&gt;&lt;style&gt;p{}&lt;/style&gt;&lt;/head&gt;&lt;body&gt;&lt;p&gt;Bring a&amp;nbsp;mat&lt;/p&gt;&lt;/body&gt;&lt;/html&gt;</t:Body>"#,
        "<t:Categories><t:String>Sport</t:String></t:Categories><t:Importance>Normal</t:Importance>",
        "<t:ReminderIsSet>true</t:ReminderIsSet><t:ReminderMinutesBeforeStart>15</t:ReminderMinutesBeforeStart>",
        "<t:Start>2024-05-06T16:30:00Z</t:Start><t:End>2024-05-06T17:30:00Z</t:End><t:IsAllDayEvent>false</t:IsAllDayEvent>",
        "<t:LegacyFreeBusyStatus>WorkingElsewhere</t:LegacyFreeBusyStatus><t:Location>Studio</t:Location>",
        "<t:Organizer><t:Mailbox><t:EmailAddress>me@example.test</t:EmailAddress></t:Mailbox></t:Organizer>",
        "<t:RequiredAttendees><t:Attendee><t:Mailbox><t:EmailAddress>Ann@Example.test</t:EmailAddress></t:Mailbox></t:Attendee><t:Attendee><t:Mailbox><t:EmailAddress>Me@Example.test</t:EmailAddress></t:Mailbox></t:Attendee></t:RequiredAttendees>",
        "<t:Recurrence><t:WeeklyRecurrence><t:Interval>1</t:Interval><t:DaysOfWeek>Monday Thursday</t:DaysOfWeek><t:FirstDayOfWeek>Monday</t:FirstDayOfWeek></t:WeeklyRecurrence>",
        "<t:EndDateRecurrence><t:StartDate>2024-05-06+02:00</t:StartDate><t:EndDate>2024-06-27+02:00</t:EndDate></t:EndDateRecurrence></t:Recurrence>",
        "<t:StartWallClock>2024-05-06T18:30:00+02:00</t:StartWallClock><t:EndWallClock>2024-05-06T19:30:00+02:00</t:EndWallClock>",
        "<t:StartTimeZoneId>Romance Standard Time</t:StartTimeZoneId><t:EndTimeZoneId>Romance Standard Time</t:EndTimeZoneId></t:CalendarItem>",
    );

    #[test]
    fn converts_a_recurring_event_in_its_own_time_zone_without_attendees() {
        let (event, left_out) = graph_event(&element(RECURRING));
        assert_eq!(
            event,
            json!({
                "subject": "Yoga & stretching",
                "body": {"contentType": "html", "content": "<html><head><style>p{}</style></head><body><p>Bring a&nbsp;mat</p></body></html>"},
                "start": {"dateTime": "2024-05-06T18:30:00", "timeZone": "Romance Standard Time"},
                "end": {"dateTime": "2024-05-06T19:30:00", "timeZone": "Romance Standard Time"},
                "isAllDay": false,
                "showAs": "workingElsewhere",
                "sensitivity": "private",
                "importance": "normal",
                "categories": ["Sport"],
                "isReminderOn": true,
                "reminderMinutesBeforeStart": 15,
                "location": {"displayName": "Studio"},
                "recurrence": {
                    "pattern": {"type": "weekly", "interval": 1, "daysOfWeek": ["monday", "thursday"], "firstDayOfWeek": "monday"},
                    "range": {"type": "endDate", "startDate": "2024-05-06", "endDate": "2024-06-27", "recurrenceTimeZone": "Romance Standard Time"}
                }
            })
        );
        assert_eq!(
            left_out,
            ["its attendees (1): Graph would send them invitations"]
        );
    }

    #[test]
    fn an_event_and_its_graph_copy_compare_equal() {
        let item = element(RECURRING);
        let source = ews_event_properties(&item);
        assert_eq!(source["body"], "Bring a mat");
        assert_eq!(source["start"], "2024-05-06T16:30:00Z");
        assert_eq!(
            source["recurrence"],
            "weekly every 1 on monday;thursday from 2024-05-06 to 2024-06-27"
        );
        assert_eq!(source["attendees"], "ann@example.test");
        // As Graph answers, in UTC, with its own defaults and HTML.
        let copy = json!({
            "subject": "Yoga & stretching",
            "body": {"contentType": "html", "content": "<html><head><meta charset=\"utf-8\"></head><body><div><p>Bring a mat</p></div></body></html>"},
            "start": {"dateTime": "2024-05-06T16:30:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2024-05-06T17:30:00.0000000", "timeZone": "UTC"},
            "originalStartTimeZone": "Romance Standard Time",
            "isAllDay": false,
            "showAs": "workingElsewhere",
            "sensitivity": "private",
            "importance": "normal",
            "categories": ["Sport"],
            "isReminderOn": true,
            "reminderMinutesBeforeStart": 15,
            "location": {"displayName": "Studio"},
            "attendees": [],
            "recurrence": {
                "pattern": {"type": "weekly", "interval": 1, "month": 0, "dayOfMonth": 0, "daysOfWeek": ["thursday", "monday"], "firstDayOfWeek": "monday", "index": "first"},
                "range": {"type": "endDate", "startDate": "2024-05-06", "endDate": "2024-06-27", "recurrenceTimeZone": "Romance Standard Time", "numberOfOccurrences": 0}
            }
        });
        let copy = graph_event_properties(&copy);
        assert!(compare_on(EVENT_COMPARED, &source, &copy).is_empty());
        assert!(!copy.contains_key("attendees"));
    }

    #[test]
    fn all_day_events_compare_by_their_days() {
        let item = element(concat!(
            "<t:CalendarItem><t:Subject>Ride</t:Subject><t:Start>2026-09-26T22:00:00Z</t:Start><t:End>2026-09-27T22:00:00Z</t:End>",
            "<t:IsAllDayEvent>true</t:IsAllDayEvent><t:StartWallClock>2026-09-27T00:00:00+02:00</t:StartWallClock>",
            "<t:EndWallClock>2026-09-28T00:00:00+02:00</t:EndWallClock><t:StartTimeZoneId>Romance Standard Time</t:StartTimeZoneId>",
            "<t:EndTimeZoneId>Romance Standard Time</t:EndTimeZoneId></t:CalendarItem>"
        ));
        let source = ews_event_properties(&item);
        assert_eq!(source["start"], "2026-09-27");
        // Graph gives an all-day event's days as they were set, even in UTC.
        let copy = graph_event_properties(&json!({
            "subject": "Ride",
            "isAllDay": true,
            "start": {"dateTime": "2026-09-27T00:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-09-28T00:00:00.0000000", "timeZone": "UTC"},
            "originalStartTimeZone": "Romance Standard Time",
            "isReminderOn": false
        }));
        assert_eq!(copy["start"], "2026-09-27");
        assert_eq!(copy["end"], "2026-09-28");
    }

    #[test]
    fn an_event_without_wall_clock_stays_in_utc() {
        let item = element(
            "<t:CalendarItem><t:Start>2013-08-05T06:30:00Z</t:Start><t:End>2013-08-05T07:00:00Z</t:End><t:StartTimeZoneId>tzone://Microsoft/Utc</t:StartTimeZoneId></t:CalendarItem>",
        );
        let (event, _) = graph_event(&item);
        assert_eq!(
            event["start"],
            json!({"dateTime": "2013-08-05T06:30:00", "timeZone": "UTC"})
        );
        assert_eq!(event["isReminderOn"], false);
    }

    #[test]
    fn converts_yearly_and_relative_recurrences() {
        let yearly = element(
            "<t:Recurrence><t:AbsoluteYearlyRecurrence><t:DayOfMonth>16</t:DayOfMonth><t:Month>June</t:Month></t:AbsoluteYearlyRecurrence><t:NoEndRecurrence><t:StartDate>2011-06-16Z</t:StartDate></t:NoEndRecurrence></t:Recurrence>",
        );
        let yearly = graph_recurrence(&yearly, "UTC").unwrap();
        assert_eq!(
            yearly["pattern"],
            json!({"type": "absoluteYearly", "interval": 1, "dayOfMonth": 16, "month": 6})
        );
        assert_eq!(
            recurrence_text(&yearly),
            "absoluteYearly every 1 on 6/16 from 2011-06-16"
        );
        let monthly = element(
            "<t:Recurrence><t:RelativeMonthlyRecurrence><t:Interval>2</t:Interval><t:DaysOfWeek>Weekday</t:DaysOfWeek><t:DayOfWeekIndex>Last</t:DayOfWeekIndex></t:RelativeMonthlyRecurrence><t:NumberedRecurrence><t:StartDate>2024-01-31Z</t:StartDate><t:NumberOfOccurrences>5</t:NumberOfOccurrences></t:NumberedRecurrence></t:Recurrence>",
        );
        let monthly = graph_recurrence(&monthly, "UTC").unwrap();
        assert_eq!(
            monthly["pattern"]["daysOfWeek"],
            json!(["monday", "tuesday", "wednesday", "thursday", "friday"])
        );
        assert_eq!(
            recurrence_text(&monthly),
            "relativeMonthly every 2 on the last friday;monday;thursday;tuesday;wednesday from 2024-01-31, 5 times"
        );
        let regenerating = element(
            "<t:Recurrence><t:DailyRegeneration><t:Interval>3</t:Interval></t:DailyRegeneration><t:NoEndRecurrence><t:StartDate>2024-01-31Z</t:StartDate></t:NoEndRecurrence></t:Recurrence>",
        );
        assert_eq!(graph_recurrence(&regenerating, "UTC"), None);
    }

    const CONTACT: &str = concat!(
        r#"<t:Contact><t:Body BodyType="Text">Met at the fair</t:Body><t:Categories><t:String>Famille</t:String></t:Categories>"#,
        "<t:FileAs>Example, Ann</t:FileAs><t:DisplayName>Ann Example</t:DisplayName><t:GivenName>Ann</t:GivenName><t:CompanyName>Acme</t:CompanyName>",
        r#"<t:EmailAddresses><t:Entry Key="EmailAddress2" Name="Ann (work)">ann@work.test</t:Entry><t:Entry Key="EmailAddress1" Name="Ann">Ann@Example.test</t:Entry></t:EmailAddresses>"#,
        r#"<t:PhysicalAddresses><t:Entry Key="Home"><t:Street>1 rue X</t:Street><t:City>Paris</t:City><t:PostalCode>75001</t:PostalCode></t:Entry></t:PhysicalAddresses>"#,
        r#"<t:PhoneNumbers><t:Entry Key="HomePhone">+33 1 00 00 00 00</t:Entry><t:Entry Key="MobilePhone">+33 6 00 00 00 00</t:Entry><t:Entry Key="BusinessFax">+33 1 99</t:Entry></t:PhoneNumbers>"#,
        "<t:Birthday>1980-05-12T11:59:00Z</t:Birthday><t:Children><t:String>Leo</t:String></t:Children><t:Surname>Example</t:Surname>",
        "<t:Attachments><t:FileAttachment><t:AttachmentId Id=\"A1\"/><t:IsContactPhoto>true</t:IsContactPhoto></t:FileAttachment></t:Attachments></t:Contact>",
    );

    #[test]
    fn converts_a_contact_and_names_what_graph_lacks() {
        let item = element(CONTACT);
        assert!(has_photo(&item));
        let (contact, left_out) = graph_contact(&item);
        assert_eq!(
            contact,
            json!({
                "displayName": "Ann Example",
                "fileAs": "Example, Ann",
                "givenName": "Ann",
                "surname": "Example",
                "companyName": "Acme",
                "birthday": "1980-05-12T11:59:00Z",
                "children": ["Leo"],
                "categories": ["Famille"],
                "emailAddresses": [
                    {"address": "Ann@Example.test", "name": "Ann"},
                    {"address": "ann@work.test", "name": "Ann (work)"}
                ],
                "homePhones": ["+33 1 00 00 00 00"],
                "mobilePhone": "+33 6 00 00 00 00",
                "homeAddress": {"street": "1 rue X", "city": "Paris", "postalCode": "75001"},
                "personalNotes": "Met at the fair"
            })
        );
        assert_eq!(
            left_out,
            ["its BusinessFax number (no such field in Graph)"]
        );
    }

    #[test]
    fn a_contact_and_its_graph_copy_compare_equal() {
        let item = element(CONTACT);
        let source = ews_contact_properties(&item, Some(b"JPEG"));
        assert_eq!(source["emails"], "ann@example.test;ann@work.test");
        assert_eq!(source["home_address"], "1 rue X | Paris |  | 75001");
        let (mut copy, _) = graph_contact(&item);
        copy["birthday"] = json!("1980-05-12T00:00:00Z");
        copy["businessPhones"] = json!([]);
        copy["businessAddress"] = json!({});
        let copy = graph_contact_properties(&copy, Some(b"JPEG"));
        assert!(compare_on(CONTACT_COMPARED, &source, &copy).is_empty());
        let other_photo = graph_contact_properties(&json!({}), Some(b"PNG"));
        assert!(
            compare_on(CONTACT_COMPARED, &source, &other_photo)
                .iter()
                .any(|d| d.name == "photo")
        );
    }

    #[test]
    fn converts_a_completed_task_and_compares_it() {
        let item = element(concat!(
            "<t:Task><t:Subject>Renew passport</t:Subject><t:Importance>High</t:Importance>",
            "<t:ReminderDueBy>2024-02-27T08:00:00Z</t:ReminderDueBy><t:ReminderIsSet>true</t:ReminderIsSet>",
            "<t:CompleteDate>2024-03-01T00:00:00Z</t:CompleteDate><t:DueDate>2024-02-28T00:00:00Z</t:DueDate>",
            "<t:Status>Completed</t:Status></t:Task>"
        ));
        let (task, left_out) = graph_task(&item);
        assert_eq!(
            task,
            json!({
                "title": "Renew passport",
                "importance": "high",
                "status": "completed",
                "isReminderOn": true,
                "dueDateTime": {"dateTime": "2024-02-28T00:00:00", "timeZone": "UTC"},
                "completedDateTime": {"dateTime": "2024-03-01T00:00:00", "timeZone": "UTC"},
                "reminderDateTime": {"dateTime": "2024-02-27T08:00:00", "timeZone": "UTC"}
            })
        );
        assert!(left_out.is_empty());
        let source = ews_task_properties(&item);
        assert_eq!(source["reminder"], "2024-02-27T08:00");
        let mut copy = task.clone();
        copy["body"] = json!({"contentType": "text", "content": ""});
        copy["dueDateTime"] = json!({"dateTime": "2024-02-28T00:00:00.0000000", "timeZone": "UTC"});
        assert!(compare_on(TASK_COMPARED, &source, &graph_task_properties(&copy)).is_empty());
    }

    #[test]
    fn leaves_out_items_whose_reminder_would_ring_later() {
        let now = parse_utc("2026-10-20T00:00:00Z").unwrap();
        let event = |end: &str, reminder: bool| {
            element(&format!(
                "<t:CalendarItem><t:ReminderIsSet>{reminder}</t:ReminderIsSet><t:End>{end}</t:End></t:CalendarItem>"
            ))
        };
        assert!(rings_later(&event("2026-11-02T23:30:00Z", true), now));
        assert!(!rings_later(&event("2026-11-02T23:30:00Z", false), now));
        assert!(!rings_later(&event("2020-11-02T23:30:00Z", true), now));
        let series = element(
            "<t:CalendarItem><t:ReminderIsSet>true</t:ReminderIsSet><t:End>2011-06-17T00:00:00Z</t:End><t:Recurrence><t:NoEndRecurrence><t:StartDate>2011-06-16Z</t:StartDate></t:NoEndRecurrence></t:Recurrence></t:CalendarItem>",
        );
        assert!(rings_later(&series, now));
    }

    #[test]
    fn picks_items_that_together_show_the_most() {
        let set = |names: &[&str]| {
            names
                .iter()
                .map(|n| (*n).to_owned())
                .collect::<BTreeSet<_>>()
        };
        let features = [
            set(&["subject"]),
            set(&["subject", "location", "body"]),
            set(&["subject", "location"]),
            set(&["subject", "recurrence"]),
        ];
        assert_eq!(pick(&features, 1), [1, 3]);
        assert_eq!(pick(&features, 3), [1, 3, 0]);
        assert_eq!(pick(&features, 9).len(), 4);
    }

    const RULE: &str = concat!(
        "<t:Rule><t:RuleId>AQ</t:RuleId><t:DisplayName>Bank &amp; co</t:DisplayName><t:Priority>3</t:Priority><t:IsEnabled>true</t:IsEnabled>",
        "<t:Conditions><t:FromAddresses><t:Address><t:Name>Bank</t:Name><t:EmailAddress>Alerts@Bank.test</t:EmailAddress><t:RoutingType>SMTP</t:RoutingType></t:Address></t:FromAddresses>",
        "<t:ContainsSubjectStrings><t:String>INVOICE</t:String></t:ContainsSubjectStrings><t:ItemClasses><t:String>IPM.Note</t:String></t:ItemClasses></t:Conditions>",
        "<t:Exceptions><t:FlaggedForAction>FYI</t:FlaggedForAction><t:Importance>High</t:Importance></t:Exceptions>",
        r#"<t:Actions><t:MoveToFolder><t:FolderId Id="E1" ChangeKey="AQ"/></t:MoveToFolder><t:MarkAsRead>true</t:MarkAsRead>"#,
        "<t:ServerReplyWithMessage><t:ItemId Id=\"T\"/></t:ServerReplyWithMessage><t:StopProcessingRules>true</t:StopProcessingRules></t:Actions></t:Rule>",
    );

    #[test]
    fn converts_a_rule_and_names_what_graph_lacks() {
        let (rule, left_out) = graph_rule(&element(RULE), &mut |id| {
            (id == "E1").then(|| "G1".to_owned())
        });
        assert_eq!(
            rule,
            json!({
                "displayName": "Bank & co",
                "sequence": 3,
                "isEnabled": true,
                "conditions": {
                    "fromAddresses": [{"emailAddress": {"address": "Alerts@Bank.test", "name": "Bank"}}],
                    "subjectContains": ["INVOICE"]
                },
                "exceptions": {"messageActionFlag": "fyi", "importance": "high"},
                "actions": {"moveToFolder": "G1", "markAsRead": true, "stopProcessingRules": true}
            })
        );
        assert_eq!(
            left_out,
            [
                "its condition ItemClasses (no such predicate in Graph)",
                "its ServerReplyWithMessage action (no such action in Graph)"
            ]
        );
        let (_, left_out) = graph_rule(&element(RULE), &mut |_| None);
        assert!(left_out.contains(&"its moveToFolder action (folder not found)".to_owned()));
    }

    #[test]
    fn a_rule_and_its_graph_copy_compare_equal() {
        let (rule, _) = graph_rule(&element(RULE), &mut |_| Some("G1".to_owned()));
        let path = |id: &str| {
            if id == "G1" {
                "Archive/Bank".to_owned()
            } else {
                id.to_owned()
            }
        };
        let source = rule_properties(&rule, "", &path);
        assert_eq!(
            source["actions"],
            "markAsRead=true, moveToFolder=Archive/Bank, stopProcessingRules=true"
        );
        assert_eq!(
            source["conditions"],
            "fromAddresses=alerts@bank.test, subjectContains=INVOICE"
        );
        // As Graph answers: the test name and state, its own empty values.
        let copy = json!({
            "displayName": "ecm-test Bank & co",
            "sequence": 15,
            "isEnabled": false,
            "conditions": {
                "fromAddresses": [{"emailAddress": {"address": "alerts@bank.test", "name": "Bank"}}],
                "subjectContains": ["INVOICE"],
                "sentToMe": false
            },
            "exceptions": {"messageActionFlag": "fyi", "importance": "high", "categories": []},
            "actions": {"moveToFolder": "G1", "markAsRead": true, "stopProcessingRules": true, "forwardTo": null}
        });
        let copy = rule_properties(&copy, TEST_PREFIX, &path);
        assert!(compare_on(RULE_COMPARED, &source, &copy).is_empty());
        assert_eq!(copy["enabled"], "false");
    }

    #[test]
    fn maps_category_colors_and_picks_the_used_ones() {
        use ecm_ews::pim::Category;
        assert_eq!(graph_color(9), "preset9");
        assert_eq!(graph_color(-1), "none");
        assert_eq!(graph_color(25), "none");
        assert_eq!(
            category_properties("ecm-test Famille", "preset9", TEST_PREFIX)["name"],
            "Famille"
        );
        let category = |name: &str| Category {
            name: name.into(),
            color: 0,
        };
        let all = [
            category("Red"),
            category("Amis"),
            category("Blue"),
            category("Famille"),
        ];
        let used: BTreeSet<String> = ["Famille".to_owned(), "Amis".to_owned()].into();
        let names = |picked: Vec<Category>| picked.into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(
            names(pick_categories(&all, &used, 3)),
            ["Amis", "Famille", "Red"]
        );
        assert_eq!(names(pick_categories(&all, &used, 1)), ["Amis", "Famille"]);
    }
}
