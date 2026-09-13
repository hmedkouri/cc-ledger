//! Local-time bucketing for UTC-stored timestamps.
//!
//! Rows are stored as UTC Unix seconds and bucketed into days, weeks and months
//! in **local** time, because "what did I spend today" means the local day.
//!
//! Everything here delegates to `chrono`'s timezone arithmetic rather than
//! dividing epoch seconds by 86 400. That distinction is the whole point: on a
//! DST transition a local day is 23 or 25 hours long, so epoch division silently
//! files a request under the wrong day twice a year.
//!
//! Functions are generic over the timezone so the tests can pin one; the
//! binaries pass `chrono::Local`.

use chrono::{Datelike, Duration, NaiveDate, TimeZone};

/// Local midnight of the day containing `ts`, as UTC seconds.
pub fn day_start<Tz: TimeZone>(ts: i64, tz: &Tz) -> i64 {
    start_of_day(local_date(ts, tz), tz)
}

/// Local midnight of the Monday of the week containing `ts`.
pub fn week_start<Tz: TimeZone>(ts: i64, tz: &Tz) -> i64 {
    let date = local_date(ts, tz);
    let back = date.weekday().num_days_from_monday() as i64;
    start_of_day(date - Duration::days(back), tz)
}

/// Local midnight of the first day of the month containing `ts`.
pub fn month_start<Tz: TimeZone>(ts: i64, tz: &Tz) -> i64 {
    let date = local_date(ts, tz);
    start_of_day(date.with_day(1).unwrap_or(date), tz)
}

pub fn day_label<Tz: TimeZone>(ts: i64, tz: &Tz) -> String {
    local_date(ts, tz).format("%Y-%m-%d").to_string()
}

pub fn week_label<Tz: TimeZone>(ts: i64, tz: &Tz) -> String {
    local_date(week_start(ts, tz), tz)
        .format("%Y-%m-%d")
        .to_string()
}

pub fn month_label<Tz: TimeZone>(ts: i64, tz: &Tz) -> String {
    local_date(ts, tz).format("%Y-%m").to_string()
}

/// Parse `YYYY-MM-DD` as local midnight, for `--since` / `--until`.
pub fn parse_date<Tz: TimeZone>(text: &str, tz: &Tz) -> Option<i64> {
    let date = NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()?;
    Some(start_of_day(date, tz))
}

fn local_date<Tz: TimeZone>(ts: i64, tz: &Tz) -> NaiveDate {
    tz.timestamp_opt(ts, 0)
        .earliest()
        .map(|dt| dt.date_naive())
        .unwrap_or_else(|| {
            // Unrepresentable instants should not exist, but a ledger row is
            // never worth a panic in a status line.
            chrono::DateTime::from_timestamp(ts, 0)
                .unwrap_or_default()
                .date_naive()
        })
}

/// Midnight does not exist on every local day: zones that spring forward at
/// 00:00 (Santiago, parts of Brazil) skip straight to 01:00. Walk forward to
/// the first hour that does exist rather than unwrapping a `None`.
fn start_of_day<Tz: TimeZone>(date: NaiveDate, tz: &Tz) -> i64 {
    for hour in 0..6 {
        if let Some(dt) = date
            .and_hms_opt(hour, 0, 0)
            .and_then(|naive| tz.from_local_datetime(&naive).earliest())
        {
            return dt.timestamp();
        }
    }
    date.and_hms_opt(0, 0, 0)
        .map(|n| n.and_utc().timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use chrono::Utc;
    use chrono_tz::America::New_York;
    use chrono_tz::Pacific::Auckland;

    /// 2026-09-05T13:56:32Z
    const TS: i64 = 1_788_616_592;

    #[test]
    fn buckets_by_local_day_not_utc_day() {
        // 2026-09-05T23:30:00Z is still the 5th in UTC, but already the 6th in
        // Auckland (+12) and still the 5th in New York (-4).
        let late = chrono::DateTime::parse_from_rfc3339("2026-09-05T23:30:00Z")
            .unwrap()
            .timestamp();

        assert_eq!(day_label(late, &Utc), "2026-09-05");
        assert_eq!(day_label(late, &Auckland), "2026-09-06");
        assert_eq!(day_label(late, &New_York), "2026-09-05");
    }

    #[test]
    fn day_start_is_local_midnight() {
        let start = day_start(TS, &New_York);
        let as_local = New_York.timestamp_opt(start, 0).unwrap();
        assert_eq!(as_local.hour_minute_second(), (0, 0, 0));
        assert!(start <= TS);
        assert!(TS - start < 24 * 3600);
    }

    trait HourMinuteSecond {
        fn hour_minute_second(&self) -> (u32, u32, u32);
    }
    impl<Tz: TimeZone> HourMinuteSecond for chrono::DateTime<Tz> {
        fn hour_minute_second(&self) -> (u32, u32, u32) {
            use chrono::Timelike;
            (self.hour(), self.minute(), self.second())
        }
    }

    /// The bug this module exists to prevent: a 23-hour local day.
    /// US DST springs forward 2026-03-08 at 02:00 America/New_York.
    #[test]
    fn spring_forward_day_is_twenty_three_hours() {
        let during = New_York
            .with_ymd_and_hms(2026, 3, 8, 12, 0, 0)
            .unwrap()
            .timestamp();
        let next = New_York
            .with_ymd_and_hms(2026, 3, 9, 12, 0, 0)
            .unwrap()
            .timestamp();

        let start = day_start(during, &New_York);
        let next_start = day_start(next, &New_York);

        assert_eq!(next_start - start, 23 * 3600, "23-hour day");
        assert_eq!(day_label(during, &New_York), "2026-03-08");
        // Epoch division would have put this in the wrong bucket.
        assert_ne!(next_start - start, 24 * 3600);
    }

    /// And a 25-hour local day when the clocks go back, 2026-11-01.
    #[test]
    fn fall_back_day_is_twenty_five_hours() {
        let during = New_York
            .with_ymd_and_hms(2026, 11, 1, 12, 0, 0)
            .unwrap()
            .timestamp();
        let next = New_York
            .with_ymd_and_hms(2026, 11, 2, 12, 0, 0)
            .unwrap()
            .timestamp();

        let start = day_start(during, &New_York);
        let next_start = day_start(next, &New_York);
        assert_eq!(next_start - start, 25 * 3600, "25-hour day");
    }

    /// An instant one second before local midnight must not leak into the next
    /// day's bucket, DST or not.
    #[test]
    fn instants_land_in_exactly_one_day_bucket() {
        let midnight = New_York
            .with_ymd_and_hms(2026, 3, 8, 0, 0, 0)
            .unwrap()
            .timestamp();

        assert_eq!(day_start(midnight, &New_York), midnight);
        assert_eq!(day_label(midnight - 1, &New_York), "2026-03-07");
        assert_eq!(day_label(midnight, &New_York), "2026-03-08");
    }

    #[test]
    fn weeks_start_on_monday() {
        // 2026-09-05 is a Saturday; its week starts Monday 2026-08-31.
        assert_eq!(week_label(TS, &Utc), "2026-08-31");

        let monday = Utc.with_ymd_and_hms(2026, 8, 31, 0, 0, 0).unwrap();
        assert_eq!(week_start(TS, &Utc), monday.timestamp());
    }

    #[test]
    fn week_start_is_stable_across_a_dst_transition() {
        // 2026-03-08 (spring forward) is a Sunday, so its week began Monday
        // 2026-03-02 — six days and one lost hour earlier.
        let sunday = New_York
            .with_ymd_and_hms(2026, 3, 8, 12, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(week_label(sunday, &New_York), "2026-03-02");

        let start = week_start(sunday, &New_York);
        assert_eq!(sunday - start, 6 * 24 * 3600 + 12 * 3600 - 3600);
    }

    #[test]
    fn months_bucket_by_local_month() {
        assert_eq!(month_label(TS, &Utc), "2026-09");

        let start = month_start(TS, &Utc);
        assert_eq!(
            start,
            Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0)
                .unwrap()
                .timestamp()
        );

        // 2026-09-01T02:00:00Z is still August in New York.
        let edge = chrono::DateTime::parse_from_rfc3339("2026-09-01T02:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(month_label(edge, &New_York), "2026-08");
    }

    #[test]
    fn parse_date_reads_local_midnight() {
        let ts = parse_date("2026-09-05", &Utc).unwrap();
        assert_eq!(
            ts,
            Utc.with_ymd_and_hms(2026, 9, 5, 0, 0, 0)
                .unwrap()
                .timestamp()
        );

        // Same calendar date is a different instant in a different zone.
        let nyc = parse_date("2026-09-05", &New_York).unwrap();
        assert_eq!(nyc - ts, 4 * 3600);

        assert_eq!(parse_date("  2026-09-05  ", &Utc), Some(ts));
        assert_eq!(parse_date("nonsense", &Utc), None);
        assert_eq!(parse_date("2026-13-01", &Utc), None);
    }

    #[test]
    fn works_in_extreme_offsets() {
        let plus = FixedOffset::east_opt(13 * 3600 + 45 * 60).unwrap();
        let minus = FixedOffset::west_opt(11 * 3600).unwrap();

        assert_eq!(day_label(TS, &plus), "2026-09-06");
        assert_eq!(day_label(TS, &minus), "2026-09-05");
        assert!(day_start(TS, &plus) <= TS);
        assert!(day_start(TS, &minus) <= TS);
    }
}
