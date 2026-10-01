//! Error-rate and p95-latency monitors: admin CRUD and the per-minute evaluator.
//!
//! Semantics worth knowing before changing a query:
//! - `error_rate` is `100 * failures / (failures + successes)`. Failures are
//!   `llm_call_failures` rows (provider 4xx/5xx, timeouts, transport, stream
//!   errors, per the locked Phase 3 decision); successes are router-metered
//!   `token_usage` rows. Responses failed-attempt rows (`failed:*` / `http:*`
//!   finish reasons) are zero-cost audit rows, so they count as neither
//!   successes nor latency samples anywhere.
//! - `p95_latency_ms` is the p95 of `latency_ms` over successes. For streaming
//!   calls that is end-to-end generation time, not time to first token.
//! - A monitor alert opens when the metric exceeds its threshold with at least
//!   `min_samples` calls in the window, and resolves only after
//!   `RESOLVE_CLEAR_EVALS` consecutive clear evaluations that also meet
//!   `min_samples`. Windows with too few samples change nothing in either
//!   direction, so a quiet period never flaps an alert.
//!
//! Every handler starts with `authz::require_admin_caller`: the outer
//! `require_user_manager` layer is allow-all in OSS, so the role check has to
//! live here. Errors use the `{error, code}` envelope; 5xx bodies never carry
//! internal text and `scope_ref` is always a bound parameter.

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use utoipa::ToSchema;
use uuid::Uuid;

use super::engine;
use super::models::{AlertKind, AlertScope, NewAlert, Severity, tokenops_link};
use crate::auth::Claims;
use crate::state::AppState;
use crate::users::authz;

/// Consecutive clear evaluations before a monitor alert resolves.
pub const RESOLVE_CLEAR_EVALS: i64 = 3;
/// Cap on enabled monitors; the evaluator runs each one every tick.
pub const MAX_ENABLED_MONITORS: i64 = 100;

const DEFAULT_MIN_SAMPLES: i64 = 20;
const DEFAULT_SEVERITY: &str = "warning";
const MAX_NAME_LEN: usize = 120;
const MAX_MODEL_LEN: usize = 200;
const MIN_WINDOW_MINUTES: i64 = 5;
const MAX_WINDOW_MINUTES: i64 = 1440;
const MAX_ERROR_RATE_PCT: f64 = 100.0;
/// Upper bound for one monitor's evaluation queries.
const EVAL_TIMEOUT: Duration = Duration::from_secs(5);

const METRICS: [&str; 2] = ["error_rate", "p95_latency_ms"];
const SCOPES: [&str; 3] = ["agent", "model", "platform"];

/// Router-metered calls that count as successes. `$1` is the evaluation time,
/// `$2` the window in minutes. `finish_reason` `failed:*` / `http:*` marks the
/// Responses surface's zero-cost failed-attempt rows.
const SUCCESS_PREDICATE: &str = "operation_type IN ('direct_llm', 'embedding') \
     AND (finish_reason IS NULL OR (finish_reason NOT LIKE 'failed:%' AND finish_reason NOT LIKE 'http:%')) \
     AND created_at > $1::timestamptz - make_interval(mins => $2) AND created_at <= $1::timestamptz";

/// Same window for `llm_call_failures`.
const FAILURE_WINDOW: &str =
    "created_at > $1::timestamptz - make_interval(mins => $2) AND created_at <= $1::timestamptz";

const MONITOR_COLUMNS: &str = "id, name, metric, scope, scope_ref, window_minutes, \
     threshold::float8 AS threshold, min_samples, severity, enabled, created_at, updated_at";

// ─── types ───────────────────────────────────────────────────────────────────

/// A monitor as returned by the API.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct MonitorView {
    pub id: Uuid,
    pub name: String,
    /// `error_rate` (percent) or `p95_latency_ms`.
    pub metric: String,
    /// `agent`, `model` or `platform`.
    pub scope: String,
    /// Agent uuid or model name; absent for `platform`.
    pub scope_ref: Option<String>,
    pub window_minutes: i32,
    pub threshold: f64,
    pub min_samples: i32,
    /// `info`, `warning` or `critical`.
    pub severity: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateMonitorRequest {
    pub name: String,
    /// `error_rate` or `p95_latency_ms`.
    pub metric: String,
    /// `agent`, `model` or `platform`.
    pub scope: String,
    pub scope_ref: Option<String>,
    /// 5 to 1440.
    pub window_minutes: i64,
    pub threshold: f64,
    /// Minimum calls in the window before the monitor can breach or clear (default 20).
    pub min_samples: Option<i64>,
    /// Default `warning`.
    pub severity: Option<String>,
    pub enabled: Option<bool>,
}

/// Every field optional; absent fields keep their stored value.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateMonitorRequest {
    pub name: Option<String>,
    pub metric: Option<String>,
    pub scope: Option<String>,
    pub scope_ref: Option<String>,
    pub window_minutes: Option<i64>,
    pub threshold: Option<f64>,
    pub min_samples: Option<i64>,
    pub severity: Option<String>,
    pub enabled: Option<bool>,
}

/// Fully merged, validated monitor fields.
struct MonitorFields {
    name: String,
    metric: String,
    scope: String,
    scope_ref: Option<String>,
    window_minutes: i64,
    threshold: f64,
    min_samples: i64,
    severity: String,
    enabled: bool,
}

fn validate(f: &MonitorFields) -> Result<(), (&'static str, &'static str)> {
    let name_len = f.name.trim().chars().count();
    if name_len == 0 || name_len > MAX_NAME_LEN {
        return Err(("invalid_name", "name must be 1 to 120 characters"));
    }
    if !METRICS.contains(&f.metric.as_str()) {
        return Err((
            "invalid_metric",
            "metric must be error_rate or p95_latency_ms",
        ));
    }
    if !SCOPES.contains(&f.scope.as_str()) {
        return Err(("invalid_scope", "scope must be agent, model or platform"));
    }
    match (f.scope.as_str(), f.scope_ref.as_deref()) {
        ("platform", None) => {}
        ("platform", Some(_)) => {
            return Err(("invalid_scope_ref", "platform monitors take no scope_ref"));
        }
        ("model", Some(m)) if !m.trim().is_empty() && m.chars().count() <= MAX_MODEL_LEN => {}
        ("model", _) => {
            return Err((
                "invalid_scope_ref",
                "model scope needs a model name up to 200 characters",
            ));
        }
        // Agent existence is checked against the database by the handler.
        ("agent", Some(a)) if Uuid::parse_str(a).is_ok() => {}
        _ => return Err(("invalid_scope_ref", "agent scope needs an agent uuid")),
    }
    if !(MIN_WINDOW_MINUTES..=MAX_WINDOW_MINUTES).contains(&f.window_minutes) {
        return Err((
            "invalid_window",
            "window_minutes must be between 5 and 1440",
        ));
    }
    if !f.threshold.is_finite()
        || f.threshold <= 0.0
        || (f.metric == "error_rate" && f.threshold > MAX_ERROR_RATE_PCT)
    {
        return Err((
            "invalid_threshold",
            "threshold must be positive (and at most 100 for error_rate)",
        ));
    }
    if f.min_samples < 1 || f.min_samples > i64::from(i32::MAX) {
        return Err(("invalid_min_samples", "min_samples must be at least 1"));
    }
    if Severity::parse(&f.severity).is_none() {
        return Err((
            "invalid_severity",
            "severity must be info, warning or critical",
        ));
    }
    Ok(())
}

// ─── responses ───────────────────────────────────────────────────────────────

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": message, "code": code}))).into_response()
}

fn bad_request(code: &str, message: &str) -> Response {
    api_error(StatusCode::BAD_REQUEST, code, message)
}

fn not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "not_found", "monitor not found")
}

fn limit_reached() -> Response {
    bad_request(
        "monitor_limit_reached",
        "the maximum number of enabled monitors is reached",
    )
}

fn internal(site: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!(%e, "{site}: error");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "internal error",
    )
}

async fn agent_exists(state: &AppState, scope_ref: &str) -> Result<bool, Response> {
    let id = Uuid::parse_str(scope_ref)
        .map_err(|_| bad_request("invalid_scope_ref", "agent scope needs an agent uuid"))?;
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agents WHERE id = $1 AND deleted_at IS NULL)")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .map_err(|e| internal("agent_exists", e))
}

async fn enabled_count(db: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM monitors WHERE enabled")
        .fetch_one(db)
        .await
}

async fn load_monitor(db: &PgPool, id: Uuid) -> Result<Option<MonitorView>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {MONITOR_COLUMNS} FROM monitors WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await
}

// ─── handlers ────────────────────────────────────────────────────────────────

/// List monitors, newest first (admin).
#[utoipa::path(
    get,
    path = "/api/monitors",
    tag = "alerts",
    responses(
        (status = 200, description = "All monitors: {data: [...]}", body = Vec<MonitorView>),
        (status = 403, description = "Caller is not an admin (admin_required)"),
    ),
)]
pub(crate) async fn list_monitors(State(state): State<AppState>, claims: Claims) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let rows: Result<Vec<MonitorView>, _> = sqlx::query_as(&format!(
        "SELECT {MONITOR_COLUMNS} FROM monitors ORDER BY created_at DESC, id"
    ))
    .fetch_all(&state.db)
    .await;
    match rows {
        Ok(data) => Json(json!({"data": data})).into_response(),
        Err(e) => internal("list_monitors", e),
    }
}

/// Create a monitor (admin).
#[utoipa::path(
    post,
    path = "/api/monitors",
    tag = "alerts",
    request_body = CreateMonitorRequest,
    responses(
        (status = 201, description = "Created monitor: {data}", body = MonitorView),
        (status = 400, description = "Invalid input or monitor_limit_reached; `code` is a stable slug"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
    ),
)]
pub(crate) async fn create_monitor(
    State(state): State<AppState>,
    claims: Claims,
    Json(req): Json<CreateMonitorRequest>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let fields = MonitorFields {
        name: req.name.trim().to_owned(),
        metric: req.metric,
        scope: req.scope,
        scope_ref: req.scope_ref,
        window_minutes: req.window_minutes,
        threshold: req.threshold,
        min_samples: req.min_samples.unwrap_or(DEFAULT_MIN_SAMPLES),
        severity: req.severity.unwrap_or_else(|| DEFAULT_SEVERITY.to_owned()),
        enabled: req.enabled.unwrap_or(true),
    };
    if let Err((code, msg)) = validate(&fields) {
        return bad_request(code, msg);
    }
    if let (true, Some(agent)) = (fields.scope == "agent", fields.scope_ref.as_deref()) {
        match agent_exists(&state, agent).await {
            Ok(true) => {}
            Ok(false) => return bad_request("invalid_scope_ref", "agent does not exist"),
            Err(r) => return r,
        }
    }
    if fields.enabled {
        match enabled_count(&state.db).await {
            Ok(n) if n >= MAX_ENABLED_MONITORS => return limit_reached(),
            Ok(_) => {}
            Err(e) => return internal("create_monitor", e),
        }
    }
    let inserted: Result<MonitorView, _> = sqlx::query_as(&format!(
        "INSERT INTO monitors (name, metric, scope, scope_ref, window_minutes, threshold, \
         min_samples, severity, enabled, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6::float8::numeric, $7, $8, $9, $10) \
         RETURNING {MONITOR_COLUMNS}"
    ))
    .bind(&fields.name)
    .bind(&fields.metric)
    .bind(&fields.scope)
    .bind(&fields.scope_ref)
    .bind(fields.window_minutes as i32)
    .bind(fields.threshold)
    .bind(fields.min_samples as i32)
    .bind(&fields.severity)
    .bind(fields.enabled)
    .bind(claims.user_uuid().ok())
    .fetch_one(&state.db)
    .await;
    match inserted {
        Ok(view) => (StatusCode::CREATED, Json(json!({"data": view}))).into_response(),
        Err(e) => internal("create_monitor", e),
    }
}

/// Read one monitor (admin).
#[utoipa::path(
    get,
    path = "/api/monitors/{id}",
    tag = "alerts",
    params(("id" = Uuid, Path, description = "Monitor id")),
    responses(
        (status = 200, description = "The monitor: {data}", body = MonitorView),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "No such monitor"),
    ),
)]
pub(crate) async fn get_monitor(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    match load_monitor(&state.db, id).await {
        Ok(Some(view)) => Json(json!({"data": view})).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal("get_monitor", e),
    }
}

/// Update a monitor; absent fields are kept (admin). Disabling resolves its
/// open alert on the next resolver sweep.
#[utoipa::path(
    put,
    path = "/api/monitors/{id}",
    tag = "alerts",
    params(("id" = Uuid, Path, description = "Monitor id")),
    request_body = UpdateMonitorRequest,
    responses(
        (status = 200, description = "Updated monitor: {data}", body = MonitorView),
        (status = 400, description = "Invalid input or monitor_limit_reached"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "No such monitor"),
    ),
)]
pub(crate) async fn update_monitor(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateMonitorRequest>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let existing = match load_monitor(&state.db, id).await {
        Ok(Some(m)) => m,
        Ok(None) => return not_found(),
        Err(e) => return internal("update_monitor", e),
    };
    let scope = req.scope.unwrap_or(existing.scope.clone());
    // A scope change without a new ref must not inherit a ref meant for another scope.
    let scope_ref = match (req.scope_ref, scope == existing.scope) {
        (Some(r), _) => Some(r),
        (None, true) => existing.scope_ref.clone(),
        (None, false) => None,
    };
    let fields = MonitorFields {
        name: req
            .name
            .map_or(existing.name.clone(), |n| n.trim().to_owned()),
        metric: req.metric.unwrap_or(existing.metric.clone()),
        scope,
        scope_ref,
        window_minutes: req
            .window_minutes
            .unwrap_or(i64::from(existing.window_minutes)),
        threshold: req.threshold.unwrap_or(existing.threshold),
        min_samples: req.min_samples.unwrap_or(i64::from(existing.min_samples)),
        severity: req.severity.unwrap_or(existing.severity.clone()),
        enabled: req.enabled.unwrap_or(existing.enabled),
    };
    if let Err((code, msg)) = validate(&fields) {
        return bad_request(code, msg);
    }
    if let (true, Some(agent)) = (fields.scope == "agent", fields.scope_ref.as_deref())
        && fields.scope_ref != existing.scope_ref
    {
        match agent_exists(&state, agent).await {
            Ok(true) => {}
            Ok(false) => return bad_request("invalid_scope_ref", "agent does not exist"),
            Err(r) => return r,
        }
    }
    if fields.enabled && !existing.enabled {
        match enabled_count(&state.db).await {
            Ok(n) if n >= MAX_ENABLED_MONITORS => return limit_reached(),
            Ok(_) => {}
            Err(e) => return internal("update_monitor", e),
        }
    }
    let updated: Result<Option<MonitorView>, _> = sqlx::query_as(&format!(
        "UPDATE monitors SET name = $2, metric = $3, scope = $4, scope_ref = $5, \
         window_minutes = $6, threshold = $7::float8::numeric, min_samples = $8, \
         severity = $9, enabled = $10 WHERE id = $1 RETURNING {MONITOR_COLUMNS}"
    ))
    .bind(id)
    .bind(&fields.name)
    .bind(&fields.metric)
    .bind(&fields.scope)
    .bind(&fields.scope_ref)
    .bind(fields.window_minutes as i32)
    .bind(fields.threshold)
    .bind(fields.min_samples as i32)
    .bind(&fields.severity)
    .bind(fields.enabled)
    .fetch_optional(&state.db)
    .await;
    match updated {
        Ok(Some(view)) => Json(json!({"data": view})).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal("update_monitor", e),
    }
}

/// Delete a monitor (admin). Its open alert is resolved by the resolver sweep.
#[utoipa::path(
    delete,
    path = "/api/monitors/{id}",
    tag = "alerts",
    params(("id" = Uuid, Path, description = "Monitor id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "No such monitor"),
    ),
)]
pub(crate) async fn delete_monitor(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    match sqlx::query("DELETE FROM monitors WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
    {
        Ok(r) if r.rows_affected() == 0 => not_found(),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal("delete_monitor", e),
    }
}

// ─── evaluation ──────────────────────────────────────────────────────────────

/// `AND` clause restricting a query to the monitor's scope, with the scope ref
/// bound as `$3`; platform monitors bind nothing.
fn scope_clause(scope: &str) -> &'static str {
    match scope {
        "agent" => " AND agent_id = $3::uuid",
        "model" => " AND model = $3",
        _ => "",
    }
}

/// Current metric value and the number of calls it was computed from.
async fn evaluate(
    conn: &mut PgConnection,
    m: &MonitorView,
    now: DateTime<Utc>,
) -> Result<(Option<f64>, i64), sqlx::Error> {
    let scope = scope_clause(&m.scope);
    if m.metric == "p95_latency_ms" {
        let sql = format!(
            "SELECT percentile_cont(0.95) WITHIN GROUP (ORDER BY latency_ms)::float8, count(*) \
             FROM token_usage WHERE latency_ms IS NOT NULL AND {SUCCESS_PREDICATE}{scope}"
        );
        let mut q = sqlx::query_as::<_, (Option<f64>, i64)>(&sql)
            .bind(now)
            .bind(m.window_minutes);
        if let Some(r) = scope_ref_for(m) {
            q = q.bind(r);
        }
        return q.fetch_one(conn).await;
    }
    let successes_sql =
        format!("SELECT count(*) FROM token_usage WHERE {SUCCESS_PREDICATE}{scope}");
    let failures_sql =
        format!("SELECT count(*) FROM llm_call_failures WHERE {FAILURE_WINDOW}{scope}");
    let mut successes = sqlx::query_scalar::<_, i64>(&successes_sql)
        .bind(now)
        .bind(m.window_minutes);
    if let Some(r) = scope_ref_for(m) {
        successes = successes.bind(r);
    }
    let successes = successes.fetch_one(&mut *conn).await?;
    let mut failures = sqlx::query_scalar::<_, i64>(&failures_sql)
        .bind(now)
        .bind(m.window_minutes);
    if let Some(r) = scope_ref_for(m) {
        failures = failures.bind(r);
    }
    let failures = failures.fetch_one(conn).await?;
    let samples = successes + failures;
    let value = (samples > 0).then(|| 100.0 * failures as f64 / samples as f64);
    Ok((value, samples))
}

/// The `$3` bind for scoped monitors. Agent refs were validated as uuids on
/// write and are cast in SQL, so a bad stored value fails the query rather than
/// matching the wrong rows.
fn scope_ref_for(m: &MonitorView) -> Option<&str> {
    match m.scope.as_str() {
        "platform" => None,
        _ => m.scope_ref.as_deref(),
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn breach_alert(m: &MonitorView, value: f64, samples: i64) -> NewAlert {
    let value = round2(value);
    let unit = if m.metric == "error_rate" { "%" } else { " ms" };
    let (scope, agent) = match m.scope.as_str() {
        "agent" => (
            AlertScope::Agent,
            m.scope_ref.as_deref().and_then(|r| Uuid::parse_str(r).ok()),
        ),
        "model" => (AlertScope::Model, None),
        _ => (AlertScope::Platform, None),
    };
    NewAlert {
        kind: AlertKind::MonitorBreach,
        severity: Severity::parse(&m.severity).unwrap_or(Severity::Warning),
        scope,
        scope_ref: m.scope_ref.clone(),
        dedup_key: format!("monitor:{}", m.id),
        title: format!("Monitor '{}' breached", m.name),
        message: format!(
            "{} is {value}{unit} (threshold {}{unit}) over the last {} minutes across {samples} calls",
            m.metric, m.threshold, m.window_minutes
        ),
        link: tokenops_link(agent, None),
        details: json!({
            "monitor_id": m.id,
            "metric": m.metric,
            "value": value,
            "threshold": m.threshold,
            "window_minutes": m.window_minutes,
            "samples": samples,
            "clear_streak": 0,
        }),
    }
}

/// Evaluate every enabled monitor once. Returns the number of state changes
/// (alerts raised or bumped, clear evaluations recorded, alerts resolved).
///
/// One monitor's failure is logged and skipped so it cannot starve the rest.
pub async fn tick_monitors(db: &PgPool, now: DateTime<Utc>) -> Result<usize, sqlx::Error> {
    let monitors: Vec<MonitorView> = sqlx::query_as(&format!(
        "SELECT {MONITOR_COLUMNS} FROM monitors WHERE enabled ORDER BY created_at, id LIMIT $1"
    ))
    .bind(MAX_ENABLED_MONITORS)
    .fetch_all(db)
    .await?;
    let mut changes = 0;
    for m in &monitors {
        match tick_one(db, m, now).await {
            Ok(n) => changes += n,
            Err(e) => {
                tracing::warn!(monitor_id = %m.id, %e, "tick_monitors: evaluation failed, skipping");
            }
        }
    }
    Ok(changes)
}

async fn tick_one(db: &PgPool, m: &MonitorView, now: DateTime<Utc>) -> Result<usize, sqlx::Error> {
    let mut tx = db.begin().await?;
    // Cross-replica guard: the streak/occurrence updates below are read-modify-write
    // on the alert row, so two replicas evaluating one monitor at once could
    // double-count. The transaction-scoped lock is released on commit or rollback.
    let locked: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('alerts:monitor:' || $1::text, 0))",
    )
    .bind(m.id.to_string())
    .fetch_one(&mut *tx)
    .await?;
    if !locked {
        return Ok(0);
    }
    let evaluated = match tokio::time::timeout(EVAL_TIMEOUT, evaluate(&mut tx, m, now)).await {
        Ok(r) => r?,
        Err(_) => {
            tracing::warn!(monitor_id = %m.id, "tick_one: evaluation timed out, skipping");
            return Ok(0);
        }
    };
    let (value, samples) = evaluated;
    let min_samples = i64::from(m.min_samples);
    let Some(value) = value.filter(|_| samples >= min_samples) else {
        return Ok(0);
    };
    let key = format!("monitor:{}", m.id);
    if value > m.threshold {
        engine::raise(&mut tx, &breach_alert(m, value, samples)).await?;
        tx.commit().await?;
        return Ok(1);
    }
    let streak: Option<Option<i32>> = sqlx::query_scalar(
        "UPDATE alerts SET details = jsonb_set(details, '{clear_streak}', \
             to_jsonb(COALESCE((details->>'clear_streak')::int, 0) + 1)) \
         WHERE dedup_key = $1 AND status <> 'resolved' \
         RETURNING (details->>'clear_streak')::int",
    )
    .bind(&key)
    .fetch_optional(&mut *tx)
    .await?;
    let mut changed = 0;
    if let Some(streak) = streak {
        changed = 1;
        if i64::from(streak.unwrap_or(0)) >= RESOLVE_CLEAR_EVALS {
            engine::resolve(&mut tx, &key).await?;
        }
    }
    tx.commit().await?;
    Ok(changed)
}
