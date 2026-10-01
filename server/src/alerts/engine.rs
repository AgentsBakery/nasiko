//! Alert engine: `raise` / `resolve` with dedup, escalation and outbox enqueue.
//!
//! Every function runs on the caller's connection so the alert change and its
//! `notification_outbox` rows commit (or roll back) together; callers that need
//! atomicity pass `&mut *tx`. Dedup rests on the partial unique index
//! `uq_alerts_open_dedup`, never on application-side locking.

use std::sync::atomic::{AtomicBool, Ordering};

use sqlx::PgConnection;
use uuid::Uuid;

use super::models::{NewAlert, NotifyEvent, RaiseOutcome};

/// Test-only hook, never set by production code. When set, `raise` resolves
/// the open row for its dedup key between its two steps (once, then the flag
/// resets), which deterministically exercises the retry-after-concurrent-resolve
/// path.
#[doc(hidden)]
pub static RESOLVE_BETWEEN_RAISE_STEPS: AtomicBool = AtomicBool::new(false);

/// Step 1: open a new alert unless a live one exists for the key. Under
/// concurrency the insert waits on the other transaction's index entry, so it
/// never fails with a unique violation.
async fn try_open(conn: &mut PgConnection, new: &NewAlert) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO alerts (kind, severity, scope, scope_ref, dedup_key, status, title, message, link, details) \
         VALUES ($1, $2, $3, $4, $5, 'open', $6, $7, $8, $9) \
         ON CONFLICT (dedup_key) WHERE status <> 'resolved' DO NOTHING \
         RETURNING id",
    )
    .bind(new.kind.as_str())
    .bind(new.severity.as_str())
    .bind(new.scope.as_str())
    .bind(&new.scope_ref)
    .bind(&new.dedup_key)
    .bind(&new.title)
    .bind(&new.message)
    .bind(&new.link)
    .bind(&new.details)
    .fetch_optional(conn)
    .await
}

/// Step 2: bump the live row. `None` means it was resolved in the meantime.
async fn bump(
    conn: &mut PgConnection,
    new: &NewAlert,
) -> Result<Option<RaiseOutcome>, sqlx::Error> {
    let row: Option<(Uuid, String, String)> = sqlx::query_as(
        "WITH old AS ( \
             SELECT id, severity FROM alerts WHERE dedup_key = $1 AND status <> 'resolved' FOR UPDATE \
         ) \
         UPDATE alerts a \
         SET occurrences = a.occurrences + 1, last_seen_at = now(), details = $2, \
             severity = CASE WHEN array_position(ARRAY['info','warning','critical'], $3::text) \
                               > array_position(ARRAY['info','warning','critical'], a.severity) \
                             THEN $3::text ELSE a.severity END \
         FROM old WHERE a.id = old.id \
         RETURNING a.id, old.severity, a.severity",
    )
    .bind(&new.dedup_key)
    .bind(&new.details)
    .bind(new.severity.as_str())
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(id, prev, now)| {
        if prev == now {
            RaiseOutcome::Repeated(id)
        } else {
            RaiseOutcome::Escalated(id)
        }
    }))
}

async fn resolve_row(
    conn: &mut PgConnection,
    dedup_key: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE alerts SET status = 'resolved', resolved_at = now() \
         WHERE dedup_key = $1 AND status <> 'resolved' RETURNING id",
    )
    .bind(dedup_key)
    .fetch_optional(conn)
    .await
}

async fn raise_steps(conn: &mut PgConnection, new: &NewAlert) -> Result<RaiseOutcome, sqlx::Error> {
    if let Some(id) = try_open(conn, new).await? {
        return Ok(RaiseOutcome::Opened(id));
    }
    if RESOLVE_BETWEEN_RAISE_STEPS.swap(false, Ordering::SeqCst) {
        resolve_row(conn, &new.dedup_key).await?;
    }
    if let Some(outcome) = bump(conn, new).await? {
        return Ok(outcome);
    }
    // The live row was resolved between the two steps: retry opening once.
    if let Some(id) = try_open(conn, new).await? {
        return Ok(RaiseOutcome::Opened(id));
    }
    // Lost the race again; one last bump, then give up rather than loop.
    bump(conn, new).await?.ok_or_else(|| {
        sqlx::Error::Protocol(format!(
            "raise: alert {} kept changing state between steps",
            new.dedup_key
        ))
    })
}

/// Raise an alert, deduplicating on `new.dedup_key` while one is open or
/// acknowledged. A new alert (`Opened`) or a higher severity (`Escalated`)
/// enqueues one outbox row per matching channel on the same connection; a
/// repeat only bumps `occurrences` and `last_seen_at`. Run it inside the
/// caller's transaction to commit alert and outbox rows atomically.
pub async fn raise(conn: &mut PgConnection, new: &NewAlert) -> Result<RaiseOutcome, sqlx::Error> {
    let outcome = raise_steps(conn, new).await?;
    match outcome {
        RaiseOutcome::Opened(id) => {
            enqueue(conn, id, NotifyEvent::Opened).await?;
        }
        RaiseOutcome::Escalated(id) => {
            enqueue(conn, id, NotifyEvent::Escalated).await?;
        }
        RaiseOutcome::Repeated(_) => {}
    }
    Ok(outcome)
}

/// Resolve the live alert for `dedup_key`, if any, and enqueue its `resolved`
/// notifications on the same connection. Returns the resolved alert id.
pub async fn resolve(
    conn: &mut PgConnection,
    dedup_key: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    let resolved = resolve_row(conn, dedup_key).await?;
    if let Some(id) = resolved {
        enqueue(conn, id, NotifyEvent::Resolved).await?;
    }
    Ok(resolved)
}

/// Resolve one alert by id (no-op when already resolved) and enqueue its
/// `resolved` notifications on the same connection. Returns whether it changed.
pub async fn resolve_by_id(conn: &mut PgConnection, id: Uuid) -> Result<bool, sqlx::Error> {
    let hit: Option<Uuid> = sqlx::query_scalar(
        "UPDATE alerts SET status = 'resolved', resolved_at = now() \
         WHERE id = $1 AND status <> 'resolved' RETURNING id",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    if hit.is_some() {
        enqueue(conn, id, NotifyEvent::Resolved).await?;
    }
    Ok(hit.is_some())
}

/// Insert one pending outbox row per enabled channel with a route matching the
/// alert (`alert_kind` NULL or equal, severity at or above `min_severity`),
/// carrying a snapshot of the alert. Several matching routes still yield one
/// row per channel. Runs on the caller's connection, so it shares the alert
/// change's transaction. Returns the number of rows written.
pub(crate) async fn enqueue(
    conn: &mut PgConnection,
    alert_id: Uuid,
    event: NotifyEvent,
) -> Result<u64, sqlx::Error> {
    const TS_FMT: &str = "YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"";
    let sql = format!(
        "INSERT INTO notification_outbox (alert_id, channel_id, event, payload, status, next_attempt_at) \
         SELECT a.id, c.id, $2::text, \
                jsonb_build_object('event', $2::text, 'alert', jsonb_build_object( \
                    'id', a.id, 'kind', a.kind, 'severity', a.severity, 'scope', a.scope, \
                    'scope_ref', a.scope_ref, 'title', a.title, 'message', a.message, 'link', a.link, \
                    'first_seen_at', to_char(a.first_seen_at AT TIME ZONE 'UTC', '{TS_FMT}'), \
                    'last_seen_at', to_char(a.last_seen_at AT TIME ZONE 'UTC', '{TS_FMT}'), \
                    'occurrences', a.occurrences, 'status', a.status)), \
                'pending', now() \
         FROM alerts a \
         JOIN notification_channels c ON c.enabled \
         JOIN notification_routes r ON r.channel_id = c.id \
         WHERE a.id = $1 \
           AND (r.alert_kind IS NULL OR r.alert_kind = a.kind) \
           AND array_position(ARRAY['info','warning','critical'], a.severity) \
               >= array_position(ARRAY['info','warning','critical'], r.min_severity) \
         GROUP BY a.id, c.id"
    );
    let res = sqlx::query(&sql)
        .bind(alert_id)
        .bind(event.as_str())
        .execute(conn)
        .await?;
    Ok(res.rows_affected())
}
