//! Turns `budget_events` rows into alerts.
//!
//! One transaction claims a batch with `FOR UPDATE OF e SKIP LOCKED`, raises
//! the alert and stamps `alerted_at` before committing, so replicas never
//! double-process an event and a crash reprocesses idempotently (the alert's
//! dedup key is the event's unique `(budget, period, kind)` tuple).

use chrono::{DateTime, Utc};
use nasiko_llm_router::budget::period::{Period, period_bounds};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::engine;
use super::models::{AlertKind, AlertScope, NewAlert, Severity};

const CLAIM_BATCH: i64 = 50;
const BUDGETS_LINK: &str = "/budgets";

#[derive(sqlx::FromRow)]
struct ClaimedEvent {
    id: Uuid,
    budget_id: Uuid,
    period_start: DateTime<Utc>,
    kind: String,
    spend_usd: f64,
    limit_usd: f64,
    name: String,
    scope: String,
    target_id: Option<Uuid>,
    period: String,
    enabled: bool,
}

/// Raise alerts for unprocessed budget events and stamp them. Events whose
/// period already ended (history from before alerting, or long downtime) and
/// events of disabled budgets are stamped without raising. Returns the number
/// of events that raised or bumped an alert.
pub async fn tick_budget_events(db: &PgPool, now: DateTime<Utc>) -> Result<usize, sqlx::Error> {
    let mut tx = db.begin().await?;
    let events: Vec<ClaimedEvent> = sqlx::query_as(
        "SELECT e.id, e.budget_id, e.period_start, e.kind, \
                e.spend_usd::float8 AS spend_usd, e.limit_usd::float8 AS limit_usd, \
                b.name, b.scope, b.target_id, b.period, b.enabled \
         FROM budget_events e JOIN budgets b ON b.id = e.budget_id \
         WHERE e.alerted_at IS NULL \
         ORDER BY e.created_at \
         FOR UPDATE OF e SKIP LOCKED \
         LIMIT $1",
    )
    .bind(CLAIM_BATCH)
    .fetch_all(&mut *tx)
    .await?;

    let mut raised = 0;
    for ev in &events {
        if let Some(alert) = alert_for(ev, now) {
            engine::raise(&mut tx, &alert).await?;
            raised += 1;
        }
        sqlx::query("UPDATE budget_events SET alerted_at = now() WHERE id = $1")
            .bind(ev.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(raised)
}

/// The alert for an event, or `None` when it must only be stamped.
fn alert_for(ev: &ClaimedEvent, now: DateTime<Utc>) -> Option<NewAlert> {
    if !ev.enabled {
        return None;
    }
    let period = Period::parse(&ev.period)?;
    let period_end = period_bounds(period, ev.period_start).1;
    if period_end <= now {
        return None;
    }
    let (kind, severity, suffix, title) = match ev.kind.as_str() {
        "soft_threshold" => (
            AlertKind::BudgetSoft,
            Severity::Warning,
            "soft",
            format!("Budget '{}' reached its soft threshold", ev.name),
        ),
        "hard_limit" => (
            AlertKind::BudgetHard,
            Severity::Critical,
            "hard",
            format!("Budget '{}' is exhausted", ev.name),
        ),
        _ => return None,
    };
    let scope = AlertScope::parse(&ev.scope)?;
    let message = format!(
        "Spend ${:.2} of ${:.2} ({} budget) in the period starting {}.",
        ev.spend_usd,
        ev.limit_usd,
        ev.period,
        ev.period_start.to_rfc3339()
    );
    Some(NewAlert {
        kind,
        severity,
        scope,
        scope_ref: ev.target_id.map(|t| t.to_string()),
        dedup_key: format!(
            "budget:{}:{}:{suffix}",
            ev.budget_id,
            ev.period_start.to_rfc3339()
        ),
        title,
        message,
        link: BUDGETS_LINK.to_owned(),
        // period_end is stored now: the budget's period is editable, so the
        // resolver must not recompute it later.
        details: json!({
            "budget_id": ev.budget_id,
            "period_start": ev.period_start.to_rfc3339(),
            "period_end": period_end.to_rfc3339(),
            "limit_usd": ev.limit_usd,
            "spend_usd": ev.spend_usd,
        }),
    })
}
