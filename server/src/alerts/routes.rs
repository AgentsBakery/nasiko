//! `/api/alerts` handlers (admin only).
//!
//! Every handler starts with `authz::require_admin_caller`: the outer
//! `require_user_manager` layer is allow-all in OSS, so the role check has to
//! live here. Errors use the `{error, code}` envelope; 5xx bodies never carry
//! internal text and filters are bound parameters, never interpolated.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use sqlx::{Postgres, QueryBuilder};
use utoipa::IntoParams;
use uuid::Uuid;

use super::models::{AlertKind, AlertScope, AlertView, Severity};
use crate::auth::Claims;
use crate::state::AppState;
use crate::users::authz;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;
const STATUSES: [&str; 3] = ["open", "acknowledged", "resolved"];
const ALERT_COLUMNS: &str = "id, kind, severity, scope, scope_ref, dedup_key, status, title, \
     message, link, details, first_seen_at, last_seen_at, occurrences, acknowledged_by, \
     acknowledged_at, resolved_at";

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": message, "code": code}))).into_response()
}

fn invalid_filter(message: &str) -> Response {
    api_error(StatusCode::BAD_REQUEST, "invalid_filter", message)
}

fn internal(site: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!(%e, "{site}: error");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "internal error",
    )
}

// ─── cursor helpers ──────────────────────────────────────────────────────────

fn encode_cursor(ts: DateTime<Utc>, id: Uuid) -> String {
    let nanos = ts.timestamp_nanos_opt().unwrap_or(0);
    URL_SAFE_NO_PAD.encode(format!("{nanos}:{id}"))
}

fn decode_cursor(s: &str) -> Option<(DateTime<Utc>, Uuid)> {
    let raw = URL_SAFE_NO_PAD.decode(s).ok()?;
    let txt = std::str::from_utf8(&raw).ok()?;
    let (nanos, id) = txt.split_once(':')?;
    Some((
        DateTime::from_timestamp_nanos(nanos.parse().ok()?),
        id.parse().ok()?,
    ))
}

// ─── list ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, IntoParams)]
pub struct ListAlertsQuery {
    /// `open`, `acknowledged` or `resolved`.
    pub status: Option<String>,
    /// `budget_soft`, `budget_hard`, `spend_spike` or `monitor_breach`.
    pub kind: Option<String>,
    /// `info`, `warning` or `critical`.
    pub severity: Option<String>,
    /// `platform`, `agent`, `model` or `user`.
    pub scope: Option<String>,
    /// RFC 3339; only alerts first seen at or after this instant.
    pub since: Option<String>,
    /// RFC 3339; only alerts first seen before this instant.
    pub until: Option<String>,
    /// Page size (default 50, max 200).
    pub limit: Option<i64>,
    /// Opaque cursor from a previous page's `next_cursor`.
    pub cursor: Option<String>,
}

fn parse_time(name: &str, raw: &str) -> Result<DateTime<Utc>, Response> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| invalid_filter(&format!("{name} must be an RFC 3339 timestamp")))
}

/// List alerts, newest first, with filters and cursor paging (admin).
#[utoipa::path(
    get,
    path = "/api/alerts",
    tag = "alerts",
    params(ListAlertsQuery),
    responses(
        (status = 200, description = "Page of alerts: {data, has_more, next_cursor, prev_cursor}", body = Vec<AlertView>),
        (status = 400, description = "Invalid filter or cursor (invalid_filter, invalid_cursor)"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
    ),
)]
pub(crate) async fn list_alerts(
    State(state): State<AppState>,
    claims: Claims,
    Query(q): Query<ListAlertsQuery>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }

    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!("SELECT {ALERT_COLUMNS} FROM alerts WHERE TRUE"));

    if let Some(status) = &q.status {
        if !STATUSES.contains(&status.as_str()) {
            return invalid_filter("unknown status");
        }
        qb.push(" AND status = ").push_bind(status.clone());
    }
    if let Some(kind) = &q.kind {
        if AlertKind::parse(kind).is_none() {
            return invalid_filter("unknown kind");
        }
        qb.push(" AND kind = ").push_bind(kind.clone());
    }
    if let Some(sev) = &q.severity {
        if Severity::parse(sev).is_none() {
            return invalid_filter("unknown severity");
        }
        qb.push(" AND severity = ").push_bind(sev.clone());
    }
    if let Some(scope) = &q.scope {
        if AlertScope::parse(scope).is_none() {
            return invalid_filter("unknown scope");
        }
        qb.push(" AND scope = ").push_bind(scope.clone());
    }
    if let Some(since) = &q.since {
        match parse_time("since", since) {
            Ok(t) => qb.push(" AND first_seen_at >= ").push_bind(t),
            Err(r) => return r,
        };
    }
    if let Some(until) = &q.until {
        match parse_time("until", until) {
            Ok(t) => qb.push(" AND first_seen_at < ").push_bind(t),
            Err(r) => return r,
        };
    }
    if let Some(cursor) = &q.cursor {
        let Some((ts, id)) = decode_cursor(cursor) else {
            return api_error(StatusCode::BAD_REQUEST, "invalid_cursor", "invalid cursor");
        };
        qb.push(" AND (first_seen_at, id) < (")
            .push_bind(ts)
            .push(", ")
            .push_bind(id)
            .push(")");
    }

    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    qb.push(" ORDER BY first_seen_at DESC, id DESC LIMIT ")
        .push_bind(limit + 1);

    let mut rows: Vec<AlertView> = match qb.build_query_as().fetch_all(&state.db).await {
        Ok(rows) => rows,
        Err(e) => return internal("list_alerts", e),
    };
    let has_more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next_cursor = if has_more {
        rows.last().map(|a| encode_cursor(a.first_seen_at, a.id))
    } else {
        None
    };
    Json(json!({
        "data": rows,
        "has_more": has_more,
        "next_cursor": next_cursor,
        "prev_cursor": null,
    }))
    .into_response()
}

// ─── acknowledge ─────────────────────────────────────────────────────────────

/// Acknowledge an open alert (admin). Idempotent for acknowledged alerts; a
/// resolved alert cannot be acknowledged.
#[utoipa::path(
    post,
    path = "/api/alerts/{id}/acknowledge",
    tag = "alerts",
    params(("id" = Uuid, Path, description = "Alert id")),
    responses(
        (status = 200, description = "The acknowledged alert", body = AlertView),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "Unknown alert (not_found)"),
        (status = 409, description = "Alert already resolved (alert_resolved)"),
    ),
)]
pub(crate) async fn acknowledge_alert(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let caller = claims.user_uuid().ok();
    let updated: Result<Option<AlertView>, sqlx::Error> = sqlx::query_as(&format!(
        "UPDATE alerts SET status = 'acknowledged', acknowledged_by = $2, acknowledged_at = now() \
         WHERE id = $1 AND status = 'open' RETURNING {ALERT_COLUMNS}"
    ))
    .bind(id)
    .bind(caller)
    .fetch_optional(&state.db)
    .await;
    match updated {
        Ok(Some(alert)) => return Json(json!({"data": alert})).into_response(),
        Ok(None) => {}
        Err(e) => return internal("acknowledge_alert", e),
    }

    let current: Result<Option<AlertView>, sqlx::Error> =
        sqlx::query_as(&format!("SELECT {ALERT_COLUMNS} FROM alerts WHERE id = $1"))
            .bind(id)
            .fetch_optional(&state.db)
            .await;
    match current {
        Ok(None) => api_error(StatusCode::NOT_FOUND, "not_found", "alert not found"),
        Ok(Some(alert)) if alert.status == "resolved" => api_error(
            StatusCode::CONFLICT,
            "alert_resolved",
            "alert is already resolved",
        ),
        Ok(Some(alert)) => Json(json!({"data": alert})).into_response(),
        Err(e) => internal("acknowledge_alert", e),
    }
}
