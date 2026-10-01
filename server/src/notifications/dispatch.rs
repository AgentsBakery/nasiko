//! Outbox dispatcher: delivers `notification_outbox` rows to their channels.
//!
//! Delivery is at-least-once. A worker claims due rows (`FOR UPDATE SKIP
//! LOCKED`, so replicas never claim the same row), commits the claim, sends,
//! and then records the outcome with a guarded update. If a worker dies after
//! sending but before recording, the row is reclaimed once the claim is stale
//! and sent again; receivers dedupe on the `X-Nasiko-Delivery` header, which is
//! the outbox row id and stays stable across retries.
//!
//! Retry schedule (`BACKOFF`): 30s, 2m, 10m, 30m, 2h, then the row is marked
//! `failed` after the 6th attempt. Failure reasons are short slugs: a URL, a
//! response body or a reqwest error string never reaches `last_error` or logs,
//! because the URL itself is a secret.
//!
//! The pool may have a single connection (tests), so the claim transaction is
//! committed and its connection released before any outbound HTTP.

use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use nasiko_config::AlertsConfig;
use nasiko_secrets::SecretsCrypto;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::payload::{
    self, HEADER_DELIVERY, HEADER_EVENT, HEADER_SIGNATURE, HEADER_TIMESTAMP, slack_body,
    webhook_body,
};
use super::ssrf::{self, ChannelKind, check_resolves_public, validate_channel_url};

/// Rows claimed per tick.
pub const CLAIM_BATCH: i64 = 20;
/// Sends in flight at once within a tick.
pub const SEND_CONCURRENCY: usize = 5;
/// Hard bound on one send. `ceil(CLAIM_BATCH / SEND_CONCURRENCY) * SEND_TIMEOUT`
/// (40s) is far below `STALE_CLAIM`, so a healthy batch always finishes before
/// another replica may reclaim its rows.
pub const SEND_TIMEOUT: Duration = ssrf::REQUEST_TIMEOUT;
/// A `sending` row claimed longer ago than this is considered abandoned.
pub const STALE_CLAIM: Duration = Duration::from_secs(120);
/// Attempts (including the first) before a row is marked failed.
pub const MAX_ATTEMPTS: i32 = 6;
/// Delay before attempt N+1 after attempt N failed (index N-1).
pub const BACKOFF: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
    Duration::from_secs(1800),
    Duration::from_secs(7200),
];

/// Delay before the next attempt after `attempts_made` attempts, or `None`
/// when the row has used up its attempts and must be marked failed.
pub fn next_delay(attempts_made: i32) -> Option<Duration> {
    if !(1..MAX_ATTEMPTS).contains(&attempts_made) {
        return None;
    }
    BACKOFF.get(attempts_made as usize - 1).copied()
}

/// Decrypted channel configuration, stored as encrypted JSON.
#[derive(Clone, Serialize, Deserialize)]
pub struct ChannelSecret {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac_secret: Option<String>,
}

// Manual Debug: the URL and HMAC key must never reach a log line.
impl std::fmt::Debug for ChannelSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelSecret").finish_non_exhaustive()
    }
}

/// Everything a send needs besides the row itself.
#[derive(Clone)]
pub struct DispatchDeps {
    pub client: reqwest::Client,
    pub allow_private_urls: bool,
    pub public_base_url: String,
}

impl DispatchDeps {
    pub fn from_config(cfg: &AlertsConfig) -> Self {
        Self {
            client: ssrf::guarded_client(cfg.allow_private_urls),
            allow_private_urls: cfg.allow_private_urls,
            public_base_url: cfg.public_base_url.clone(),
        }
    }
}

/// Outcome of one send attempt. `error` is a slug, never free text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct SendResult {
    pub delivered: bool,
    pub status_code: Option<u16>,
    pub error: Option<&'static str>,
}

impl SendResult {
    fn failed(error: &'static str) -> Self {
        Self {
            delivered: false,
            status_code: None,
            error: Some(error),
        }
    }
}

/// Event names allowed into the `X-Nasiko-Event` header.
fn event_header(snapshot: &Value) -> &'static str {
    match snapshot.get("event").and_then(Value::as_str) {
        Some("opened") => "opened",
        Some("escalated") => "escalated",
        Some("resolved") => "resolved",
        Some("test") => "test",
        _ => "unknown",
    }
}

/// Send one notification. The URL is re-validated and re-resolved on every
/// call (DNS rebinding, stored-before-policy rows), so the dispatcher and the
/// test endpoint share one guarded path.
pub async fn send_once(
    deps: &DispatchDeps,
    kind: ChannelKind,
    secret: &ChannelSecret,
    snapshot: &Value,
    delivery_id: Uuid,
) -> SendResult {
    match tokio::time::timeout(
        SEND_TIMEOUT,
        send_inner(deps, kind, secret, snapshot, delivery_id),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => SendResult::failed("timeout"),
    }
}

async fn send_inner(
    deps: &DispatchDeps,
    kind: ChannelKind,
    secret: &ChannelSecret,
    snapshot: &Value,
    delivery_id: Uuid,
) -> SendResult {
    let url = match validate_channel_url(&secret.url, kind, deps.allow_private_urls) {
        Ok(url) => url,
        Err(e) => return SendResult::failed(e.slug()),
    };
    if let Err(e) = check_resolves_public(&url, deps.allow_private_urls).await {
        return SendResult::failed(e.slug());
    }
    let body = match kind {
        ChannelKind::Webhook => webhook_body(snapshot, Utc::now()),
        ChannelKind::Slack => slack_body(snapshot, &deps.public_base_url),
    };
    let mut req = deps
        .client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(HEADER_EVENT, event_header(snapshot))
        .header(HEADER_DELIVERY, delivery_id.to_string());
    if kind == ChannelKind::Webhook
        && let Some(key) = secret.hmac_secret.as_deref().filter(|k| !k.is_empty())
    {
        let ts = Utc::now().timestamp();
        req = req
            .header(HEADER_TIMESTAMP, ts.to_string())
            .header(HEADER_SIGNATURE, payload::sign(key.as_bytes(), ts, &body));
    }
    match req.body(body).send().await {
        Ok(resp) => {
            let code = resp.status();
            let error = if code.is_success() {
                None
            } else if code.is_redirection() {
                Some("http_3xx")
            } else if code.is_client_error() {
                Some("http_4xx")
            } else if code.is_server_error() {
                Some("http_5xx")
            } else {
                Some("http_other")
            };
            SendResult {
                delivered: error.is_none(),
                status_code: Some(code.as_u16()),
                error,
            }
        }
        Err(e) if e.is_timeout() => SendResult::failed("timeout"),
        Err(e) if e.is_connect() => SendResult::failed("connect"),
        Err(_) => SendResult::failed("request"),
    }
}

/// Record the outcome of one claimed row. The update only applies while the
/// row is still `sending` under the same `claimed_at`: a worker whose claim
/// was reclaimed as stale must not overwrite the newer claim's state. Returns
/// whether the update applied.
pub async fn finish_row(
    db: &PgPool,
    id: Uuid,
    claimed_at: DateTime<Utc>,
    result: &SendResult,
    attempts: i32,
) -> Result<bool, sqlx::Error> {
    let res = if result.delivered {
        sqlx::query(
            "UPDATE notification_outbox \
             SET status = 'delivered', delivered_at = now(), last_error = NULL, claimed_at = NULL \
             WHERE id = $1 AND status = 'sending' AND claimed_at = $2",
        )
        .bind(id)
        .bind(claimed_at)
        .execute(db)
        .await?
    } else if let Some(delay) = next_delay(attempts) {
        sqlx::query(
            "UPDATE notification_outbox \
             SET status = 'pending', last_error = $3, claimed_at = NULL, \
                 next_attempt_at = now() + $4::float8 * interval '1 second' \
             WHERE id = $1 AND status = 'sending' AND claimed_at = $2",
        )
        .bind(id)
        .bind(claimed_at)
        .bind(result.error)
        .bind(delay.as_secs() as f64)
        .execute(db)
        .await?
    } else {
        sqlx::query(
            "UPDATE notification_outbox \
             SET status = 'failed', last_error = $3, claimed_at = NULL \
             WHERE id = $1 AND status = 'sending' AND claimed_at = $2",
        )
        .bind(id)
        .bind(claimed_at)
        .bind(result.error)
        .execute(db)
        .await?
    };
    Ok(res.rows_affected() > 0)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DispatchStats {
    pub claimed: usize,
    pub delivered: usize,
    pub retried: usize,
    pub failed: usize,
}

struct Claimed {
    id: Uuid,
    channel_id: Uuid,
    payload: Value,
    attempts: i32,
    claimed_at: DateTime<Utc>,
    channel: Option<ChannelRow>,
}

struct ChannelRow {
    kind: String,
    enabled: bool,
    config_encrypted: String,
}

/// Claim due rows and load their channels in one short transaction, then
/// release the connection.
async fn claim_batch(db: &PgPool) -> Result<Vec<Claimed>, sqlx::Error> {
    let mut tx = db.begin().await?;
    let rows = sqlx::query(
        "WITH due AS ( \
             SELECT id FROM notification_outbox \
             WHERE (status = 'pending' AND next_attempt_at <= now()) \
                OR (status = 'sending' AND claimed_at < now() - $2::float8 * interval '1 second') \
             ORDER BY next_attempt_at \
             FOR UPDATE SKIP LOCKED \
             LIMIT $1) \
         UPDATE notification_outbox o \
         SET status = 'sending', attempts = o.attempts + 1, claimed_at = now() \
         FROM due WHERE o.id = due.id \
         RETURNING o.id, o.channel_id, o.payload, o.attempts, o.claimed_at",
    )
    .bind(CLAIM_BATCH)
    .bind(STALE_CLAIM.as_secs() as f64)
    .fetch_all(&mut *tx)
    .await?;

    let mut claimed = Vec::with_capacity(rows.len());
    for row in &rows {
        let channel_id: Uuid = row.try_get("channel_id")?;
        let channel = sqlx::query(
            "SELECT kind, enabled, config_encrypted FROM notification_channels WHERE id = $1",
        )
        .bind(channel_id)
        .fetch_optional(&mut *tx)
        .await?
        .map(|c| -> Result<ChannelRow, sqlx::Error> {
            Ok(ChannelRow {
                kind: c.try_get("kind")?,
                enabled: c.try_get("enabled")?,
                config_encrypted: c.try_get("config_encrypted")?,
            })
        })
        .transpose()?;
        claimed.push(Claimed {
            id: row.try_get("id")?,
            channel_id,
            payload: row.try_get("payload")?,
            attempts: row.try_get("attempts")?,
            claimed_at: row.try_get("claimed_at")?,
            channel,
        });
    }
    tx.commit().await?;
    Ok(claimed)
}

/// What to record for a claimed row. `attempts` is the value `finish_row`
/// uses to pick retry versus failed: a terminal failure passes
/// [`MAX_ATTEMPTS`] so it is never retried.
struct Outcome {
    result: SendResult,
    attempts: i32,
}

async fn process(deps: &DispatchDeps, crypto: Option<&SecretsCrypto>, row: &Claimed) -> Outcome {
    let terminal = |error: &'static str| Outcome {
        result: SendResult::failed(error),
        attempts: MAX_ATTEMPTS,
    };
    let Some(channel) = row.channel.as_ref().filter(|c| c.enabled) else {
        return terminal("channel_disabled");
    };
    let Some(kind) = ChannelKind::parse(&channel.kind) else {
        return terminal("invalid_kind");
    };
    let Some(crypto) = crypto else {
        // Master key unavailable is an operator problem that may be fixed
        // before the retries run out, so it stays retryable.
        return Outcome {
            result: SendResult::failed("decrypt_failed"),
            attempts: row.attempts,
        };
    };
    let secret = match crypto
        .decrypt(&channel.config_encrypted)
        .ok()
        .and_then(|plain| serde_json::from_str::<ChannelSecret>(&plain).ok())
    {
        Some(secret) => secret,
        None => {
            tracing::error!(outbox_id = %row.id, channel_id = %row.channel_id,
                "notification dispatch: decrypt failed");
            return terminal("decrypt_failed");
        }
    };
    Outcome {
        result: send_once(deps, kind, &secret, &row.payload, row.id).await,
        attempts: row.attempts,
    }
}

/// One dispatcher pass: claim due rows, send them with bounded concurrency and
/// record each outcome.
pub async fn tick_outbox_dispatch(
    db: &PgPool,
    deps: &DispatchDeps,
) -> Result<DispatchStats, sqlx::Error> {
    let claimed = claim_batch(db).await?;
    let mut stats = DispatchStats {
        claimed: claimed.len(),
        ..DispatchStats::default()
    };
    if claimed.is_empty() {
        return Ok(stats);
    }
    let crypto = SecretsCrypto::try_for_system().ok();
    let crypto = crypto.as_ref();

    let finished: Vec<_> = futures::stream::iter(claimed.iter())
        .map(|row| async move {
            let outcome = process(deps, crypto, row).await;
            let applied = finish_row(
                db,
                row.id,
                row.claimed_at,
                &outcome.result,
                outcome.attempts,
            )
            .await;
            (row, outcome, applied)
        })
        .buffer_unordered(SEND_CONCURRENCY)
        .collect()
        .await;

    for (row, outcome, applied) in finished {
        match applied {
            Ok(true) => {
                if outcome.result.delivered {
                    stats.delivered += 1;
                } else if next_delay(outcome.attempts).is_some() {
                    stats.retried += 1;
                } else {
                    stats.failed += 1;
                }
            }
            Ok(false) => {
                tracing::debug!(outbox_id = %row.id, "notification dispatch: claim lost, result dropped");
            }
            Err(e) => {
                // The row stays `sending` and is reclaimed once stale.
                tracing::warn!(outbox_id = %row.id, channel_id = %row.channel_id, %e,
                    "notification dispatch: recording outcome failed");
            }
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule_and_cap() {
        let secs: Vec<u64> = (1..=5).map(|n| next_delay(n).unwrap().as_secs()).collect();
        assert_eq!(secs, [30, 120, 600, 1800, 7200]);
        assert_eq!(next_delay(6), None);
        assert_eq!(MAX_ATTEMPTS, 6);
    }

    #[test]
    fn healthy_batch_finishes_inside_stale_window() {
        let worst = SEND_TIMEOUT * (CLAIM_BATCH as usize).div_ceil(SEND_CONCURRENCY) as u32;
        assert!(
            worst < STALE_CLAIM,
            "{worst:?} must be below {STALE_CLAIM:?}"
        );
    }
}
