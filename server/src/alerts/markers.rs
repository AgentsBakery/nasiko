//! `GET /api/alerts/spike-markers`: spend-spike markers for the TokenOps chart.
//!
//! Open to every authenticated user, scoped like the Phase 1 FinOps endpoints:
//! admins see platform and agent markers, everyone else only the agents they
//! can access and never platform markers. Asking for an agent the caller
//! cannot access is the same 404 as an unknown agent.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::Claims;
use crate::observability::handler::{
    FinopsFilterParams, accessible_agent_ids, resolve_agent_filter, validate_range,
};
use crate::observability::routes::agent_name_fully_accessible;
use crate::observability::service::resolve_window;
use crate::state::AppState;
use crate::users::authz;

/// Most markers returned per request.
const MAX_MARKERS: i64 = 500;

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": message, "code": code}))).into_response()
}

fn internal(site: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!(%e, "{site}: error");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "internal error",
    )
}

/// One spend-spike marker.
#[derive(Debug, Serialize, ToSchema)]
pub struct SpikeMarker {
    pub alert_id: Uuid,
    /// Start of the spiking hour, RFC 3339 UTC.
    pub hour_start: String,
    /// `platform` or `agent`.
    pub scope: String,
    pub scope_ref: Option<String>,
    /// The agent id for agent-scope markers, null for the platform.
    pub agent_id: Option<String>,
    pub severity: String,
    pub spend_usd: Option<f64>,
    pub threshold_usd: Option<f64>,
    pub title: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SpikeMarkersResponse {
    pub data: Vec<SpikeMarker>,
}

#[derive(sqlx::FromRow)]
struct MarkerRow {
    id: Uuid,
    hour_start: DateTime<Utc>,
    scope: String,
    scope_ref: Option<String>,
    severity: String,
    spend_usd: Option<f64>,
    threshold_usd: Option<f64>,
    title: String,
}

/// Spend-spike markers within the requested window, scoped to the caller.
#[utoipa::path(
    get,
    path = "/api/alerts/spike-markers",
    tag = "alerts",
    params(FinopsFilterParams),
    responses(
        (status = 200, description = "Spike markers, newest first", body = SpikeMarkersResponse),
        (status = 400, description = "Invalid range or window"),
        (status = 404, description = "Agent not found or not accessible"),
    ),
    security(("bearer" = []))
)]
pub async fn spike_markers(
    State(state): State<AppState>,
    claims: Claims,
    Query(params): Query<FinopsFilterParams>,
) -> Response {
    if validate_range(params.range.as_deref()).is_err() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_range",
            "range must be 24h, 7d or 30d",
        );
    }
    let (start, end, _) = match resolve_window(
        params.start_time.as_deref(),
        params.end_time.as_deref(),
        params.range.as_deref(),
    ) {
        Ok(w) => w,
        Err(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_window",
                "invalid time window",
            );
        }
    };

    // `scope_refs`: agent uuids (as text) the caller may see; `platform`:
    // whether platform markers are allowed.
    let (scope_refs, platform): (Option<Vec<String>>, bool) = if params.agent_id.is_some() {
        let name = match resolve_agent_filter(&state.db, params.agent_id.as_deref()).await {
            Ok(Some(n)) => n,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "not_found", "agent not found"),
            Err(r) => return r,
        };
        if !agent_name_fully_accessible(&state, &claims, &name).await {
            return api_error(StatusCode::NOT_FOUND, "not_found", "agent not found");
        }
        let ids: Vec<Uuid> = match sqlx::query_scalar(
            "SELECT id FROM agents WHERE name = $1 AND deleted_at IS NULL",
        )
        .bind(&name)
        .fetch_all(&state.db)
        .await
        {
            Ok(ids) => ids,
            Err(e) => return internal("spike_markers: agent lookup", e),
        };
        (Some(ids.iter().map(Uuid::to_string).collect()), false)
    } else {
        match authz::caller_is_admin(&state, &claims).await {
            Err(r) => return r,
            Ok(true) => (None, true),
            Ok(false) => {
                let ids = accessible_agent_ids(&state, &claims)
                    .await
                    .unwrap_or_default();
                (Some(ids.iter().map(Uuid::to_string).collect()), false)
            }
        }
    };

    let rows: Result<Vec<MarkerRow>, _> = sqlx::query_as(
        "SELECT id, (details->>'hour_start')::timestamptz AS hour_start, scope, scope_ref, \
                severity, (details->>'spend_usd')::float8 AS spend_usd, \
                (details->>'threshold_usd')::float8 AS threshold_usd, title \
         FROM alerts \
         WHERE kind = 'spend_spike' AND details->>'hour_start' IS NOT NULL \
           AND (details->>'hour_start')::timestamptz >= $1 \
           AND (details->>'hour_start')::timestamptz < $2 \
           AND ( ($3 AND scope = 'platform') \
                 OR (scope = 'agent' AND ($4::text[] IS NULL OR scope_ref = ANY($4))) ) \
         ORDER BY (details->>'hour_start')::timestamptz DESC, id \
         LIMIT $5",
    )
    .bind(start)
    .bind(end)
    .bind(platform)
    .bind(&scope_refs)
    .bind(MAX_MARKERS)
    .fetch_all(&state.db)
    .await;
    match rows {
        Ok(rows) => Json(SpikeMarkersResponse {
            data: rows
                .into_iter()
                .map(|r| SpikeMarker {
                    alert_id: r.id,
                    hour_start: r.hour_start.to_rfc3339_opts(SecondsFormat::Secs, true),
                    agent_id: (r.scope == "agent").then(|| r.scope_ref.clone()).flatten(),
                    scope: r.scope,
                    scope_ref: r.scope_ref,
                    severity: r.severity,
                    spend_usd: r.spend_usd,
                    threshold_usd: r.threshold_usd,
                    title: r.title,
                })
                .collect(),
        })
        .into_response(),
        Err(e) => internal("spike_markers: query", e),
    }
}
