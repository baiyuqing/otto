//! Time rules for scheduled items. Every function is generic over the time
//! zone so tests can use a fixed offset; callers pass [`chrono::Local`].
//!
//! A local time that does not exist (a daylight-saving gap) moves forward to
//! the first valid minute; an ambiguous one uses the earlier instant.
//! `Weekdays` means Monday to Friday.

use chrono::{
    DateTime, Datelike, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta,
    TimeZone, Utc,
};

use super::Repeat;

/// The furthest a one-shot item may be scheduled ahead.
pub const MAX_AHEAD_DAYS: i64 = 30;

/// How many calendar days a search for the next or previous matching day
/// scans; a weekend needs 2.
const SEARCH_DAYS: i64 = 10;

fn local_to_utc<Tz: TimeZone>(date: NaiveDate, time: NaiveTime, tz: &Tz) -> DateTime<Utc> {
    let mut naive = date.and_time(time);
    // A gap is at most 24 h long in any zone; step one minute at a time.
    for _ in 0..=24 * 60 {
        match tz.from_local_datetime(&naive) {
            LocalResult::Single(at) => return at.with_timezone(&Utc),
            LocalResult::Ambiguous(first, second) => {
                return first.min(second).with_timezone(&Utc);
            }
            LocalResult::None => naive += TimeDelta::minutes(1),
        }
    }
    naive.and_utc()
}

fn runs_on(repeat: Repeat, date: NaiveDate) -> bool {
    match repeat {
        Repeat::Daily => true,
        Repeat::Weekdays => date.weekday().number_from_monday() <= 5,
    }
}

/// The first occurrence of `time` on a day `repeat` runs, strictly after `after`.
pub fn next_occurrence<Tz: TimeZone>(
    repeat: Repeat,
    time: NaiveTime,
    after: DateTime<Utc>,
    tz: &Tz,
) -> DateTime<Utc> {
    let start = after.with_timezone(tz).date_naive();
    for offset in 0..=SEARCH_DAYS {
        let date = start + TimeDelta::days(offset);
        if !runs_on(repeat, date) {
            continue;
        }
        let candidate = local_to_utc(date, time, tz);
        if candidate > after {
            return candidate;
        }
    }
    after + TimeDelta::days(1)
}

/// The latest occurrence of `time` on a day `repeat` runs, at or before
/// `at_or_before`.
pub fn latest_occurrence<Tz: TimeZone>(
    repeat: Repeat,
    time: NaiveTime,
    at_or_before: DateTime<Utc>,
    tz: &Tz,
) -> DateTime<Utc> {
    let start = at_or_before.with_timezone(tz).date_naive();
    for offset in 0..=SEARCH_DAYS {
        let date = start - TimeDelta::days(offset);
        if !runs_on(repeat, date) {
            continue;
        }
        let candidate = local_to_utc(date, time, tz);
        if candidate <= at_or_before {
            return candidate;
        }
    }
    at_or_before
}

/// Resolves the `at` argument. `HH:MM` is the next occurrence after `now` and
/// also returns the time of day, for `repeat`. `YYYY-MM-DD HH:MM` must be after
/// `now` and at most [`MAX_AHEAD_DAYS`] ahead.
pub fn resolve_at<Tz: TimeZone>(
    at: &str,
    now: DateTime<Utc>,
    tz: &Tz,
) -> Result<(DateTime<Utc>, Option<NaiveTime>), String> {
    let at = at.trim();
    if at.len() == 5
        && let Ok(time) = NaiveTime::parse_from_str(at, "%H:%M")
    {
        return Ok((next_occurrence(Repeat::Daily, time, now, tz), Some(time)));
    }
    let Ok(naive) = NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M") else {
        return Err(format!(
            "at must be HH:MM or YYYY-MM-DD HH:MM in the host's local time zone, got {at:?}"
        ));
    };
    let due = local_to_utc(naive.date(), naive.time(), tz);
    if due <= now {
        return Err("at must be after the current time".into());
    }
    if due > now + TimeDelta::days(MAX_AHEAD_DAYS) {
        return Err(format!("at must be at most {MAX_AHEAD_DAYS} days ahead"));
    }
    Ok((due, None))
}

/// `2026-10-05 08:00 +08:00`, in the host's local time zone.
pub fn format_local(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M %:z")
        .to_string()
}

/// `14h40m`, `2h3m`, `5m10s`, `45s`; negative values print as `0s`.
pub fn format_duration(delta: TimeDelta) -> String {
    let total = (delta.num_milliseconds().max(0) + 500) / 1000;
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 && seconds > 0 {
        format!("{minutes}m{seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(8 * 3600).unwrap()
    }

    /// A local (+08:00) wall-clock instant.
    fn at(text: &str) -> DateTime<Utc> {
        let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
        local_to_utc(naive.date(), naive.time(), &tz())
    }

    fn t(text: &str) -> NaiveTime {
        NaiveTime::parse_from_str(text, "%H:%M").unwrap()
    }

    #[test]
    fn hh_mm_is_the_next_occurrence_strictly_after_now() {
        let now = at("2026-10-04 17:20");
        let (due, time) = resolve_at("18:00", now, &tz()).unwrap();
        assert_eq!(due, at("2026-10-04 18:00"));
        assert_eq!(time, Some(t("18:00")));
        assert_eq!(
            resolve_at("08:00", now, &tz()).unwrap().0,
            at("2026-10-05 08:00")
        );
        // The current minute is not "after now".
        assert_eq!(
            resolve_at("17:20", now, &tz()).unwrap().0,
            at("2026-10-05 17:20")
        );
    }

    #[test]
    fn dated_at_must_be_in_range() {
        let now = at("2026-10-04 17:20");
        let (due, time) = resolve_at("2026-10-06 09:30", now, &tz()).unwrap();
        assert_eq!((due, time), (at("2026-10-06 09:30"), None));
        assert!(resolve_at("2026-10-04 17:20", now, &tz()).is_err());
        assert!(resolve_at("2026-10-03 09:00", now, &tz()).is_err());
        assert!(resolve_at("2026-11-03 17:21", now, &tz()).is_err());
        assert!(resolve_at("2026-11-03 17:20", now, &tz()).is_ok());
        for bad in [
            "",
            "tomorrow",
            "25:00",
            "2026-10-05",
            "8am",
            "2026-10-05T09:00",
        ] {
            assert!(resolve_at(bad, now, &tz()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn daily_and_weekdays_advance() {
        // 2026-10-02 is a Friday.
        let friday = at("2026-10-02 08:30");
        assert_eq!(
            next_occurrence(Repeat::Daily, t("08:00"), friday, &tz()),
            at("2026-10-03 08:00")
        );
        assert_eq!(
            next_occurrence(Repeat::Weekdays, t("08:00"), friday, &tz()),
            at("2026-10-05 08:00"),
            "Friday to Monday"
        );
        assert_eq!(
            next_occurrence(Repeat::Weekdays, t("09:00"), friday, &tz()),
            at("2026-10-02 09:00")
        );
        let saturday = at("2026-10-03 12:00");
        assert_eq!(
            next_occurrence(Repeat::Weekdays, t("08:00"), saturday, &tz()),
            at("2026-10-05 08:00")
        );
    }

    #[test]
    fn latest_occurrence_is_at_or_before() {
        let monday_noon = at("2026-10-05 12:00");
        assert_eq!(
            latest_occurrence(Repeat::Daily, t("08:00"), monday_noon, &tz()),
            at("2026-10-05 08:00")
        );
        assert_eq!(
            latest_occurrence(Repeat::Daily, t("08:00"), at("2026-10-05 08:00"), &tz()),
            at("2026-10-05 08:00")
        );
        assert_eq!(
            latest_occurrence(Repeat::Daily, t("20:00"), monday_noon, &tz()),
            at("2026-10-04 20:00")
        );
        assert_eq!(
            latest_occurrence(Repeat::Weekdays, t("08:00"), at("2026-10-04 12:00"), &tz()),
            at("2026-10-02 08:00"),
            "Sunday catches up to Friday"
        );
    }

    #[test]
    fn a_dst_gap_moves_forward_and_an_overlap_uses_the_earlier_instant() {
        use chrono::offset::LocalResult;
        // A zone whose 02:30 does not exist on one day and 01:30 exists twice
        // is not available as a fixed offset; use the gap helper directly with
        // a stub zone.
        #[derive(Clone, Copy, Debug)]
        struct Stub;
        #[derive(Clone, Copy, Debug)]
        struct StubOffset(i32);
        impl chrono::Offset for StubOffset {
            fn fix(&self) -> FixedOffset {
                FixedOffset::east_opt(self.0).unwrap()
            }
        }
        impl std::fmt::Display for StubOffset {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl TimeZone for Stub {
            type Offset = StubOffset;
            fn from_offset(_: &StubOffset) -> Self {
                Stub
            }
            fn offset_from_local_date(&self, _: &NaiveDate) -> LocalResult<StubOffset> {
                LocalResult::Single(StubOffset(0))
            }
            fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<StubOffset> {
                let gap = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
                let overlap = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
                let hour = local.time().format("%H").to_string();
                if local.date() == gap && hour == "02" {
                    LocalResult::None
                } else if local.date() == overlap && hour == "01" {
                    LocalResult::Ambiguous(StubOffset(-4 * 3600), StubOffset(-5 * 3600))
                } else {
                    LocalResult::Single(StubOffset(-5 * 3600))
                }
            }
            fn offset_from_utc_date(&self, _: &NaiveDate) -> StubOffset {
                StubOffset(-5 * 3600)
            }
            fn offset_from_utc_datetime(&self, _: &NaiveDateTime) -> StubOffset {
                StubOffset(-5 * 3600)
            }
        }
        let gap = local_to_utc(
            NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(),
            t("02:30"),
            &Stub,
        );
        assert_eq!(
            gap,
            Utc.with_ymd_and_hms(2026, 3, 8, 8, 0, 0).unwrap(),
            "03:00 -05:00"
        );
        let overlap = local_to_utc(
            NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
            t("01:30"),
            &Stub,
        );
        assert_eq!(
            overlap,
            Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0).unwrap(),
            "earlier instant"
        );
    }

    #[test]
    fn durations_use_the_short_form() {
        let secs = TimeDelta::seconds;
        assert_eq!(format_duration(secs(14 * 3600 + 40 * 60)), "14h40m");
        assert_eq!(format_duration(secs(2 * 3600 + 3 * 60 + 20)), "2h3m");
        assert_eq!(format_duration(secs(310)), "5m10s");
        assert_eq!(format_duration(secs(300)), "5m");
        assert_eq!(format_duration(secs(45)), "45s");
        assert_eq!(format_duration(secs(-5)), "0s");
    }
}
