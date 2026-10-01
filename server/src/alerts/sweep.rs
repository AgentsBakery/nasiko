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

/// Finished outbox rows (delivered or failed) are kept this long as delivery history.
const OUTBOX_RETENTION_DAYS: i32 = 30;
/// Rows deleted per purge step, so a large backlog never holds one long delete.
const PURGE_BATCH: i64 = 1000;
/// `llm_call_failures` rows are kept this long.
const FAILURE_RETENTION_DAYS: i32 = 30;

/// Run every sweep step; returns the number of alerts resolved.
pub async fn tick_resolve_sweep(db: &PgPool, now: DateTime<Utc>) -> Result<usize, sqlx::Error> {
    let mut tx = db.begin().await?;
    let mut resolved = 0;
    resolved += resolve_budget_alerts(&mut tx, now).await?;
    resolved += resolve_orphaned_monitor_alerts(&mut tx).await?;
    purge_old_outbox(&mut tx).await?;
    purge_old_failures(&mut tx).await?;
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

/// A monitor alert has no condition left to track once its monitor is disabled
/// or deleted (alerts keep no FK to monitors).
async fn resolve_orphaned_monitor_alerts(conn: &mut PgConnection) -> Result<usize, sqlx::Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE alerts SET status = 'resolved', resolved_at = now() \
         WHERE kind = 'monitor_breach' AND status <> 'resolved' \
           AND NOT EXISTS (SELECT 1 FROM monitors m \
                           WHERE m.id = (details->>'monitor_id')::uuid AND m.enabled) \
         RETURNING id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for id in &ids {
        engine::enqueue(conn, *id, NotifyEvent::Resolved).await?;
    }
    Ok(ids.len())
}

/// Failure-record retention: `llm_call_failures` only feeds recent-window monitors.
async fn purge_old_failures(conn: &mut PgConnection) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "DELETE FROM llm_call_failures WHERE id IN ( \
             SELECT id FROM llm_call_failures \
             WHERE created_at < now() - $1::int * interval '1 day' \
             LIMIT $2)",
    )
    .bind(FAILURE_RETENTION_DAYS)
    .bind(PURGE_BATCH)
    .execute(conn)
    .await?;
    Ok(res.rows_affected())
}

/// Outbox retention: drop finished rows past `OUTBOX_RETENTION_DAYS`. Pending
/// and sending rows are never touched, however old.
async fn purge_old_outbox(conn: &mut PgConnection) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "DELETE FROM notification_outbox WHERE id IN ( \
             SELECT id FROM notification_outbox \
             WHERE status IN ('delivered', 'failed') \
               AND created_at < now() - $1::int * interval '1 day' \
             LIMIT $2)",
    )
    .bind(OUTBOX_RETENTION_DAYS)
    .bind(PURGE_BATCH)
    .execute(conn)
    .await?;
    Ok(res.rows_affected())
}
