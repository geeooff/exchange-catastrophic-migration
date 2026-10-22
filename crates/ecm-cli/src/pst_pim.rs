//! A PST's calendar items, contacts and tasks as EWS gives them (milestone
//! M10): elements shaped like a `GetItem` answer with `AllProperties`, so that
//! the conversions to Graph, the comparisons, the plans and the checks written
//! for an Exchange source take them as they are. [`create_xml`] writes them
//! back as EWS `CreateItem` XML, to copy them into an Exchange mailbox.

use std::path::Path;

use base64::Engine as _;
use ecm_ews::xml::{Element, escape};
use ecm_model::item::format_iso;
use ecm_pst::{
    calendar::{
        End, Frequency, Pattern, Recurrence, Rule, TimeZone, Transition, filetime_to_seconds,
        format_seconds, minutes_to_seconds,
    },
    pim::{
        Item, PSETID_ADDRESS, PSETID_APPOINTMENT, PSETID_COMMON, PSETID_TASK, PimItems, Properties,
        Value,
    },
};

use crate::pim_cmd::windows_zone;

const PR_IMPORTANCE: u16 = 0x0017;
const PR_SENSITIVITY: u16 = 0x0036;
const PR_SUBJECT: u16 = 0x0037;
const PR_SENT_REPRESENTING_NAME: u16 = 0x0042;
const PR_SENT_REPRESENTING_EMAIL_ADDRESS: u16 = 0x0065;
const PR_RECIPIENT_TYPE: u16 = 0x0C15;
const PR_DISPLAY_NAME: u16 = 0x3001;
const PR_EMAIL_ADDRESS: u16 = 0x3003;
const PR_ATTACH_FILENAME: u16 = 0x3704;
const PR_ATTACH_LONG_FILENAME: u16 = 0x3707;
const PR_SMTP_ADDRESS: u16 = 0x39FE;
const PR_SENT_REPRESENTING_SMTP_ADDRESS: u16 = 0x5D02;
const PR_RECIPIENT_FLAGS: u16 = 0x5FFD;

/// `recipOrganizer`: the recipient is the meeting's organizer.
const RECIP_ORGANIZER: i64 = 0x0002;
/// `asfReceived` and `asfCanceled` of `PidLidAppointmentStateFlags`.
const ASF_RECEIVED: i64 = 0x0002;
const ASF_CANCELED: i64 = 0x0004;

/// Contact fields that are tagged properties, by EWS element.
const CONTACT_TAGS: [(&str, u16); 15] = [
    ("GivenName", 0x3A06),
    ("Initials", 0x3A0A),
    ("MiddleName", 0x3A44),
    ("Nickname", 0x3A4F),
    ("CompanyName", 0x3A16),
    ("AssistantName", 0x3A30),
    ("BusinessHomePage", 0x3A51),
    ("Department", 0x3A18),
    ("Generation", 0x3A05),
    ("JobTitle", 0x3A17),
    ("Manager", 0x3A4E),
    ("OfficeLocation", 0x3A19),
    ("Profession", 0x3A46),
    ("SpouseName", 0x3A48),
    ("Surname", 0x3A11),
];

/// Phone numbers, by EWS key, in EWS order.
const PHONES: [(&str, u16); 19] = [
    ("AssistantPhone", 0x3A2E),
    ("BusinessFax", 0x3A24),
    ("BusinessPhone", 0x3A08),
    ("BusinessPhone2", 0x3A1B),
    ("Callback", 0x3A02),
    ("CarPhone", 0x3A1E),
    ("CompanyMainPhone", 0x3A57),
    ("HomeFax", 0x3A25),
    ("HomePhone", 0x3A09),
    ("HomePhone2", 0x3A2F),
    ("Isdn", 0x3A2D),
    ("MobilePhone", 0x3A1C),
    ("OtherFax", 0x3A23),
    ("OtherTelephone", 0x3A1F),
    ("Pager", 0x3A21),
    ("PrimaryPhone", 0x3A1A),
    ("RadioPhone", 0x3A1D),
    ("Telex", 0x3A2C),
    ("TtyTddPhone", 0x3A4B),
];

/// Postal addresses: EWS key, then street, city, state, postal code and
/// country, tagged (`None` for the business address, which is named).
const HOME_ADDRESS: [u16; 5] = [0x3A5D, 0x3A59, 0x3A5C, 0x3A5B, 0x3A5A];
const OTHER_ADDRESS: [u16; 5] = [0x3A63, 0x3A5F, 0x3A62, 0x3A61, 0x3A60];
/// The business address: `PidLidWorkAddress*`, else `PR_BUSINESS_ADDRESS_*`.
const WORK_ADDRESS: [u32; 5] = [0x8045, 0x8046, 0x8047, 0x8048, 0x8049];
const BUSINESS_ADDRESS: [u16; 5] = [0x3A29, 0x3A27, 0x3A28, 0x3A2A, 0x3A26];
const ADDRESS_PARTS: [&str; 5] = ["Street", "City", "State", "PostalCode", "CountryOrRegion"];

const WEEK: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
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

fn leaf(name: &str, text: impl Into<String>) -> Element {
    Element {
        name: name.into(),
        text: text.into(),
        ..Element::default()
    }
}

fn node(name: &str, children: Vec<Element>) -> Element {
    Element {
        name: name.into(),
        children,
        ..Element::default()
    }
}

fn push(children: &mut Vec<Element>, name: &str, text: Option<&str>) {
    if let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) {
        children.push(leaf(name, text));
    }
}

fn strings(name: &str, values: &[String]) -> Option<Element> {
    (!values.is_empty()).then(|| {
        node(
            name,
            values.iter().map(|v| leaf("String", v.as_str())).collect(),
        )
    })
}

fn entry(key: &str, text: &str) -> Element {
    Element {
        name: "Entry".into(),
        attributes: vec![("Key".into(), key.into())],
        text: text.into(),
        ..Element::default()
    }
}

fn utc_text(filetime: i64) -> String {
    format_iso(filetime_to_seconds(filetime))
}

fn named_text(props: &Properties, set: u128, id: u32) -> Option<&str> {
    props.named(set, id).and_then(Value::as_text)
}

fn named_int(props: &Properties, set: u128, id: u32) -> Option<i64> {
    props.named(set, id).and_then(Value::as_integer)
}

fn named_bool(props: &Properties, set: u128, id: u32) -> bool {
    props.named(set, id).and_then(Value::as_bool) == Some(true)
}

fn body(item: &Item) -> Option<Element> {
    let body = item.body()?;
    Some(Element {
        name: "Body".into(),
        attributes: vec![(
            "BodyType".into(),
            if body.html { "HTML" } else { "Text" }.into(),
        )],
        text: body.content,
        ..Element::default()
    })
}

/// The children every item type shares: subject, sensitivity, body, categories,
/// importance.
fn common(item: &Item, children: &mut Vec<Element>) {
    let props = &item.properties;
    push(
        children,
        "Subject",
        props.text(PR_SUBJECT).map(ecm_pst::subject_text),
    );
    let sensitivity = match props.get(PR_SENSITIVITY).and_then(Value::as_integer) {
        Some(1) => "Personal",
        Some(2) => "Private",
        Some(3) => "Confidential",
        _ => "Normal",
    };
    children.push(leaf("Sensitivity", sensitivity));
    children.extend(body(item));
    children.extend(strings("Categories", &props.categories()));
    let importance = match props.get(PR_IMPORTANCE).and_then(Value::as_integer) {
        Some(0) => "Low",
        Some(2) => "High",
        _ => "Normal",
    };
    children.push(leaf("Importance", importance));
}

// Time zones.

/// A change of time on a Sunday: month, week of the month (5 for the last),
/// hour.
type SundayChange = (u16, u16, u16);

/// Well-known zones by their rules: Windows id, bias, then daylight start and
/// standard start.
const KNOWN_ZONES: [(&str, i32, SundayChange, SundayChange); 6] = [
    ("Romance Standard Time", -60, (3, 5, 2), (10, 5, 3)),
    ("GMT Standard Time", 0, (3, 5, 1), (10, 5, 2)),
    ("Eastern Standard Time", 300, (3, 2, 2), (11, 1, 2)),
    ("Central Standard Time", 360, (3, 2, 2), (11, 1, 2)),
    ("Mountain Standard Time", 420, (3, 2, 2), (11, 1, 2)),
    ("Pacific Standard Time", 480, (3, 2, 2), (11, 1, 2)),
];

/// Whether `t` changes the time on the same day and hour in `year` as the
/// well-known change: a custom zone may say "the fourth Sunday of October"
/// where Paris says "the last", the same day most years.
fn same_change(t: &Transition, (month, week, hour): SundayChange, year: i64) -> bool {
    let known = Transition {
        year: 0,
        month,
        weekday: 0,
        day: week,
        hour,
        minute: 0,
    };
    t.in_year(year).is_some() && t.in_year(year) == known.in_year(year)
}

/// The Windows id of a zone: its own name when it is one, the Windows zone of
/// an IANA name, else a well-known zone whose time changes on the same days
/// and hours in `year`.
pub fn windows_id(zone: &TimeZone, year: i64) -> Option<String> {
    let name = zone.id.trim();
    if name.ends_with("Standard Time") || name.eq_ignore_ascii_case("UTC") {
        return Some(name.to_owned());
    }
    if name.contains('/')
        && let Some(id) = windows_zone(name)
    {
        return Some(id);
    }
    let rule: &Rule = zone
        .rules
        .iter()
        .rev()
        .find(|r| i64::from(r.year) <= year)
        .or_else(|| zone.rules.first())?;
    KNOWN_ZONES
        .iter()
        .find(|(_, bias, daylight, standard)| {
            rule.bias == *bias
                && rule.daylight_bias - rule.standard_bias == -60
                && same_change(&rule.daylight, *daylight, year)
                && same_change(&rule.standard, *standard, year)
        })
        .map(|(id, ..)| (*id).to_owned())
}

/// The item's zone: its own (`TZDEFINITION`), with its Windows id in `year`.
fn zone_of(props: &Properties, recurring: bool, year: i64) -> Option<(TimeZone, String)> {
    let ids: &[u32] = if recurring {
        &[0x8260, 0x825E]
    } else {
        &[0x825E]
    };
    let zone = ids.iter().find_map(|id| {
        props
            .named(PSETID_APPOINTMENT, *id)
            .and_then(Value::as_binary)
            .and_then(TimeZone::parse)
    })?;
    let id = windows_id(&zone, year)?;
    Some((zone, id))
}

// Calendar items.

fn days_text(mask: u32, relative: bool) -> String {
    if relative {
        match mask {
            0x7F => return "Day".into(),
            0x3E => return "Weekday".into(),
            0x41 => return "WeekendDay".into(),
            _ => {}
        }
    }
    WEEK.iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, day)| *day)
        .collect::<Vec<_>>()
        .join(" ")
}

fn index_text(n: u32) -> &'static str {
    match n {
        1 => "First",
        2 => "Second",
        3 => "Third",
        4 => "Fourth",
        _ => "Last",
    }
}

/// A local day (minutes since 1601) as `YYYY-MM-DD`.
fn day_text(minutes: u32) -> String {
    format_seconds(minutes_to_seconds(minutes))[..10].to_owned()
}

/// The EWS `Recurrence` of a series or a task; `None` for a pattern EWS does
/// not have (a Hijri calendar).
pub fn recurrence_element(recurrence: &Recurrence, regenerating: bool) -> Option<Element> {
    let month = || {
        let date = day_text(recurrence.start_date);
        let month: usize = date[5..7].parse().unwrap_or(1);
        MONTHS[month.clamp(1, 12) - 1]
    };
    let interval = |n: u32| leaf("Interval", n.max(1).to_string());
    let pattern = match (recurrence.frequency, recurrence.pattern) {
        (_, Pattern::Other(_)) => return None,
        (Frequency::Daily, Pattern::Day) if regenerating => node(
            "DailyRegeneration",
            vec![interval(recurrence.period / 1440)],
        ),
        (Frequency::Daily, Pattern::Day) => {
            node("DailyRecurrence", vec![interval(recurrence.period / 1440)])
        }
        (_, Pattern::Week { days }) => node(
            if regenerating {
                "WeeklyRegeneration"
            } else {
                "WeeklyRecurrence"
            },
            vec![
                interval(if recurrence.frequency == Frequency::Daily {
                    1
                } else {
                    recurrence.period
                }),
                leaf("DaysOfWeek", days_text(days, false)),
                leaf(
                    "FirstDayOfWeek",
                    WEEK[(recurrence.first_day_of_week as usize).min(6)],
                ),
            ],
        ),
        (Frequency::Yearly, _) if regenerating => {
            node("YearlyRegeneration", vec![interval(recurrence.period / 12)])
        }
        (_, _) if regenerating => node("MonthlyRegeneration", vec![interval(recurrence.period)]),
        (Frequency::Yearly, Pattern::Month { day }) => node(
            "AbsoluteYearlyRecurrence",
            vec![leaf("DayOfMonth", day.to_string()), leaf("Month", month())],
        ),
        (Frequency::Yearly, Pattern::MonthNth { days, n }) => node(
            "RelativeYearlyRecurrence",
            vec![
                leaf("DaysOfWeek", days_text(days, true)),
                leaf("DayOfWeekIndex", index_text(n)),
                leaf("Month", month()),
            ],
        ),
        (Frequency::Yearly, Pattern::MonthEnd) => node(
            "RelativeYearlyRecurrence",
            vec![
                leaf("DaysOfWeek", "Day"),
                leaf("DayOfWeekIndex", "Last"),
                leaf("Month", month()),
            ],
        ),
        (_, Pattern::Month { day }) => node(
            "AbsoluteMonthlyRecurrence",
            vec![
                interval(recurrence.period),
                leaf("DayOfMonth", day.to_string()),
            ],
        ),
        (_, Pattern::MonthNth { days, n }) => node(
            "RelativeMonthlyRecurrence",
            vec![
                interval(recurrence.period),
                leaf("DaysOfWeek", days_text(days, true)),
                leaf("DayOfWeekIndex", index_text(n)),
            ],
        ),
        (_, Pattern::MonthEnd) => node(
            "RelativeMonthlyRecurrence",
            vec![
                interval(recurrence.period),
                leaf("DaysOfWeek", "Day"),
                leaf("DayOfWeekIndex", "Last"),
            ],
        ),
        (_, Pattern::Day) => node("DailyRecurrence", vec![interval(recurrence.period / 1440)]),
    };
    let start = leaf("StartDate", day_text(recurrence.start_date));
    let range = match recurrence.end {
        End::Date => node(
            "EndDateRecurrence",
            vec![start, leaf("EndDate", day_text(recurrence.end_date))],
        ),
        End::Count => node(
            "NumberedRecurrence",
            vec![
                start,
                leaf("NumberOfOccurrences", recurrence.occurrences.to_string()),
            ],
        ),
        End::Never => node("NoEndRecurrence", vec![start]),
    };
    Some(node("Recurrence", vec![pattern, range]))
}

fn mailbox(name: Option<&str>, address: &str) -> Element {
    let mut children = Vec::new();
    push(&mut children, "Name", name);
    children.push(leaf("EmailAddress", address));
    node("Mailbox", children)
}

/// A recipient's SMTP address: `PR_SMTP_ADDRESS`, else `PR_EMAIL_ADDRESS` when
/// it is one.
fn smtp(props: &Properties, smtp: u16, email: u16) -> Option<String> {
    props
        .text(smtp)
        .or_else(|| props.text(email).filter(|a| a.contains('@')))
        .map(str::to_owned)
}

/// The EWS calendar item for a PST appointment, and what of it is not kept.
pub fn calendar_item(item: &Item) -> (Element, Vec<String>) {
    let props = &item.properties;
    let mut notes = Vec::new();
    let mut children = Vec::new();
    common(item, &mut children);
    let reminder = named_bool(props, PSETID_COMMON, 0x8503);
    children.push(leaf("ReminderIsSet", reminder.to_string()));
    if reminder && let Some(minutes) = named_int(props, PSETID_COMMON, 0x8501) {
        children.push(leaf("ReminderMinutesBeforeStart", minutes.to_string()));
    }
    let start = props
        .named(PSETID_APPOINTMENT, 0x820D)
        .and_then(Value::as_time);
    let end = props
        .named(PSETID_APPOINTMENT, 0x820E)
        .and_then(Value::as_time);
    for (name, time) in [("Start", start), ("End", end)] {
        if let Some(time) = time {
            children.push(leaf(name, utc_text(time)));
        }
    }
    let state = named_int(props, PSETID_APPOINTMENT, 0x8217).unwrap_or(0);
    if state & ASF_CANCELED != 0 {
        children.push(leaf("IsCancelled", "true"));
    }
    let all_day = named_bool(props, PSETID_APPOINTMENT, 0x8215);
    children.push(leaf("IsAllDayEvent", all_day.to_string()));
    let free_busy = match named_int(props, PSETID_APPOINTMENT, 0x8205) {
        Some(0) => "Free",
        Some(1) => "Tentative",
        Some(3) => "OOF",
        Some(4) => "WorkingElsewhere",
        _ => "Busy",
    };
    children.push(leaf("LegacyFreeBusyStatus", free_busy));
    push(
        &mut children,
        "Location",
        named_text(props, PSETID_APPOINTMENT, 0x8208),
    );

    // Organizer and attendees.
    let organizer = smtp(
        props,
        PR_SENT_REPRESENTING_SMTP_ADDRESS,
        PR_SENT_REPRESENTING_EMAIL_ADDRESS,
    );
    if let Some(address) = &organizer {
        children.push(node(
            "Organizer",
            vec![mailbox(props.text(PR_SENT_REPRESENTING_NAME), address)],
        ));
    }
    let mut lists: [(&str, Vec<Element>); 3] = [
        ("RequiredAttendees", Vec::new()),
        ("OptionalAttendees", Vec::new()),
        ("Resources", Vec::new()),
    ];
    for recipient in &item.recipients {
        let flags = recipient
            .get(PR_RECIPIENT_FLAGS)
            .and_then(Value::as_integer)
            .unwrap_or(0);
        let Some(address) = smtp(recipient, PR_SMTP_ADDRESS, PR_EMAIL_ADDRESS) else {
            continue;
        };
        if flags & RECIP_ORGANIZER != 0 {
            continue;
        }
        let list = match recipient.get(PR_RECIPIENT_TYPE).and_then(Value::as_integer) {
            Some(2) => 1,
            Some(3) => 2,
            _ => 0,
        };
        lists[list].1.push(node(
            "Attendee",
            vec![mailbox(recipient.text(PR_DISPLAY_NAME), &address)],
        ));
    }
    for (name, attendees) in lists {
        if !attendees.is_empty() {
            children.push(node(name, attendees));
        }
    }
    children.push(leaf("IsOrganizer", (state & ASF_RECEIVED == 0).to_string()));

    // The series, in its own zone.
    let recurrence = props
        .named(PSETID_APPOINTMENT, 0x8216)
        .and_then(Value::as_binary)
        .and_then(Recurrence::parse);
    let year = start.map_or(1970, |s| {
        ecm_model::item::civil_from_days(filetime_to_seconds(s).div_euclid(86_400)).0
    });
    let zone = zone_of(props, recurrence.is_some(), year);
    // An all-day event without a zone: its days start at local midnight, kept in
    // UTC; the nearest midnight is its day in any zone within 12 hours of UTC.
    if zone.is_none() && all_day {
        for (name, time) in [("Start", start), ("End", end)] {
            if let (Some(time), Some(child)) = (time, children.iter_mut().find(|c| c.name == name))
            {
                let day = (filetime_to_seconds(time) + 43_200).div_euclid(86_400);
                child.text = format_iso(day * 86_400);
            }
        }
    }
    if zone.is_none()
        && let Some(raw) = props
            .named(PSETID_APPOINTMENT, 0x825E)
            .and_then(Value::as_binary)
            .and_then(TimeZone::parse)
    {
        notes.push(format!(
            "its time zone {:?} is not a known one: its times stay in UTC",
            raw.id
        ));
    }
    if let Some(recurrence) = &recurrence {
        match recurrence_element(recurrence, false) {
            Some(element) => children.push(element),
            None => notes.push("its recurrence (a Hijri calendar pattern)".into()),
        }
        let to_utc = |local_minutes: u32| -> String {
            let local = minutes_to_seconds(local_minutes);
            let utc = zone.as_ref().map_or(local, |(z, _)| z.to_utc(local));
            format_iso(utc)
        };
        let occurrences: Vec<Element> = recurrence
            .exceptions
            .iter()
            .map(|e| {
                node(
                    "Occurrence",
                    vec![
                        leaf("Start", to_utc(e.start)),
                        leaf("End", to_utc(e.end)),
                        leaf("OriginalStart", to_utc(e.original_start)),
                    ],
                )
            })
            .collect();
        if !occurrences.is_empty() {
            children.push(node("ModifiedOccurrences", occurrences));
        }
        let deleted: Vec<Element> = recurrence
            .removed()
            .iter()
            .map(|day| {
                node(
                    "DeletedOccurrence",
                    vec![leaf("Start", to_utc(day + recurrence.start_offset))],
                )
            })
            .collect();
        if !deleted.is_empty() {
            children.push(node("DeletedOccurrences", deleted));
        }
    }
    if let Some((zone, id)) = &zone {
        for (name, time) in [("StartWallClock", start), ("EndWallClock", end)] {
            if let Some(time) = time {
                children.push(leaf(
                    name,
                    format_seconds(zone.to_local(filetime_to_seconds(time))),
                ));
            }
        }
        children.push(leaf("StartTimeZoneId", id.as_str()));
        children.push(leaf("EndTimeZoneId", id.as_str()));
    }
    (node("CalendarItem", children), notes)
}

// Contacts.

/// An email address of a contact (`PidLidEmail1EmailAddress`…): the SMTP
/// address, or its original display name when that is one (an Exchange
/// address).
fn email(props: &Properties, base: u32) -> Option<String> {
    let address = named_text(props, PSETID_ADDRESS, base + 3);
    let kind = named_text(props, PSETID_ADDRESS, base + 2).unwrap_or("SMTP");
    match address {
        Some(address) if !kind.eq_ignore_ascii_case("EX") && address.contains('@') => {
            Some(address.to_owned())
        }
        _ => named_text(props, PSETID_ADDRESS, base + 4)
            .filter(|a| a.contains('@') && !a.contains(' '))
            .map(str::to_owned),
    }
}

/// A contact's birthday, at 11:59 UTC as Outlook.com keeps it (the same day in
/// every zone): the day of `PidLidBirthdayLocal` (midnight UTC), else the
/// nearest day of `PidTagBirthday`, which is midnight local time in UTC (the
/// day before at 23:00 for Paris).
fn birthday(props: &Properties) -> Option<String> {
    const DAY: i64 = 86_400;
    let day = match props.named(PSETID_ADDRESS, 0x80DE).and_then(Value::as_time) {
        Some(local) => filetime_to_seconds(local).div_euclid(DAY),
        None => {
            let utc = props.get(0x3A42).and_then(Value::as_time)?;
            (filetime_to_seconds(utc) + DAY / 2).div_euclid(DAY)
        }
    };
    Some(format_iso(day * DAY + 11 * 3600 + 59 * 60))
}

/// The EWS contact for a PST contact, with its photo as the attachment
/// `IsContactPhoto`, its content inline.
pub fn contact(item: &Item) -> Element {
    let props = &item.properties;
    let mut children = Vec::new();
    common(item, &mut children);
    if let Some(photo) = item.contact_photo() {
        let name = item
            .attachments
            .iter()
            .find(|a| a.is_contact_photo())
            .and_then(|a| {
                a.properties
                    .text(PR_ATTACH_LONG_FILENAME)
                    .or_else(|| a.properties.text(PR_ATTACH_FILENAME))
            })
            .unwrap_or("ContactPicture.jpg");
        children.push(node(
            "Attachments",
            vec![node(
                "FileAttachment",
                vec![
                    leaf("Name", name),
                    leaf("IsContactPhoto", "true"),
                    leaf(
                        "Content",
                        base64::engine::general_purpose::STANDARD.encode(photo),
                    ),
                ],
            )],
        ));
    }
    push(
        &mut children,
        "FileAs",
        named_text(props, PSETID_ADDRESS, 0x8005),
    );
    push(&mut children, "DisplayName", props.text(PR_DISPLAY_NAME));
    for (name, id) in &CONTACT_TAGS[..5] {
        push(&mut children, name, props.text(*id));
    }
    let emails: Vec<Element> = [0x8080, 0x8090, 0x80A0]
        .iter()
        .enumerate()
        .filter_map(|(n, base)| {
            email(props, *base).map(|a| entry(&format!("EmailAddress{}", n + 1), &a))
        })
        .collect();
    if !emails.is_empty() {
        children.push(node("EmailAddresses", emails));
    }
    let mut addresses = Vec::new();
    for (key, parts) in [
        ("Home", HOME_ADDRESS.map(|id| props.text(id))),
        (
            "Business",
            std::array::from_fn(|i| {
                named_text(props, PSETID_ADDRESS, WORK_ADDRESS[i])
                    .or_else(|| props.text(BUSINESS_ADDRESS[i]))
            }),
        ),
        ("Other", OTHER_ADDRESS.map(|id| props.text(id))),
    ] {
        let mut inner = Vec::new();
        for (name, part) in ADDRESS_PARTS.iter().zip(parts) {
            push(&mut inner, name, part);
        }
        if !inner.is_empty() {
            addresses.push(Element {
                name: "Entry".into(),
                attributes: vec![("Key".into(), key.into())],
                children: inner,
                ..Element::default()
            });
        }
    }
    if !addresses.is_empty() {
        children.push(node("PhysicalAddresses", addresses));
    }
    let phones: Vec<Element> = PHONES
        .iter()
        .filter_map(|(key, id)| props.text(*id).map(|p| entry(key, p)))
        .collect();
    if !phones.is_empty() {
        children.push(node("PhoneNumbers", phones));
    }
    if let Some(birthday) = birthday(props) {
        children.push(leaf("Birthday", birthday));
    }
    let im = named_text(props, PSETID_ADDRESS, 0x8062);
    if let Some(im) = im {
        children.push(node("ImAddresses", vec![entry("ImAddress1", im)]));
    }
    children.extend(strings(
        "Children",
        &props.get(0x3A58).map(Value::as_texts).unwrap_or_default(),
    ));
    for (name, id) in &CONTACT_TAGS[5..] {
        push(&mut children, name, props.text(*id));
    }
    if item.contact_photo().is_some() {
        children.push(leaf("HasPicture", "true"));
    }
    node("Contact", children)
}

/// The photo of a contact made by [`contact`].
pub fn photo(contact: &Element) -> Option<Vec<u8>> {
    let content = contact
        .child("Attachments")?
        .children_named("FileAttachment")
        .find(|a| a.child_text("IsContactPhoto") == Some("true"))?
        .child_text("Content")?;
    base64::engine::general_purpose::STANDARD
        .decode(content.trim())
        .ok()
}

// Tasks.

/// The EWS task for a PST task, and what of it is not kept.
pub fn task(item: &Item) -> (Element, Vec<String>) {
    let props = &item.properties;
    let mut notes = Vec::new();
    let mut children = Vec::new();
    common(item, &mut children);
    let reminder = named_bool(props, PSETID_COMMON, 0x8503);
    if reminder && let Some(time) = props.named(PSETID_COMMON, 0x8502).and_then(Value::as_time) {
        children.push(leaf("ReminderDueBy", utc_text(time)));
    }
    children.push(leaf("ReminderIsSet", reminder.to_string()));
    let status = match named_int(props, PSETID_TASK, 0x8101) {
        Some(1) => "InProgress",
        Some(2) => "Completed",
        Some(3) => "WaitingOnOthers",
        Some(4) => "Deferred",
        _ => "NotStarted",
    };
    let time = |id: u32| props.named(PSETID_TASK, id).and_then(Value::as_time);
    if status == "Completed"
        && let Some(done) = time(0x810F)
    {
        children.push(leaf("CompleteDate", utc_text(done)));
    }
    if let Some(due) = time(0x8105) {
        children.push(leaf("DueDate", utc_text(due)));
    }
    if named_bool(props, PSETID_TASK, 0x8126)
        && let Some(recurrence) = props
            .named(PSETID_TASK, 0x8116)
            .and_then(Value::as_binary)
            .and_then(Recurrence::parse)
    {
        match recurrence_element(&recurrence, recurrence.regenerating) {
            Some(element) => children.push(element),
            None => notes.push("its recurrence (a Hijri calendar pattern)".into()),
        }
    }
    if let Some(start) = time(0x8104) {
        children.push(leaf("StartDate", utc_text(start)));
    }
    children.push(leaf("Status", status));
    (node("Task", children), notes)
}

// Creating them over EWS.

/// What `CreateItem` takes of a calendar item, in schema order. `Start` and
/// `End` are written in the item's zone, followed by `StartTimeZone` and
/// `EndTimeZone`.
const EVENT_ORDER: [&str; 16] = [
    "Subject",
    "Sensitivity",
    "Body",
    "Categories",
    "Importance",
    "ReminderIsSet",
    "ReminderMinutesBeforeStart",
    "Start",
    "End",
    "IsAllDayEvent",
    "LegacyFreeBusyStatus",
    "Location",
    "RequiredAttendees",
    "OptionalAttendees",
    "Resources",
    "Recurrence",
];

/// What `CreateItem` takes of a contact, in schema order; the photo is
/// attached afterwards.
const CONTACT_ORDER: [&str; 27] = [
    "Sensitivity",
    "Body",
    "Categories",
    "Importance",
    "FileAs",
    "DisplayName",
    "GivenName",
    "Initials",
    "MiddleName",
    "Nickname",
    "CompanyName",
    "EmailAddresses",
    "PhysicalAddresses",
    "PhoneNumbers",
    "AssistantName",
    "Birthday",
    "BusinessHomePage",
    "Children",
    "Department",
    "Generation",
    "ImAddresses",
    "JobTitle",
    "Manager",
    "OfficeLocation",
    "Profession",
    "SpouseName",
    "Surname",
];

/// What `CreateItem` takes of a task, in schema order.
const TASK_ORDER: [&str; 12] = [
    "Subject",
    "Sensitivity",
    "Body",
    "Categories",
    "Importance",
    "ReminderDueBy",
    "ReminderIsSet",
    "CompleteDate",
    "DueDate",
    "Recurrence",
    "StartDate",
    "Status",
];

/// The parts of a postal address, in schema order.
const ADDRESS_ORDER: [&str; 5] = ["Street", "City", "State", "CountryOrRegion", "PostalCode"];

/// An element as EWS XML, in the `t:` namespace; a postal address's parts in
/// schema order.
fn write_element(element: &Element, out: &mut String) {
    out.push_str("<t:");
    out.push_str(&element.name);
    for (name, value) in &element.attributes {
        out.push_str(&format!(r#" {name}="{}""#, escape(value)));
    }
    out.push('>');
    if element.children.is_empty() {
        out.push_str(&escape(&element.text));
    } else {
        let mut children: Vec<&Element> = element.children.iter().collect();
        if element.name == "Entry" {
            children.sort_by_key(|c| ADDRESS_ORDER.iter().position(|n| *n == c.name));
        }
        for child in children {
            write_element(child, out);
        }
    }
    out.push_str("</t:");
    out.push_str(&element.name);
    out.push('>');
}

/// The children of `item` named in `order`, as EWS XML, in that order.
fn write_ordered(item: &Element, order: &[&str], out: &mut String) {
    for name in order {
        for child in item.children_named(name) {
            write_element(child, out);
        }
    }
}

/// The EWS XML that creates a calendar item, contact or task made by
/// [`calendar_item`], [`contact`] or [`task`], and what of it the copy does not
/// get. A meeting keeps its attendees only when the user organized it (on an
/// invitation received, they would make the user its organizer); `CreateItem`
/// sends no invitation.
pub fn create_xml(item: &Element) -> (String, Vec<String>) {
    let mut notes = Vec::new();
    let mut xml = format!("<t:{}>", item.name);
    match item.name.as_str() {
        "CalendarItem" => {
            let organizer = item.child_text("IsOrganizer") != Some("false");
            let mut item = item.clone();
            let zone = item.child_text("StartTimeZoneId").map(str::to_owned);
            for (time, wall) in [("Start", "StartWallClock"), ("End", "EndWallClock")] {
                let local = item
                    .child_text(wall)
                    .and_then(|w| w.get(..19))
                    .map(str::to_owned);
                if let (Some(_), Some(local)) = (&zone, local)
                    && let Some(child) = item.children.iter_mut().find(|c| c.name == time)
                {
                    child.text = local;
                }
            }
            if !organizer {
                let attendees = ["RequiredAttendees", "OptionalAttendees", "Resources"]
                    .iter()
                    .filter_map(|name| item.child(name))
                    .map(|list| list.children.len())
                    .sum::<usize>();
                item.children.retain(|c| {
                    !matches!(
                        c.name.as_str(),
                        "RequiredAttendees" | "OptionalAttendees" | "Resources"
                    )
                });
                notes.push(if attendees > 0 {
                    format!(
                        "its organizer and attendees ({attendees}): the copy is the user's own event"
                    )
                } else {
                    "its organizer: the copy is the user's own event".into()
                });
            }
            for (name, what) in [
                ("ModifiedOccurrences", "modified occurrences"),
                ("DeletedOccurrences", "deleted occurrences"),
            ] {
                let count = item.child(name).map_or(0, |l| l.children.len());
                if count > 0 {
                    notes.push(format!("{count} {what} of the series"));
                }
            }
            write_ordered(&item, &EVENT_ORDER, &mut xml);
            let zone = zone.unwrap_or_else(|| "UTC".into());
            xml += &format!(
                r#"<t:StartTimeZone Id="{0}"/><t:EndTimeZone Id="{0}"/>"#,
                escape(&zone)
            );
        }
        "Contact" => write_ordered(item, &CONTACT_ORDER, &mut xml),
        _ => write_ordered(item, &TASK_ORDER, &mut xml),
    }
    xml += &format!("</t:{}>", item.name);
    (xml, notes)
}

/// The PST's calendar items, contacts and tasks as EWS elements, and what is
/// left out: items `outlook-pst` cannot read, contact groups, and what each
/// item loses.
pub struct PstPim {
    /// Calendar items.
    pub events: Vec<Element>,
    /// Contacts.
    pub contacts: Vec<Element>,
    /// Tasks.
    pub tasks: Vec<Element>,
    /// What is not copied, and why.
    pub left_out: Vec<String>,
}

/// Converts what [`ecm_pst::PstSource::pim_items`] read.
pub fn convert(read: &PimItems) -> PstPim {
    let mut pim = PstPim {
        events: Vec::new(),
        contacts: Vec::new(),
        tasks: Vec::new(),
        left_out: read
            .set_aside
            .iter()
            .map(|(folder, count)| {
                format!(
                    "{folder}: {count} items not read (name the folder with --folder to read it)"
                )
            })
            .chain(read.unreadable.iter().map(|u| format!("unreadable: {u}")))
            .collect(),
    };
    let mut groups = 0;
    for item in &read.items {
        let subject = item
            .properties
            .text(PR_SUBJECT)
            .or_else(|| item.properties.text(PR_DISPLAY_NAME))
            .unwrap_or_default()
            .to_owned();
        let class = item.class();
        if class.starts_with("IPM.Appointment") {
            let (element, notes) = calendar_item(item);
            pim.left_out
                .extend(notes.iter().map(|n| format!("event {subject:?}: {n}")));
            pim.events.push(element);
        } else if class.starts_with("IPM.Contact") {
            pim.contacts.push(contact(item));
        } else if class.starts_with("IPM.Task") {
            let (element, notes) = task(item);
            pim.left_out
                .extend(notes.iter().map(|n| format!("task {subject:?}: {n}")));
            pim.tasks.push(element);
        } else {
            groups += 1;
        }
    }
    if groups > 0 {
        pim.left_out.push(format!(
            "{groups} contact groups (distribution lists): not copied"
        ));
    }
    pim
}

/// Reads a PST's calendar items, contacts and tasks, of the folders named in
/// `folders` or else of its calendar, contact and task folders, checking the
/// file did not change while it was read.
pub fn read_pst(pst: &Path, folders: &[String]) -> Result<PstPim, String> {
    let before = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
    let read = {
        let reader = ecm_pst::PstSource::open(pst).map_err(|e| e.to_string())?;
        reader.pim_items(folders).map_err(|e| e.to_string())?
    };
    if !folders.is_empty() && read.items.is_empty() && read.unreadable.is_empty() {
        return Err(format!(
            "no calendar item, contact or task in the folders {folders:?} of {}",
            pst.display()
        ));
    }
    let after = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
    if before != after {
        return Err(format!("{} changed while it was read", pst.display()));
    }
    Ok(convert(&read))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, rc::Rc};

    use ecm_pst::pim::{Attachment, AttachmentContent, Name, PS_PUBLIC_STRINGS};

    use super::*;
    use crate::graph_pim_cmd::{
        ews_contact_properties, ews_event_properties, graph_contact, graph_event,
    };

    /// FILETIME of a UTC time.
    fn filetime(text: &str) -> i64 {
        (ecm_model::item::parse_utc(text).unwrap() + 11_644_473_600) * 10_000_000
    }

    /// Properties with named ones numbered from 0x8000 in the order given.
    fn properties(tagged: Vec<(u16, Value)>, named: Vec<(Name, Value)>) -> Properties {
        let mut names = HashMap::new();
        let mut values: std::collections::BTreeMap<u16, Value> = tagged.into_iter().collect();
        for (n, (name, value)) in named.into_iter().enumerate() {
            let id = 0x8000 + n as u16;
            names.insert(name, id);
            values.insert(id, value);
        }
        Properties::new(values, Rc::new(names))
    }

    fn le16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    /// A `TZDEFINITION` with no rule of its own matching a known zone by name.
    fn paris_zone(name: &str) -> Vec<u8> {
        let mut out = vec![2, 1];
        let id: Vec<u16> = name.encode_utf16().collect();
        le16(&mut out, (2 + 2 + id.len() * 2 + 2) as u16);
        le16(&mut out, 2);
        le16(&mut out, id.len() as u16);
        for unit in id {
            le16(&mut out, unit);
        }
        le16(&mut out, 1);
        out.extend_from_slice(&[2, 1]);
        le16(&mut out, 0x3E);
        le16(&mut out, 2);
        le16(&mut out, 2000);
        out.extend_from_slice(&[0; 14]);
        out.extend_from_slice(&(-60i32).to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&(-60i32).to_le_bytes());
        for (month, hour) in [(10, 3), (3, 2)] {
            for v in [0, month, 0, 5, hour, 0, 0, 0] {
                le16(&mut out, v);
            }
        }
        out
    }

    #[test]
    fn writes_create_xml_in_schema_order_and_local_time() {
        let element = |name: &str, children: Vec<Element>| node(name, children);
        let event = element(
            "CalendarItem",
            vec![
                leaf("Subject", "Fish & chips"),
                leaf("Importance", "High"),
                leaf("ReminderIsSet", "false"),
                leaf("Start", "2024-07-15T08:30:00Z"),
                leaf("End", "2024-07-15T09:00:00Z"),
                leaf("IsAllDayEvent", "false"),
                node(
                    "RequiredAttendees",
                    vec![node(
                        "Attendee",
                        vec![mailbox(Some("Ann"), "ann@example.test")],
                    )],
                ),
                leaf("IsOrganizer", "false"),
                leaf("StartWallClock", "2024-07-15T10:30:00+02:00"),
                leaf("EndWallClock", "2024-07-15T11:00:00+02:00"),
                leaf("StartTimeZoneId", "Romance Standard Time"),
            ],
        );
        let (xml, notes) = create_xml(&event);
        assert_eq!(
            xml,
            concat!(
                "<t:CalendarItem><t:Subject>Fish &amp; chips</t:Subject><t:Importance>High</t:Importance>",
                "<t:ReminderIsSet>false</t:ReminderIsSet><t:Start>2024-07-15T10:30:00</t:Start>",
                "<t:End>2024-07-15T11:00:00</t:End><t:IsAllDayEvent>false</t:IsAllDayEvent>",
                r#"<t:StartTimeZone Id="Romance Standard Time"/><t:EndTimeZone Id="Romance Standard Time"/></t:CalendarItem>"#
            )
        );
        assert_eq!(
            notes,
            ["its organizer and attendees (1): the copy is the user's own event"]
        );

        let contact = element(
            "Contact",
            vec![
                leaf("DisplayName", "Ann"),
                node(
                    "PhysicalAddresses",
                    vec![Element {
                        name: "Entry".into(),
                        attributes: vec![("Key".into(), "Home".into())],
                        children: vec![
                            leaf("PostalCode", "75001"),
                            leaf("CountryOrRegion", "France"),
                        ],
                        ..Element::default()
                    }],
                ),
                leaf("Categories", "ignored: not a list"),
                leaf("HasPicture", "true"),
            ],
        );
        let (xml, _) = create_xml(&contact);
        assert_eq!(
            xml,
            concat!(
                "<t:Contact><t:Categories>ignored: not a list</t:Categories><t:DisplayName>Ann</t:DisplayName>",
                r#"<t:PhysicalAddresses><t:Entry Key="Home"><t:CountryOrRegion>France</t:CountryOrRegion>"#,
                "<t:PostalCode>75001</t:PostalCode></t:Entry></t:PhysicalAddresses></t:Contact>"
            )
        );
    }

    #[test]
    fn names_zones_by_id_iana_name_or_rules() {
        for name in ["Romance Standard Time", "Europe/Paris", "Paris, Madrid"] {
            let zone = TimeZone::parse(&paris_zone(name)).unwrap();
            assert_eq!(
                windows_id(&zone, 2024).as_deref(),
                Some("Romance Standard Time"),
                "{name}"
            );
        }
        // Back to standard time on the fourth Sunday of October: the last one
        // in 2018, not in 2017.
        let mut zone = TimeZone::parse(&paris_zone("Customized Time Zone")).unwrap();
        zone.rules[0].standard.day = 4;
        assert_eq!(
            windows_id(&zone, 2018).as_deref(),
            Some("Romance Standard Time")
        );
        assert_eq!(windows_id(&zone, 2017), None);
    }

    #[test]
    fn puts_an_all_day_event_without_a_zone_on_its_day() {
        let appointment = Item {
            properties: properties(
                vec![(0x001A, Value::Text("IPM.Appointment".into()))],
                vec![
                    (
                        Name::Number(PSETID_APPOINTMENT, 0x820D),
                        Value::Time(filetime("2014-01-08T23:00:00Z")),
                    ),
                    (
                        Name::Number(PSETID_APPOINTMENT, 0x820E),
                        Value::Time(filetime("2014-01-09T23:00:00Z")),
                    ),
                    (Name::Number(PSETID_APPOINTMENT, 0x8215), Value::Bool(true)),
                ],
            ),
            ..Item::default()
        };
        let (element, _) = calendar_item(&appointment);
        assert_eq!(element.child_text("Start"), Some("2014-01-09T00:00:00Z"));
        assert_eq!(element.child_text("End"), Some("2014-01-10T00:00:00Z"));
    }

    #[test]
    fn converts_an_appointment_in_its_own_time_zone() {
        let appointment = Item {
            properties: properties(
                vec![
                    (0x001A, Value::Text("IPM.Appointment".into())),
                    (0x0037, Value::Text("Dentist".into())),
                    (0x0017, Value::Integer(2)),
                    (0x1000, Value::Text("Bring the X-rays".into())),
                    (0x0042, Value::Text("Ann".into())),
                    (0x5D02, Value::Text("ann@example.test".into())),
                ],
                vec![
                    (
                        Name::Number(PSETID_APPOINTMENT, 0x820D),
                        Value::Time(filetime("2024-07-15T08:30:00Z")),
                    ),
                    (
                        Name::Number(PSETID_APPOINTMENT, 0x820E),
                        Value::Time(filetime("2024-07-15T09:00:00Z")),
                    ),
                    (
                        Name::Number(PSETID_APPOINTMENT, 0x8208),
                        Value::Text("Rue X".into()),
                    ),
                    (Name::Number(PSETID_APPOINTMENT, 0x8205), Value::Integer(3)),
                    (Name::Number(PSETID_COMMON, 0x8503), Value::Bool(true)),
                    (Name::Number(PSETID_COMMON, 0x8501), Value::Integer(15)),
                    (
                        Name::String(PS_PUBLIC_STRINGS, "Keywords".into()),
                        Value::Texts(vec!["Santé".into()]),
                    ),
                    (
                        Name::Number(PSETID_APPOINTMENT, 0x825E),
                        Value::Binary(paris_zone("Europe/Paris")),
                    ),
                ],
            ),
            ..Item::default()
        };
        let (element, notes) = calendar_item(&appointment);
        assert!(notes.is_empty(), "{notes:?}");
        let (event, lost) = graph_event(&element);
        assert!(lost.is_empty(), "{lost:?}");
        assert_eq!(
            event["start"],
            serde_json::json!({ "dateTime": "2024-07-15T10:30:00", "timeZone": "Romance Standard Time" })
        );
        assert_eq!(event["showAs"], "oof");
        assert_eq!(event["importance"], "high");
        assert_eq!(event["reminderMinutesBeforeStart"], 15);
        assert_eq!(event["location"]["displayName"], "Rue X");
        let props = ews_event_properties(&element);
        assert_eq!(props["start"], "2024-07-15T08:30:00Z");
        assert_eq!(props["categories"], "Santé");
        assert_eq!(props["body"], "Bring the X-rays");
    }

    #[test]
    fn converts_a_yearly_series_and_its_occurrences() {
        let recurrence = Recurrence {
            frequency: Frequency::Yearly,
            pattern: Pattern::Month { day: 15 },
            period: 12,
            first_day_of_week: 0,
            end: End::Never,
            occurrences: 10,
            start_date: (((ecm_model::item::days_from_civil(2014, 6, 15) * 86_400)
                + 11_644_473_600)
                / 60) as u32,
            end_date: 0,
            deleted: Vec::new(),
            modified: Vec::new(),
            start_offset: 0,
            end_offset: 1440,
            exceptions: Vec::new(),
            regenerating: false,
        };
        let element = recurrence_element(&recurrence, false).unwrap();
        let pattern = element.child("AbsoluteYearlyRecurrence").unwrap();
        assert_eq!(pattern.child_text("DayOfMonth"), Some("15"));
        assert_eq!(pattern.child_text("Month"), Some("June"));
        assert_eq!(
            element
                .path(&["NoEndRecurrence", "StartDate"])
                .map(|e| e.text.as_str()),
            Some("2014-06-15")
        );
    }

    #[test]
    fn keeps_a_birthday_on_its_day_whatever_the_zone_it_was_saved_in() {
        let paris = properties(
            vec![(0x3A42, Value::Time(filetime("1988-01-16T23:00:00Z")))],
            Vec::new(),
        );
        assert_eq!(birthday(&paris).as_deref(), Some("1988-01-17T11:59:00Z"));
        let montreal = properties(
            vec![(0x3A42, Value::Time(filetime("1988-01-17T05:00:00Z")))],
            Vec::new(),
        );
        assert_eq!(birthday(&montreal).as_deref(), Some("1988-01-17T11:59:00Z"));
        let local = properties(
            vec![(0x3A42, Value::Time(filetime("1988-01-16T23:00:00Z")))],
            vec![(
                Name::Number(PSETID_ADDRESS, 0x80DE),
                Value::Time(filetime("1988-01-17T00:00:00Z")),
            )],
        );
        assert_eq!(birthday(&local).as_deref(), Some("1988-01-17T11:59:00Z"));
    }

    #[test]
    fn converts_a_contact_with_its_photo_and_addresses() {
        let photo_bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        let item = Item {
            properties: properties(
                vec![
                    (0x001A, Value::Text("IPM.Contact".into())),
                    (0x3001, Value::Text("Ann Example".into())),
                    (0x3A06, Value::Text("Ann".into())),
                    (0x3A11, Value::Text("Example".into())),
                    (0x3A1C, Value::Text("+33 6 00 00 00 00 ".into())),
                    (0x3A24, Value::Text("+33 1 00 00 00 01".into())),
                    (0x3A26, Value::Text("France".into())),
                    (0x3A59, Value::Text("Paris".into())),
                ],
                vec![
                    (
                        Name::Number(PSETID_ADDRESS, 0x8005),
                        Value::Text("Example, Ann".into()),
                    ),
                    (
                        Name::Number(PSETID_ADDRESS, 0x8083),
                        Value::Text("ann@example.test".into()),
                    ),
                    (
                        Name::Number(PSETID_ADDRESS, 0x8082),
                        Value::Text("SMTP".into()),
                    ),
                ],
            ),
            attachments: vec![Attachment {
                properties: properties(
                    vec![
                        (0x7FFF, Value::Bool(true)),
                        (0x3707, Value::Text("ContactPicture.jpg".into())),
                    ],
                    Vec::new(),
                ),
                content: AttachmentContent::File(photo_bytes.clone()),
            }],
            ..Item::default()
        };
        let element = contact(&item);
        assert_eq!(photo(&element), Some(photo_bytes.clone()));
        let (json, lost) = graph_contact(&element);
        assert_eq!(json["fileAs"], "Example, Ann");
        assert_eq!(json["mobilePhone"], "+33 6 00 00 00 00");
        assert_eq!(json["emailAddresses"][0]["address"], "ann@example.test");
        assert_eq!(json["homeAddress"]["city"], "Paris");
        assert_eq!(json["businessAddress"]["countryOrRegion"], "France");
        assert_eq!(lost, ["its BusinessFax number (no such field in Graph)"]);
        let props = ews_contact_properties(&element, Some(&photo_bytes));
        assert_eq!(props["display_name"], "Ann Example");
        assert!(props.contains_key("photo"));
    }
}
