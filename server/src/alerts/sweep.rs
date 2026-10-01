//! Resolver sweep: closes alerts whose condition has ended.
//!
//! The sweep is a list of named steps run in one transaction so later plans
//! (outbox retention, monitors, spikes, failure retention) append a step
//! without restructuring it.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::engine;
use super::models::NotifyEvent;

/// Run every sweep step; returns the number of alerts resolved.
pub async fn tick_resolve_sweep(db: &PgPool, now: DateTime<Utc>) -> Result<usize, sqlx::Error> {
    let mut tx = db.begin().await?;
    let mut resolved = 0;
    resolved += resolve_budget_alerts(&mut tx, now).await?;
    tx.commit().await?;
    Ok(resolved)
}

/// Budget alerts end with their period, or when the budget is disabled or
/// deleted (alerts keep no FK to budgets, so a deleted budget simply has no
/// enabled row).
async fn resolve_budget_alerts(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
) -> Result<usize, sqlx::Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE alerts SET status = 'resolved', resolved_at = now() \
         WHERE kind IN ('budget_soft', 'budget_hard') AND status <> 'resolved' \
           AND ( (details->>'period_end')::timestamptz <= $1 \
                 OR NOT EXISTS (SELECT 1 FROM budgets b \
                                WHERE b.id = (details->>'budget_id')::uuid AND b.enabled) ) \
         RETURNING id",
    )
    .bind(now)
    .fetch_all(&mut *conn)
    .await?;
    for id in &ids {
        engine::enqueue(conn, *id, NotifyEvent::Resolved).await?;
    }
    Ok(ids.len())
}
