//! The calendar blobs of [MS-OXOCAL]: a series' recurrence
//! (`PidLidAppointmentRecur`, an `AppointmentRecurrencePattern`) and a time zone
//! (`PidLidAppointmentTimeZoneDefinitionStartDisplay`, a `TZDEFINITION`).
//!
//! Times in a recurrence are local to the series' time zone, in minutes since
//! 1601-01-01; [`TimeZone`] converts between UTC and local times.

use ecm_model::item::{civil_from_days, days_from_civil};

/// Seconds from 1601-01-01 to 1970-01-01.
const EPOCH_1601: i64 = 11_644_473_600;

/// Minutes since 1601-01-01 (local) as seconds since 1970-01-01 (local).
pub fn minutes_to_seconds(minutes: u32) -> i64 {
    i64::from(minutes) * 60 - EPOCH_1601
}

/// A `FILETIME` as seconds since 1970-01-01.
pub fn filetime_to_seconds(filetime: i64) -> i64 {
    filetime.div_euclid(10_000_000) - EPOCH_1601
}

/// Seconds since 1970-01-01 as `YYYY-MM-DDTHH:MM:SS`, without a zone.
pub fn format_seconds(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let rest = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Day of the week of a day since 1970-01-01: 0 for Sunday.
fn weekday(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    let next = if month == 12 {
        days_from_civil(year + 1, 1, 1)
    } else {
        days_from_civil(year, month + 1, 1)
    };
    next - days_from_civil(year, month, 1)
}

/// A little-endian reader that gives `None` past the end.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let bytes = self.data.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(bytes)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    fn utf16(&mut self, chars: usize) -> Option<String> {
        let units: Vec<u16> = self
            .bytes(chars.checked_mul(2)?)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        Some(String::from_utf16_lossy(&units))
    }

    /// A block whose size comes first, skipped.
    fn skip_block(&mut self) -> Option<()> {
        let size = self.u32()? as usize;
        self.bytes(size).map(|_| ())
    }
}

/// How often a series repeats (`RecurFrequency`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    /// Every n days.
    Daily,
    /// Every n weeks.
    Weekly,
    /// Every n months.
    Monthly,
    /// Every n years.
    Yearly,
}

/// The days a series falls on (`PatternType` and `PatternTypeSpecific`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// Every `period` minutes, in whole days.
    Day,
    /// On these days of the week: bit 0 for Sunday to bit 6 for Saturday.
    Week {
        /// Days of the week.
        days: u32,
    },
    /// On this day of the month.
    Month {
        /// Day of the month, 1 to 31.
        day: u32,
    },
    /// On the last day of the month.
    MonthEnd,
    /// On the n-th of these days of the week in the month (5 for the last).
    MonthNth {
        /// Days of the week, as for [`Pattern::Week`].
        days: u32,
        /// 1 to 4, or 5 for the last.
        n: u32,
    },
    /// A Hijri calendar pattern, or one this reader does not know.
    Other(u16),
}

/// When a series ends (`EndType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// On [`Recurrence::end_date`].
    Date,
    /// After [`Recurrence::occurrences`] occurrences.
    Count,
    /// Never.
    Never,
}

/// One modified occurrence of a series (`ExceptionInfo` and
/// `ExtendedException`). Times are local, in minutes since 1601-01-01.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exception {
    /// Its start.
    pub start: u32,
    /// Its end.
    pub end: u32,
    /// The start the occurrence had in the series.
    pub original_start: u32,
    /// Its subject, when it has its own.
    pub subject: Option<String>,
    /// Its location, when it has its own.
    pub location: Option<String>,
    /// Its reminder, in minutes before the start, when it has its own.
    pub reminder_minutes: Option<u32>,
    /// Whether its reminder is set, when it has its own.
    pub reminder_set: Option<bool>,
    /// Its free/busy status (`PidLidBusyStatus`), when it has its own.
    pub busy_status: Option<u32>,
    /// Whether it is all-day, when it has its own.
    pub all_day: Option<bool>,
    /// Whether its body differs from the series' (it is then in the embedded
    /// message).
    pub own_body: bool,
}

/// A series' recurrence: its pattern, its range, and its deleted and modified
/// occurrences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recurrence {
    /// How often it repeats.
    pub frequency: Frequency,
    /// On which days.
    pub pattern: Pattern,
    /// Every how many: minutes for [`Frequency::Daily`], weeks, or months (12 for
    /// yearly).
    pub period: u32,
    /// The first day of the week: 0 for Sunday.
    pub first_day_of_week: u32,
    /// How it ends.
    pub end: End,
    /// The number of occurrences, for [`End::Count`].
    pub occurrences: u32,
    /// The day of the first occurrence (local midnight, minutes since 1601).
    pub start_date: u32,
    /// The day of the last occurrence, for [`End::Date`].
    pub end_date: u32,
    /// The original days of the deleted occurrences, modified ones included.
    pub deleted: Vec<u32>,
    /// The days of the modified occurrences.
    pub modified: Vec<u32>,
    /// The start of each occurrence, in minutes after midnight.
    pub start_offset: u32,
    /// The end of each occurrence, in minutes after midnight.
    pub end_offset: u32,
    /// The modified occurrences.
    pub exceptions: Vec<Exception>,
    /// Whether the next occurrence comes a period after the last one was
    /// completed (`SlidingFlag`, tasks only).
    pub regenerating: bool,
}

/// `ARO_*` flags of an exception.
const ARO_SUBJECT: u16 = 0x0001;
const ARO_MEETINGTYPE: u16 = 0x0002;
const ARO_REMINDERDELTA: u16 = 0x0004;
const ARO_REMINDER: u16 = 0x0008;
const ARO_LOCATION: u16 = 0x0010;
const ARO_BUSYSTATUS: u16 = 0x0020;
const ARO_ATTACHMENT: u16 = 0x0040;
const ARO_SUBTYPE: u16 = 0x0080;
const ARO_APPTCOLOR: u16 = 0x0100;
const ARO_EXCEPTIONAL_BODY: u16 = 0x0200;

impl Recurrence {
    /// Reads an `AppointmentRecurrencePattern`; `None` when it is not one.
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut r = Reader::new(data);
        let (_reader, _writer) = (r.u16()?, r.u16()?);
        let frequency = match r.u16()? {
            0x200A => Frequency::Daily,
            0x200B => Frequency::Weekly,
            0x200C => Frequency::Monthly,
            0x200D => Frequency::Yearly,
            _ => return None,
        };
        let pattern_type = r.u16()?;
        let _calendar = r.u16()?;
        let _first_date_time = r.u32()?;
        let period = r.u32()?;
        let sliding = r.u32()?;
        let pattern = match pattern_type {
            0x0000 => Pattern::Day,
            0x0001 => Pattern::Week { days: r.u32()? },
            0x0002 => Pattern::Month { day: r.u32()? },
            0x0003 => Pattern::MonthNth {
                days: r.u32()?,
                n: r.u32()?,
            },
            0x0004 => {
                r.u32()?;
                Pattern::MonthEnd
            }
            0x000A | 0x000C => {
                r.u32()?;
                Pattern::Other(pattern_type)
            }
            0x000B => {
                r.u32()?;
                r.u32()?;
                Pattern::Other(pattern_type)
            }
            other => Pattern::Other(other),
        };
        let end = match r.u32()? {
            0x2021 => End::Date,
            0x2022 => End::Count,
            _ => End::Never,
        };
        let occurrences = r.u32()?;
        let first_day_of_week = r.u32()?;
        let list = |r: &mut Reader| -> Option<Vec<u32>> {
            let count = r.u32()? as usize;
            (0..count).map(|_| r.u32()).collect()
        };
        let deleted = list(&mut r)?;
        let modified = list(&mut r)?;
        let start_date = r.u32()?;
        let end_date = r.u32()?;

        let mut recurrence = Recurrence {
            frequency,
            pattern,
            period,
            first_day_of_week,
            end,
            occurrences,
            start_date,
            end_date,
            deleted,
            modified,
            start_offset: 0,
            end_offset: 0,
            exceptions: Vec::new(),
            regenerating: sliding != 0,
        };
        // A task's recurrence stops there; an appointment's goes on.
        let Some(_reader2) = r.u32() else {
            return Some(recurrence);
        };
        let writer2 = r.u32()?;
        recurrence.start_offset = r.u32()?;
        recurrence.end_offset = r.u32()?;
        let count = r.u16()?;
        let mut flags = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let mut exception = Exception {
                start: r.u32()?,
                end: r.u32()?,
                original_start: r.u32()?,
                ..Exception::default()
            };
            let overrides = r.u16()?;
            if overrides & ARO_SUBJECT != 0 {
                let _length = r.u16()?;
                let length = r.u16()?;
                exception.subject = Some(ansi(r.bytes(usize::from(length))?));
            }
            if overrides & ARO_MEETINGTYPE != 0 {
                r.u32()?;
            }
            if overrides & ARO_REMINDERDELTA != 0 {
                exception.reminder_minutes = Some(r.u32()?);
            }
            if overrides & ARO_REMINDER != 0 {
                exception.reminder_set = Some(r.u32()? != 0);
            }
            if overrides & ARO_LOCATION != 0 {
                let _length = r.u16()?;
                let length = r.u16()?;
                exception.location = Some(ansi(r.bytes(usize::from(length))?));
            }
            if overrides & ARO_BUSYSTATUS != 0 {
                exception.busy_status = Some(r.u32()?);
            }
            if overrides & ARO_ATTACHMENT != 0 {
                r.u32()?;
            }
            if overrides & ARO_SUBTYPE != 0 {
                exception.all_day = Some(r.u32()? != 0);
            }
            if overrides & ARO_APPTCOLOR != 0 {
                r.u32()?;
            }
            exception.own_body = overrides & ARO_EXCEPTIONAL_BODY != 0;
            flags.push(overrides);
            recurrence.exceptions.push(exception);
        }
        r.skip_block()?;
        // Unicode subjects and locations, when the writer kept them.
        for (exception, overrides) in recurrence.exceptions.iter_mut().zip(flags) {
            if read_extended(&mut r, writer2, overrides, exception).is_none() {
                break;
            }
        }
        Some(recurrence)
    }

    /// The deleted occurrences that are not modified ones: those removed from
    /// the series.
    pub fn removed(&self) -> Vec<u32> {
        self.deleted
            .iter()
            .filter(|day| {
                !self
                    .exceptions
                    .iter()
                    .any(|e| e.original_start / 1440 == **day / 1440)
            })
            .copied()
            .collect()
    }
}

/// An `ExtendedException`: the Unicode subject and location of an exception.
fn read_extended(
    r: &mut Reader,
    writer2: u32,
    overrides: u16,
    exception: &mut Exception,
) -> Option<()> {
    if writer2 >= 0x3009 {
        r.skip_block()?;
    }
    r.skip_block()?;
    if overrides & (ARO_SUBJECT | ARO_LOCATION) != 0 {
        r.u32()?;
        r.u32()?;
        r.u32()?;
        if overrides & ARO_SUBJECT != 0 {
            let length = usize::from(r.u16()?);
            exception.subject = Some(r.utf16(length)?);
        }
        if overrides & ARO_LOCATION != 0 {
            let length = usize::from(r.u16()?);
            exception.location = Some(r.utf16(length)?);
        }
        r.skip_block()?;
    }
    Some(())
}

/// Windows-1252 bytes as a string.
fn ansi(bytes: &[u8]) -> String {
    encoding_rs::WINDOWS_1252
        .decode_without_bom_handling(bytes)
        .0
        .trim_end_matches('\0')
        .to_owned()
}

/// A transition of a time zone rule (`SYSTEMTIME`): with no year, the `week`-th
/// `weekday` of `month` (5 for the last).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Transition {
    /// Year, or 0 for every year.
    pub year: u16,
    /// Month, 1 to 12; 0 when the zone has no daylight time.
    pub month: u16,
    /// Day of the week, 0 for Sunday.
    pub weekday: u16,
    /// Week of the month, 1 to 5; or the day, with a year.
    pub day: u16,
    /// Hour, local.
    pub hour: u16,
    /// Minute.
    pub minute: u16,
}

impl Transition {
    fn read(r: &mut Reader) -> Option<Self> {
        let t = Transition {
            year: r.u16()?,
            month: r.u16()?,
            weekday: r.u16()?,
            day: r.u16()?,
            hour: r.u16()?,
            minute: r.u16()?,
        };
        r.u16()?;
        r.u16()?;
        Some(t)
    }

    /// The transition in `year`, in seconds since 1970-01-01, local.
    pub fn in_year(&self, year: i64) -> Option<i64> {
        if self.month == 0 {
            return None;
        }
        let month = i64::from(self.month);
        let day = if self.year != 0 {
            i64::from(self.day)
        } else {
            let first = days_from_civil(year, month, 1);
            let offset = (i64::from(self.weekday) + 7 - weekday(first)) % 7;
            let mut day = 1 + offset + 7 * (i64::from(self.day).clamp(1, 5) - 1);
            while day > days_in_month(year, month) {
                day -= 7;
            }
            day
        };
        Some(
            days_from_civil(year, month, day) * 86_400
                + i64::from(self.hour) * 3600
                + i64::from(self.minute) * 60,
        )
    }
}

/// One rule of a time zone (`TZRULE`): its offsets from a year on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// The year it starts applying.
    pub year: u16,
    /// Minutes to add to local time to get UTC.
    pub bias: i32,
    /// Added to `bias` in standard time.
    pub standard_bias: i32,
    /// Added to `bias` in daylight time.
    pub daylight_bias: i32,
    /// When standard time starts, in daylight time.
    pub standard: Transition,
    /// When daylight time starts, in standard time.
    pub daylight: Transition,
}

impl Rule {
    /// Whether daylight time applies at this local standard time.
    fn daylight_at(&self, standard_local: i64) -> bool {
        let year = civil_from_days(standard_local.div_euclid(86_400)).0;
        let (Some(start), Some(end)) = (self.daylight.in_year(year), self.standard.in_year(year))
        else {
            return false;
        };
        // The end is given in daylight time.
        let end = end + i64::from(self.daylight_bias - self.standard_bias) * 60;
        if start < end {
            start <= standard_local && standard_local < end
        } else {
            standard_local >= start || standard_local < end
        }
    }
}

/// A time zone (`TZDEFINITION`): its Windows id and its rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeZone {
    /// The Windows time zone id, e.g. `Romance Standard Time`.
    pub id: String,
    /// Its rules, by year.
    pub rules: Vec<Rule>,
}

impl TimeZone {
    /// Reads a `TZDEFINITION`; `None` when it is not one.
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut r = Reader::new(data);
        let (_major, _minor) = (r.u8()?, r.u8()?);
        let header = usize::from(r.u16()?);
        let header_start = r.at;
        let _reserved = r.u16()?;
        let chars = usize::from(r.u16()?);
        // Some writers count a NUL and what follows it in the name.
        let id = r
            .utf16(chars)?
            .split('\0')
            .next()
            .unwrap_or_default()
            .to_owned();
        // The header ends with the rule count.
        r.at = (header_start + header).checked_sub(2)?;
        let count = r.u16()?;
        let mut rules = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let (_major, _minor) = (r.u8()?, r.u8()?);
            let _reserved = r.u16()?;
            let _flags = r.u16()?;
            let year = r.u16()?;
            r.bytes(14)?;
            let bias = r.i32()?;
            let standard_bias = r.i32()?;
            let daylight_bias = r.i32()?;
            let standard = Transition::read(&mut r)?;
            let daylight = Transition::read(&mut r)?;
            rules.push(Rule {
                year,
                bias,
                standard_bias,
                daylight_bias,
                standard,
                daylight,
            });
        }
        rules.sort_by_key(|rule| rule.year);
        Some(TimeZone {
            id: id.trim_end_matches('\0').to_owned(),
            rules,
        })
    }

    fn rule(&self, seconds: i64) -> Option<&Rule> {
        let year = civil_from_days(seconds.div_euclid(86_400)).0;
        self.rules
            .iter()
            .rev()
            .find(|rule| i64::from(rule.year) <= year)
            .or_else(|| self.rules.first())
    }

    /// The offset of local time from UTC at this UTC time, in seconds.
    fn offset(&self, utc: i64) -> i64 {
        let Some(rule) = self.rule(utc) else {
            return 0;
        };
        let standard_local = utc - i64::from(rule.bias + rule.standard_bias) * 60;
        let bias = if rule.daylight_at(standard_local) {
            rule.bias + rule.daylight_bias
        } else {
            rule.bias + rule.standard_bias
        };
        -i64::from(bias) * 60
    }

    /// A UTC time (seconds since 1970) as local time.
    pub fn to_local(&self, utc: i64) -> i64 {
        utc + self.offset(utc)
    }

    /// A local time (seconds since 1970) as UTC: the earlier one when the local
    /// time happens twice.
    pub fn to_utc(&self, local: i64) -> i64 {
        let Some(rule) = self.rule(local) else {
            return local;
        };
        let daylight = local + i64::from(rule.bias + rule.daylight_bias) * 60;
        if self.to_local(daylight) == local {
            return daylight;
        }
        local + i64::from(rule.bias + rule.standard_bias) * 60
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn le32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    /// Minutes since 1601 of a local date and time.
    fn minutes(year: i64, month: i64, day: i64, hour: u32, minute: u32) -> u32 {
        ((days_from_civil(year, month, day) * 86_400 + EPOCH_1601) / 60) as u32 + hour * 60 + minute
    }

    #[test]
    fn converts_civil_dates() {
        assert_eq!(civil_from_days(-134_774), (1601, 1, 1));
        assert_eq!(weekday(days_from_civil(2026, 10, 21)), 3);
        assert_eq!(
            format_seconds(minutes_to_seconds(minutes(2024, 5, 6, 18, 30))),
            "2024-05-06T18:30:00"
        );
    }

    /// A `TZDEFINITION` for Paris: UTC+1, daylight from the last Sunday of March
    /// at 2:00 to the last Sunday of October at 3:00.
    fn paris() -> Vec<u8> {
        let mut out = vec![2, 1];
        let id: Vec<u16> = "Romance Standard Time".encode_utf16().collect();
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
    fn converts_between_utc_and_a_zones_local_time() {
        let zone = TimeZone::parse(&paris()).unwrap();
        assert_eq!(zone.id, "Romance Standard Time");
        let utc = |y, mo, d, h: i64| days_from_civil(y, mo, d) * 86_400 + h * 3600;
        // Winter: UTC+1; summer: UTC+2.
        assert_eq!(zone.to_local(utc(2024, 1, 15, 10)), utc(2024, 1, 15, 11));
        assert_eq!(zone.to_local(utc(2024, 7, 15, 10)), utc(2024, 7, 15, 12));
        // 2024-03-31 01:00 UTC is 03:00 local, just after the change.
        assert_eq!(zone.to_local(utc(2024, 3, 31, 0)), utc(2024, 3, 31, 1));
        assert_eq!(zone.to_local(utc(2024, 3, 31, 1)), utc(2024, 3, 31, 3));
        // 2024-10-27 00:59 UTC is still daylight time, 01:00 UTC no longer.
        assert_eq!(zone.to_local(utc(2024, 10, 27, 0)), utc(2024, 10, 27, 2));
        assert_eq!(zone.to_local(utc(2024, 10, 27, 1)), utc(2024, 10, 27, 2));
        assert_eq!(zone.to_utc(utc(2024, 7, 15, 12)), utc(2024, 7, 15, 10));
        assert_eq!(zone.to_utc(utc(2024, 1, 15, 11)), utc(2024, 1, 15, 10));
    }

    #[test]
    fn reads_a_weekly_series_with_a_modified_and_a_deleted_occurrence() {
        let monday = minutes(2024, 5, 6, 0, 0);
        let moved = minutes(2024, 5, 13, 0, 0);
        let deleted = minutes(2024, 5, 20, 0, 0);
        let mut out = Vec::new();
        for v in [0x3004, 0x3004, 0x200B, 0x0001, 0] {
            le16(&mut out, v);
        }
        for v in [0, 1, 0, 0b0010_0010, 0x2021, 10, 1] {
            le32(&mut out, v);
        }
        le32(&mut out, 2);
        le32(&mut out, moved);
        le32(&mut out, deleted);
        le32(&mut out, 1);
        le32(&mut out, moved);
        le32(&mut out, monday);
        le32(&mut out, minutes(2024, 6, 27, 0, 0));
        for v in [0x3006, 0x3009, 18 * 60 + 30, 19 * 60 + 30] {
            le32(&mut out, v);
        }
        le16(&mut out, 1);
        le32(&mut out, moved + 19 * 60);
        le32(&mut out, moved + 20 * 60);
        le32(&mut out, moved + 18 * 60 + 30);
        le16(&mut out, ARO_SUBJECT | ARO_LOCATION);
        for text in ["Yoga late", "Gym"] {
            le16(&mut out, text.len() as u16 + 1);
            le16(&mut out, text.len() as u16);
            out.extend_from_slice(text.as_bytes());
        }
        le32(&mut out, 0);
        // ExtendedException: change highlight, reserved, then Unicode texts.
        le32(&mut out, 4);
        le32(&mut out, 0);
        le32(&mut out, 0);
        le32(&mut out, moved + 19 * 60);
        le32(&mut out, moved + 20 * 60);
        le32(&mut out, moved + 18 * 60 + 30);
        for text in ["Yoga tardif", "Salle é"] {
            let units: Vec<u16> = text.encode_utf16().collect();
            le16(&mut out, units.len() as u16);
            for unit in units {
                le16(&mut out, unit);
            }
        }
        le32(&mut out, 0);
        le32(&mut out, 0);

        let recurrence = Recurrence::parse(&out).unwrap();
        assert_eq!(recurrence.frequency, Frequency::Weekly);
        assert_eq!(recurrence.pattern, Pattern::Week { days: 0b0010_0010 });
        assert_eq!(recurrence.end, End::Date);
        assert_eq!(recurrence.start_offset, 18 * 60 + 30);
        assert_eq!(recurrence.exceptions.len(), 1);
        let exception = &recurrence.exceptions[0];
        assert_eq!(exception.subject.as_deref(), Some("Yoga tardif"));
        assert_eq!(exception.location.as_deref(), Some("Salle é"));
        assert_eq!(exception.start, moved + 19 * 60);
        assert_eq!(recurrence.removed(), [deleted]);
    }
}
