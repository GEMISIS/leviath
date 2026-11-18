//! Calendar time: the user's local zone, and the few date shapes Leviath
//! reads and prints.
//!
//! Run records store Unix seconds ([`crate::duration::now_secs`]) and need no
//! calendar. This module is for the places a person or a model reads a date:
//! the dashboard, `lev rage`, the `current_time` tool, and a provider's
//! published timestamps. Everything fallible in the `time` crate is settled
//! here, so a caller gets a value or an `Option` and never an error type.

pub use time::{Date, OffsetDateTime, UtcOffset};

/// The host's UTC offset at `at`, which is what makes a past timestamp print
/// with the offset in force on that day rather than today's.
///
/// UTC when the host cannot say: a timestamp in a known zone is more use than
/// no timestamp.
pub fn local_offset(at: OffsetDateTime) -> UtcOffset {
    UtcOffset::local_offset_at(at).unwrap_or(UtcOffset::UTC)
}

/// The current instant in the host's local zone.
pub fn local_now() -> OffsetDateTime {
    in_local(OffsetDateTime::now_utc())
}

/// `secs` (Unix seconds) in the host's local zone, or `None` for a value no
/// calendar holds.
pub fn local_at(secs: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(secs).ok().map(in_local)
}

/// `at` moved to the host's local zone.
fn in_local(at: OffsetDateTime) -> OffsetDateTime {
    at.to_offset(local_offset(at))
}

/// Today's date in UTC, the calendar a provider's published dates are in.
pub fn today_utc() -> Date {
    OffsetDateTime::now_utc().date()
}

/// `YYYY-MM-DD`.
pub fn date(day: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        day.year(),
        u8::from(day.month()),
        day.day()
    )
}

/// `HH:MM:SS`, on a 24-hour clock.
pub fn time_of_day(at: OffsetDateTime) -> String {
    format!("{:02}:{:02}:{:02}", at.hour(), at.minute(), at.second())
}

/// `YYYY-MM-DD HH:MM:SS`.
pub fn date_time(at: OffsetDateTime) -> String {
    format!("{} {}", date(at.date()), time_of_day(at))
}

/// An offset as `+HH:MM` or `-HH:MM`. UTC is `+00:00`.
pub fn offset(offset: UtcOffset) -> String {
    let sign = if offset.is_negative() { '-' } else { '+' };
    format!(
        "{sign}{:02}:{:02}",
        offset.whole_hours().unsigned_abs(),
        offset.minutes_past_hour().unsigned_abs()
    )
}

/// RFC 3339 to the second with a numeric offset: `2026-08-18T14:32:07-05:00`.
pub fn rfc3339(at: OffsetDateTime) -> String {
    format!(
        "{}T{}{}",
        date(at.date()),
        time_of_day(at),
        offset(at.offset())
    )
}

/// RFC 3339 to the second in UTC, with the `Z` suffix: `2026-08-18T19:32:07Z`.
pub fn rfc3339_utc(at: OffsetDateTime) -> String {
    let at = at.to_offset(UtcOffset::UTC);
    format!("{}T{}Z", date(at.date()), time_of_day(at))
}

/// An RFC 3339 timestamp as an instant, or `None` when `text` is not one.
pub fn parse_rfc3339(text: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
}

/// A `YYYY-MM-DD` date, or `None` when `text` is not one.
pub fn parse_date(text: &str) -> Option<Date> {
    Date::parse(text, &time::format_description::well_known::Iso8601::DATE).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-18T19:32:07Z, a Tuesday.
    const INSTANT: i64 = 1_787_081_527;

    fn at(hours: i8, minutes: i8) -> OffsetDateTime {
        let offset = UtcOffset::from_hms(hours, minutes, 0).expect("a real offset");
        OffsetDateTime::from_unix_timestamp(INSTANT)
            .expect("a real instant")
            .to_offset(offset)
    }

    #[test]
    fn an_instant_prints_in_each_shape() {
        let chicago = at(-5, 0);
        assert_eq!(date(chicago.date()), "2026-08-18");
        assert_eq!(time_of_day(chicago), "14:32:07");
        assert_eq!(date_time(chicago), "2026-08-18 14:32:07");
        assert_eq!(rfc3339(chicago), "2026-08-18T14:32:07-05:00");
        assert_eq!(rfc3339_utc(chicago), "2026-08-18T19:32:07Z");
    }

    /// The sign belongs to the whole offset, so a half-hour zone west of UTC
    /// is `-03:30` and never `-03:-30`; UTC keeps a numeric offset.
    #[test]
    fn an_offset_prints_with_one_sign() {
        assert_eq!(offset(at(5, 30).offset()), "+05:30");
        assert_eq!(offset(at(-3, -30).offset()), "-03:30");
        assert_eq!(offset(UtcOffset::UTC), "+00:00");
        assert_eq!(rfc3339(at(0, 0)), "2026-08-18T19:32:07+00:00");
    }

    #[test]
    fn an_rfc3339_timestamp_parses_with_any_offset_and_fraction() {
        let seconds = |text| parse_rfc3339(text).map(OffsetDateTime::unix_timestamp);
        assert_eq!(seconds("2026-08-18T19:32:07Z"), Some(INSTANT));
        assert_eq!(seconds("2026-08-18T14:32:07-05:00"), Some(INSTANT));
        assert_eq!(seconds("2026-08-18T19:32:07.250Z"), Some(INSTANT));
        assert_eq!(seconds("2026-08-18"), None);
        assert_eq!(seconds("soon"), None);
    }

    #[test]
    fn a_date_parses_only_as_year_month_day() {
        let day = parse_date("2026-08-18").expect("a real date");
        assert_eq!(date(day), "2026-08-18");
        assert!(parse_date("2026-02-28").expect("a real date") < day);
        for not_a_date in ["", "2026", "2026-08", "2026-13-01", "2026-02-30", "a-b-c"] {
            assert_eq!(parse_date(not_a_date), None, "{not_a_date}");
        }
    }

    /// Whatever zone the host is in, the local rendering is the same instant.
    #[test]
    fn local_time_is_the_same_instant_in_the_hosts_zone() {
        let local = local_at(INSTANT).expect("a real instant");
        assert_eq!(local.unix_timestamp(), INSTANT);
        assert_eq!(local.offset(), local_offset(local));
        assert_eq!(local_at(i64::MAX), None);

        let now = local_now();
        assert!((now.unix_timestamp() - crate::duration::now_secs()).abs() <= 5);
        assert!((now.date() - today_utc()).whole_days().abs() <= 1);
    }
}
