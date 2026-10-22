//! Items as an inventory records them, from a source or a target.

use serde::{Deserialize, Serialize};

/// One item of a folder: the properties used to match it with its counterpart and
/// to detect what a migration damaged.
///
/// Times are seconds since the Unix epoch, UTC.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRecord {
    /// Identifier within its store: PST node id, or EWS item id.
    pub key: String,
    /// Folder path from the store's top folder, `/`-separated. `""` is the top
    /// folder itself.
    pub folder: String,
    /// Item class, e.g. `IPM.Note`.
    pub class: String,
    /// `Message-ID` header (`PR_INTERNET_MESSAGE_ID`), trimmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Sender address, lower-cased, or display name when there is no address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    /// When it was sent (`PR_CLIENT_SUBMIT_TIME`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent: Option<i64>,
    /// When it was received (`PR_MESSAGE_DELIVERY_TIME`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received: Option<i64>,
    /// When this copy was created in its store (`PR_CREATION_TIME`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    /// Size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Whether it has attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_attachments: Option<bool>,
    /// Whether it is read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<bool>,
    /// Whether it carries Internet headers (`PR_TRANSPORT_MESSAGE_HEADERS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_headers: Option<bool>,
    /// Native body format (`PR_NATIVE_BODY_INFO`): 1 plain text, 2 RTF, 3 HTML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_body: Option<u8>,
    /// Whether the body has no text (its preview is empty). A copy with an empty
    /// body whose source has one lost its content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_empty: Option<bool>,
}

impl ItemRecord {
    /// Whether this is a mail-like item (`IPM.Note` and its sub-classes, reports,
    /// meeting messages), matched by `Message-ID`.
    pub fn is_message(&self) -> bool {
        let class = self.class.to_ascii_lowercase();
        class == "ipm.note"
            || class.starts_with("ipm.note.")
            || class.starts_with("report.")
            || class.starts_with("ipm.schedule.meeting.")
    }

    /// A key that survives the usual migration damage: lower-cased subject without
    /// reply prefixes, and the send time to the minute. `None` without a send time.
    pub fn fingerprint(&self) -> Option<String> {
        let sent = self.sent?;
        Some(format!(
            "{}|{}",
            normalize_subject(self.subject.as_deref().unwrap_or("")),
            sent.div_euclid(60)
        ))
    }
}

/// Lower-cases a subject, collapses white space and drops `RE:`/`TR:`/`FW:`-style
/// prefixes, which clients add and some migrations rewrite.
pub fn normalize_subject(subject: &str) -> String {
    let mut s = subject
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    loop {
        let trimmed = ["re:", "tr:", "fw:", "fwd:", "réf:", "aw:", "wg:"]
            .iter()
            .find_map(|prefix| s.strip_prefix(prefix))
            .map(|rest| rest.trim_start().to_owned());
        match trimmed {
            Some(rest) => s = rest,
            None => return s,
        }
    }
}

/// Converts a Windows `FILETIME` (100 ns since 1601-01-01) to Unix seconds.
pub fn filetime_to_unix(filetime: i64) -> i64 {
    filetime.div_euclid(10_000_000) - 11_644_473_600
}

/// Parses an ISO 8601 UTC time as EWS writes it (`2024-12-30T03:19:13Z`, optionally
/// with fractional seconds) to Unix seconds.
pub fn parse_utc(text: &str) -> Option<i64> {
    let text = text.trim().strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let time = time.split('.').next()?;
    let mut time = time.splitn(3, ':').map(str::parse::<i64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Formats Unix seconds as `YYYY-MM-DD HH:MM` UTC.
pub fn format_utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60
    )
}

/// Formats Unix seconds as ISO 8601 UTC, as EWS expects: `2024-12-30T03:19:13Z`.
pub fn format_iso(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date of a day since 1970-01-01: year, month, day.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_convert_both_ways() {
        let t = parse_utc("2024-12-30T03:19:13Z").unwrap();
        assert_eq!(t, 1_735_528_753);
        assert_eq!(format_utc(t), "2024-12-30 03:19");
        assert_eq!(
            parse_utc("2026-09-01T05:02:38.123Z"),
            parse_utc("2026-09-01T05:02:38Z")
        );
        assert_eq!(parse_utc("2026-09-01 05:02:38"), None);
        // 2024-12-30T02:35:53Z as a FILETIME.
        assert_eq!(filetime_to_unix(133_799_997_530_000_000), 1_735_526_153);
        assert_eq!(format_utc(1_735_526_153), "2024-12-30 02:35");
        assert_eq!(format_utc(0), "1970-01-01 00:00");
    }

    #[test]
    fn subjects_normalize_reply_prefixes() {
        assert_eq!(
            normalize_subject("  RE: TR:  Devis   Montréal "),
            "devis montréal"
        );
        assert_eq!(normalize_subject("Re:Re: x"), "x");
    }

    #[test]
    fn fingerprints_ignore_seconds_and_prefixes() {
        let a = ItemRecord {
            subject: Some("RE: Devis".into()),
            sent: Some(1_000_020),
            ..Default::default()
        };
        let b = ItemRecord {
            subject: Some("devis".into()),
            sent: Some(1_000_050),
            ..Default::default()
        };
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(ItemRecord::default().fingerprint(), None);
    }

    #[test]
    fn message_classes() {
        let item = |class: &str| ItemRecord {
            class: class.into(),
            ..Default::default()
        };
        assert!(item("IPM.Note").is_message());
        assert!(item("IPM.Note.SMIME").is_message());
        assert!(item("REPORT.IPM.Note.NDR").is_message());
        assert!(!item("IPM.Appointment").is_message());
        assert!(!item("IPM.Contact").is_message());
    }
}
