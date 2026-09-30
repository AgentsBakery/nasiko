//! UTC-aligned budget periods. Pure: no clock, no I/O.

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};

const SECS_PER_DAY: u64 = 86_400;
/// Counters outlive their period by this much so late reconciliation and
/// status reads of a just-closed period still find the key.
const COUNTER_GRACE_DAYS: u64 = 2;

/// Budget reset cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// 00:00 UTC each day.
    Daily,
    /// Monday 00:00 UTC (ISO week).
    Weekly,
    /// The 1st at 00:00 UTC.
    Monthly,
}

impl Period {
    pub fn as_str(self) -> &'static str {
        match self {
            Period::Daily => "daily",
            Period::Weekly => "weekly",
            Period::Monthly => "monthly",
        }
    }

    pub fn parse(s: &str) -> Option<Period> {
        match s {
            "daily" => Some(Period::Daily),
            "weekly" => Some(Period::Weekly),
            "monthly" => Some(Period::Monthly),
            _ => None,
        }
    }
}

fn midnight(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0)
        .single()
        .expect("midnight UTC of a valid calendar date is unambiguous")
}

/// The half-open `[start, end)` period containing `now`.
pub fn period_bounds(period: Period, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let day = midnight(now.year(), now.month(), now.day());
    match period {
        Period::Daily => (day, day + Duration::days(1)),
        Period::Weekly => {
            let start = day - Duration::days(i64::from(now.weekday().num_days_from_monday()));
            (start, start + Duration::days(7))
        }
        Period::Monthly => {
            let start = midnight(now.year(), now.month(), 1);
            let (ny, nm) = if now.month() == 12 {
                (now.year() + 1, 1)
            } else {
                (now.year(), now.month() + 1)
            };
            (start, midnight(ny, nm, 1))
        }
    }
}

/// Redis TTL for a period's counter: the period length plus two days.
pub fn counter_ttl_secs(period: Period, start: DateTime<Utc>) -> u64 {
    let (_, end) = period_bounds(period, start);
    let len = u64::try_from((end - start).num_seconds()).unwrap_or(0);
    len + COUNTER_GRACE_DAYS * SECS_PER_DAY
}

/// Fraction of the period elapsed at `now`, in `[0, 1)`.
pub fn elapsed_fraction(period: Period, now: DateTime<Utc>) -> f64 {
    let (start, end) = period_bounds(period, now);
    (now - start).num_milliseconds() as f64 / (end - start).num_milliseconds() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn daily_bounds() {
        let (s, e) = period_bounds(Period::Daily, at(2026, 3, 15, 13, 45));
        assert_eq!((s, e), (at(2026, 3, 15, 0, 0), at(2026, 3, 16, 0, 0)));
        let (s, _) = period_bounds(Period::Daily, at(2026, 3, 15, 0, 0));
        assert_eq!(s, at(2026, 3, 15, 0, 0));
    }

    #[test]
    fn weekly_bounds_start_on_monday() {
        let (s, e) = period_bounds(Period::Weekly, at(2026, 3, 15, 23, 59));
        assert_eq!((s, e), (at(2026, 3, 9, 0, 0), at(2026, 3, 16, 0, 0)));
        let (s, e) = period_bounds(Period::Weekly, at(2026, 3, 16, 0, 0));
        assert_eq!((s, e), (at(2026, 3, 16, 0, 0), at(2026, 3, 23, 0, 0)));
    }

    #[test]
    fn monthly_bounds_cross_year_and_leap_day() {
        let (s, e) = period_bounds(Period::Monthly, at(2026, 12, 31, 23, 0));
        assert_eq!((s, e), (at(2026, 12, 1, 0, 0), at(2027, 1, 1, 0, 0)));
        let (s, e) = period_bounds(Period::Monthly, at(2028, 2, 29, 12, 0));
        assert_eq!((s, e), (at(2028, 2, 1, 0, 0), at(2028, 3, 1, 0, 0)));
    }

    #[test]
    fn counter_ttl_is_period_plus_two_days() {
        assert_eq!(
            counter_ttl_secs(Period::Daily, at(2026, 3, 15, 0, 0)),
            3 * 86_400
        );
        assert_eq!(
            counter_ttl_secs(Period::Monthly, at(2027, 2, 1, 0, 0)),
            30 * 86_400
        );
    }

    #[test]
    fn elapsed_fraction_midday() {
        let f = elapsed_fraction(Period::Daily, at(2026, 3, 15, 12, 0));
        assert!((f - 0.5).abs() < 1e-12);
    }

    #[test]
    fn period_round_trips_through_str() {
        for p in [Period::Daily, Period::Weekly, Period::Monthly] {
            assert_eq!(Period::parse(p.as_str()), Some(p));
        }
        assert_eq!(Period::parse("yearly"), None);
    }
}
