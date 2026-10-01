//! Hourly spend-spike detector.
//!
//! Data source is the `token_usage` rows the LLM router writes
//! (`operation_type IN ('direct_llm','embedding')`, the same set budgets
//! meter). The TokenOps chart reads `trace_usage`, so the numbers can differ
//! slightly; the alert is about router-metered spend.
//!
//! For the latest complete hour the detector compares each scope's spend (the
//! platform and every agent) with the mean + N sample standard deviations of
//! the previous 168 hours. A sustained spike raises one alert per hour by
//! design (the dedup key carries the hour); each resolves on the first quiet
//! hour or after 24 hours.
//!
//! Only one replica pays for the baseline scan: a transaction-scoped advisory
//! try-lock guards the whole tick (never a session lock, which a pooled
//! connection would leak).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::engine;
use super::models::{AlertKind, AlertScope, NewAlert, Severity, tokenops_link};

/// Hours of history the baseline is computed from.
pub const BASELINE_HOURS: usize = 168;
/// A scope needs this many non-zero baseline hours before it can spike
/// (cold-start guard: a brand-new agent has no meaningful baseline).
pub const MIN_NONZERO_HOURS: usize = 24;
/// Above this multiple of the threshold a spike is critical instead of warning.
const CRITICAL_MULTIPLE: f64 = 2.0;
const SECS_PER_HOUR: i64 = 3600;

/// Detector knobs, from `AlertsConfig`.
#[derive(Debug, Clone, Copy)]
pub struct SpikeSettings {
    pub sigma: f64,
    pub floor_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Baseline {
    pub mean: f64,
    pub std: f64,
    pub nonzero_hours: usize,
}

// ─── pure math ───────────────────────────────────────────────────────────────

/// Mean and sample standard deviation (n-1) of the hourly values.
pub fn baseline(hours: &[f64]) -> Baseline {
    let n = hours.len();
    let nonzero_hours = hours.iter().filter(|v| **v > 0.0).count();
    if n == 0 {
        return Baseline {
            mean: 0.0,
            std: 0.0,
            nonzero_hours,
        };
    }
    let mean = hours.iter().sum::<f64>() / n as f64;
    let std = if n > 1 {
        let ss: f64 = hours.iter().map(|v| (v - mean).powi(2)).sum();
        (ss / (n - 1) as f64).sqrt()
    } else {
        0.0
    };
    Baseline {
        mean,
        std,
        nonzero_hours,
    }
}

pub fn threshold(b: &Baseline, sigma: f64) -> f64 {
    b.mean + sigma * b.std
}

pub fn is_spike(latest: f64, b: &Baseline, sigma: f64, floor_usd: f64) -> bool {
    b.nonzero_hours >= MIN_NONZERO_HOURS && latest >= floor_usd && latest > threshold(b, sigma)
}

pub fn severity_for(latest: f64, threshold: f64) -> Severity {
    if latest > CRITICAL_MULTIPLE * threshold {
        Severity::Critical
    } else {
        Severity::Warning
    }
}

/// Start of the latest complete hour relative to `now`.
pub fn latest_complete_hour(now: DateTime<Utc>) -> DateTime<Utc> {
    let secs = now.timestamp();
    let floored = secs - secs.rem_euclid(SECS_PER_HOUR) - SECS_PER_HOUR;
    DateTime::from_timestamp(floored, 0).expect("hour boundary is a valid timestamp")
}

// ─── tick ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Scope {
    Platform,
    Agent(Uuid),
}

struct OpenSpike {
    dedup_key: String,
    hour_start: DateTime<Utc>,
}

fn hour_label(h: DateTime<Utc>) -> String {
    h.format("%Y-%m-%dT%H:00Z").to_string()
}

/// Evaluate the latest complete hour. Returns the number of alerts raised
/// (0 when another replica holds the lock).
pub async fn tick_spike(
    db: &PgPool,
    settings: &SpikeSettings,
    now: DateTime<Utc>,
) -> Result<usize, sqlx::Error> {
    let mut tx = db.begin().await?;
    let locked: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtext('alerts:spike'))")
            .fetch_one(&mut *tx)
            .await?;
    if !locked {
        tx.rollback().await?;
        return Ok(0);
    }

    let latest = latest_complete_hour(now);
    let latest_idx = latest.timestamp() / SECS_PER_HOUR;
    let first_idx = latest_idx - BASELINE_HOURS as i64;

    let rows: Vec<(i64, Option<Uuid>, f64)> = sqlx::query_as(
        "SELECT floor(extract(epoch FROM created_at) / 3600)::bigint AS h, agent_id, \
                COALESCE(SUM(cost_usd), 0)::float8 \
         FROM token_usage \
         WHERE operation_type IN ('direct_llm','embedding') \
           AND created_at >= $1 - interval '168 hours' AND created_at < $1 + interval '1 hour' \
         GROUP BY 1, 2",
    )
    .bind(latest)
    .fetch_all(&mut *tx)
    .await?;

    // 168 baseline slots followed by the latest hour.
    let mut series: HashMap<Scope, Vec<f64>> = HashMap::new();
    series.insert(Scope::Platform, vec![0.0; BASELINE_HOURS + 1]);
    for (hour_idx, agent, cost) in rows {
        let Ok(slot) = usize::try_from(hour_idx - first_idx) else {
            continue;
        };
        if slot > BASELINE_HOURS {
            continue;
        }
        series.entry(Scope::Platform).or_default()[slot] += cost;
        if let Some(agent) = agent {
            series
                .entry(Scope::Agent(agent))
                .or_insert_with(|| vec![0.0; BASELINE_HOURS + 1])[slot] += cost;
        }
    }

    // Resolution is driven from open alerts: a scope with an open spike alert
    // is evaluated even when it has no rows at all (silent hour = $0).
    let open_rows: Vec<(String, String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT dedup_key, scope, scope_ref, (details->>'hour_start')::timestamptz \
         FROM alerts WHERE kind = 'spend_spike' AND status <> 'resolved' \
           AND details->>'hour_start' IS NOT NULL",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut open: HashMap<Scope, Vec<OpenSpike>> = HashMap::new();
    for (dedup_key, scope, scope_ref, hour_start) in open_rows {
        let scope = match (scope.as_str(), scope_ref.as_deref().map(Uuid::parse_str)) {
            ("platform", _) => Scope::Platform,
            ("agent", Some(Ok(id))) => Scope::Agent(id),
            _ => continue,
        };
        series
            .entry(scope)
            .or_insert_with(|| vec![0.0; BASELINE_HOURS + 1]);
        open.entry(scope).or_default().push(OpenSpike {
            dedup_key,
            hour_start,
        });
    }

    struct Spiking {
        scope: Scope,
        spend: f64,
        threshold: f64,
        base: Baseline,
    }
    let mut spiking = Vec::new();
    let mut quiet: HashSet<Scope> = HashSet::new();
    for (scope, slots) in &series {
        let base = baseline(&slots[..BASELINE_HOURS]);
        let spend = slots[BASELINE_HOURS];
        if is_spike(spend, &base, settings.sigma, settings.floor_usd) {
            spiking.push(Spiking {
                scope: *scope,
                spend,
                threshold: threshold(&base, settings.sigma),
                base,
            });
        } else {
            quiet.insert(*scope);
        }
    }

    let agent_ids: Vec<Uuid> = spiking
        .iter()
        .filter_map(|s| match s.scope {
            Scope::Agent(id) => Some(id),
            Scope::Platform => None,
        })
        .collect();
    let names: HashMap<Uuid, String> = if agent_ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, name FROM agents WHERE id = ANY($1)")
            .bind(&agent_ids)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect()
    };

    let label = hour_label(latest);
    for s in &spiking {
        let (scope, scope_ref, agent, dedup_key, who) = match s.scope {
            Scope::Platform => (
                AlertScope::Platform,
                None,
                None,
                format!("spike:platform:all:{label}"),
                "platform".to_owned(),
            ),
            Scope::Agent(id) => (
                AlertScope::Agent,
                Some(id.to_string()),
                Some(id),
                format!("spike:agent:{id}:{label}"),
                names.get(&id).cloned().unwrap_or_else(|| id.to_string()),
            ),
        };
        let new = NewAlert {
            kind: AlertKind::SpendSpike,
            severity: severity_for(s.spend, s.threshold),
            scope,
            scope_ref,
            dedup_key,
            title: format!("Spend spike: {who}"),
            message: format!(
                "{who} spent ${:.2} in the hour starting {label}, above the ${:.2} threshold \
                 (baseline mean ${:.2}, std ${:.2} over the previous {BASELINE_HOURS} hours).",
                s.spend, s.threshold, s.base.mean, s.base.std
            ),
            link: tokenops_link(agent, Some("24h")),
            details: json!({
                "hour_start": latest.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "spend_usd": s.spend,
                "threshold_usd": s.threshold,
                "baseline_mean": s.base.mean,
                "baseline_std": s.base.std,
            }),
        };
        engine::raise(&mut tx, &new).await?;
    }

    for scope in &quiet {
        for alert in open.get(scope).into_iter().flatten() {
            if alert.hour_start < latest {
                engine::resolve(&mut tx, &alert.dedup_key).await?;
            }
        }
    }

    tx.commit().await?;
    Ok(spiking.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_of_zeros() {
        let b = baseline(&[0.0; BASELINE_HOURS]);
        assert_eq!((b.mean, b.std, b.nonzero_hours), (0.0, 0.0, 0));
    }

    #[test]
    fn baseline_uses_sample_stddev() {
        let mut hours = vec![1.0; 30];
        hours.extend(vec![0.0; BASELINE_HOURS - 30]);
        let b = baseline(&hours);
        let mean: f64 = 30.0 / 168.0;
        let ss: f64 = 30.0 * (1.0 - mean).powi(2) + 138.0 * mean.powi(2);
        let std = (ss / 167.0).sqrt();
        assert_eq!(b.nonzero_hours, 30);
        assert!((b.mean - mean).abs() < 1e-9);
        assert!((b.std - std).abs() < 1e-9);
    }

    #[test]
    fn is_spike_gates() {
        let warm = Baseline {
            mean: 1.0,
            std: 0.5,
            nonzero_hours: 30,
        };
        let cold = Baseline {
            nonzero_hours: 23,
            ..warm
        };
        assert!(!is_spike(100.0, &cold, 3.0, 5.0));
        assert!(!is_spike(4.0, &warm, 3.0, 5.0), "below floor");
        assert!(!is_spike(1.5, &warm, 1.0, 0.0), "at threshold is not above");
        assert!(is_spike(6.0, &warm, 3.0, 5.0));
    }

    #[test]
    fn severity_doubles_to_critical() {
        assert_eq!(severity_for(10.0, 5.0), Severity::Warning);
        assert_eq!(severity_for(10.1, 5.0), Severity::Critical);
    }

    #[test]
    fn latest_hour_is_previous_complete_hour() {
        let now: DateTime<Utc> = "2026-03-01T10:05:00Z".parse().unwrap();
        let want: DateTime<Utc> = "2026-03-01T09:00:00Z".parse().unwrap();
        assert_eq!(latest_complete_hour(now), want);
    }
}
