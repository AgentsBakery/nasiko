//! The only definition of the Redis spend-counter key and the micro-USD unit.
//!
//! Counters are integers of micro-USD (`1e-6` USD) so `INCRBY` never touches
//! floats. The server and the router both go through these helpers; no other
//! code may spell the key format.

use chrono::{DateTime, Utc};
use uuid::Uuid;

const MICROS_PER_USD: f64 = 1_000_000.0;

/// Counter key for one budget's spend in the period starting at `period_start`.
pub fn spend_key(budget_id: Uuid, period_start: DateTime<Utc>) -> String {
    format!(
        "nasiko:budget:spend:{budget_id}:{}",
        period_start.timestamp()
    )
}

pub fn usd_to_micros(usd: f64) -> i64 {
    (usd * MICROS_PER_USD).round() as i64
}

pub fn micros_to_usd(micros: i64) -> f64 {
    micros as f64 / MICROS_PER_USD
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn key_format_is_stable() {
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let start = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
        assert_eq!(
            spend_key(id, start),
            "nasiko:budget:spend:11111111-2222-3333-4444-555555555555:1772323200"
        );
    }

    #[test]
    fn micro_conversions() {
        assert_eq!(usd_to_micros(0.000008), 8);
        assert_eq!(usd_to_micros(1.5), 1_500_000);
        assert_eq!(micros_to_usd(2_000_000), 2.0);
    }
}
