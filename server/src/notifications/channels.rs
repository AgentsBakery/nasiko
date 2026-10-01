//! `/api/notification-channels` and `/api/notification-deliveries` handlers.
//!
//! Every handler starts with `authz::require_admin_caller`: the outer
//! `require_user_manager` layer is allow-all in OSS, so the role check has to
//! live here. A channel's URL and HMAC key are stored encrypted
//! (`SecretsCrypto::try_for_system`) and are write-only: responses carry only
//! `url_hint` and `has_hmac_secret`. Error bodies are `{error, code}` with
//! generic messages; a rejected URL is never echoed back.

use std::collections::BTreeSet;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use nasiko_secrets::SecretsCrypto;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Postgres, QueryBuilder, Row};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::dispatch::{ChannelSecret, DispatchDeps, SendResult, send_once};
use super::payload::{test_snapshot, url_hint};
use super::ssrf::{ChannelKind, validate_channel_url};
use crate::alerts::models::{AlertKind, Severity};
use crate::auth::Claims;
use crate::state::AppState;
use crate::users::authz;

const MAX_NAME_CHARS: usize = 120;
const MAX_HMAC_SECRET_CHARS: usize = 256;
const MAX_ROUTES: usize = 20;
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;
const CHANNEL_COLUMNS: &str =
    "id, name, kind, url_hint, has_hmac_secret, enabled, created_at, updated_at";

// ─── helpers ─────────────────────────────────────────────────────────────────

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": message, "code": code}))).into_response()
}

fn bad_request(code: &str, message: &str) -> Response {
    api_error(StatusCode::BAD_REQUEST, code, message)
}

fn not_found() -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "not_found",
        "notification channel not found",
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

/// Crypto failures log the site only: the error text can name key material.
fn secrets_unavailable(site: &str) -> Response {
    tracing::error!("{site}: secrets crypto unavailable");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "secrets_unavailable",
        "secret storage is unavailable",
    )
}

fn invalid_url(e: super::ssrf::UrlError) -> Response {
    bad_request(
        "invalid_channel_url",
        &format!("channel url is not allowed ({})", e.slug()),
    )
}

fn validate_name(name: &str) -> Result<String, Response> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(bad_request(
            "invalid_name",
            "name must be 1 to 120 characters",
        ));
    }
    Ok(name.to_owned())
}

fn validate_hmac(kind: ChannelKind, secret: &str) -> Result<(), Response> {
    if secret.is_empty() {
        return Ok(());
    }
    if kind == ChannelKind::Slack {
        return Err(bad_request(
            "hmac_not_supported",
            "slack channels do not support an hmac secret",
        ));
    }
    if secret.chars().count() > MAX_HMAC_SECRET_CHARS {
        return Err(bad_request(
            "invalid_hmac_secret",
            "hmac_secret must be at most 256 characters",
        ));
    }
    Ok(())
}

// ─── views ───────────────────────────────────────────────────────────────────

/// A channel as the API shows it. Has no `url` and no `hmac_secret`, ever.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChannelView {
    pub id: Uuid,
    pub name: String,
    /// `webhook` or `slack`.
    pub kind: String,
    /// Host plus the last characters of the path.
    pub url_hint: String,
    pub has_hmac_secret: bool,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ChannelView {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            kind: row.try_get("kind")?,
            url_hint: row.try_get("url_hint")?,
            has_hmac_secret: row.try_get("has_hmac_secret")?,
            enabled: row.try_get("enabled")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateChannelRequest {
    pub name: String,
    /// `webhook` or `slack`.
    pub kind: String,
    pub url: String,
    /// Webhook only. Signs every delivery.
    pub hmac_secret: Option<String>,
    pub enabled: Option<bool>,
}

/// Omitted `url`/`hmac_secret` keep the stored value; `hmac_secret: ""` clears it.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateChannelRequest {
    pub name: Option<String>,
    pub url: Option<String>,
    pub hmac_secret: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RouteView {
    pub id: Uuid,
    /// `null` matches every alert kind.
    pub alert_kind: Option<String>,
    pub min_severity: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RouteInput {
    pub alert_kind: Option<String>,
    pub min_severity: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PutRoutesRequest {
    pub routes: Vec<RouteInput>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeliveryView {
    pub id: Uuid,
    pub alert_id: Option<Uuid>,
    pub channel_id: Uuid,
    pub event: String,
    pub status: String,
    pub attempts: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ListDeliveriesQuery {
    pub alert_id: Option<Uuid>,
    pub channel_id: Option<Uuid>,
    /// Default 50, max 200.
    pub limit: Option<i64>,
}

// ─── channels ────────────────────────────────────────────────────────────────

/// List notification channels (admin).
#[utoipa::path(
    get,
    path = "/api/notification-channels",
    tag = "notifications",
    responses(
        (status = 200, description = "All channels", body = Vec<ChannelView>),
        (status = 403, description = "Caller is not an admin (admin_required)"),
    ),
)]
pub(crate) async fn list_channels(State(state): State<AppState>, claims: Claims) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let sql =
        format!("SELECT {CHANNEL_COLUMNS} FROM notification_channels ORDER BY created_at, id");
    match sqlx::query(&sql).fetch_all(&state.db).await {
        Ok(rows) => match rows
            .iter()
            .map(ChannelView::from_row)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(views) => Json(json!({"data": views})).into_response(),
            Err(e) => internal("list_channels", e),
        },
        Err(e) => internal("list_channels", e),
    }
}

/// Create a notification channel (admin).
#[utoipa::path(
    post,
    path = "/api/notification-channels",
    tag = "notifications",
    request_body = CreateChannelRequest,
    responses(
        (status = 201, description = "Created channel", body = ChannelView),
        (status = 400, description = "Invalid input; `code` is a stable slug"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
    ),
)]
pub(crate) async fn create_channel(
    State(state): State<AppState>,
    claims: Claims,
    Json(req): Json<CreateChannelRequest>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let name = match validate_name(&req.name) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let Some(kind) = ChannelKind::parse(&req.kind) else {
        return bad_request("invalid_kind", "kind must be webhook or slack");
    };
    let url = match validate_channel_url(&req.url, kind, state.config.alerts.allow_private_urls) {
        Ok(u) => u,
        Err(e) => return invalid_url(e),
    };
    let hmac_secret = req.hmac_secret.filter(|s| !s.is_empty());
    if let Some(secret) = &hmac_secret
        && let Err(r) = validate_hmac(kind, secret)
    {
        return r;
    }
    let Ok(crypto) = SecretsCrypto::try_for_system() else {
        return secrets_unavailable("create_channel");
    };
    let secret = ChannelSecret {
        url: url.to_string(),
        hmac_secret: hmac_secret.clone(),
    };
    let config_encrypted = match serde_json::to_string(&secret) {
        Ok(json) => crypto.encrypt(&json),
        Err(e) => return internal("create_channel", e),
    };
    let sql = format!(
        "INSERT INTO notification_channels \
         (name, kind, config_encrypted, url_hint, has_hmac_secret, enabled) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING {CHANNEL_COLUMNS}"
    );
    let row = sqlx::query(&sql)
        .bind(&name)
        .bind(kind.as_str())
        .bind(&config_encrypted)
        .bind(url_hint(&url))
        .bind(hmac_secret.is_some())
        .bind(req.enabled.unwrap_or(true))
        .fetch_one(&state.db)
        .await;
    match row
        .map_err(|e| e.to_string())
        .and_then(|r| ChannelView::from_row(&r).map_err(|e| e.to_string()))
    {
        Ok(view) => (StatusCode::CREATED, Json(json!({"data": view}))).into_response(),
        Err(e) => internal("create_channel", e),
    }
}

/// Fetch one channel (admin).
#[utoipa::path(
    get,
    path = "/api/notification-channels/{id}",
    tag = "notifications",
    params(("id" = Uuid, Path, description = "Channel id")),
    responses(
        (status = 200, description = "The channel", body = ChannelView),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "Unknown channel (not_found)"),
    ),
)]
pub(crate) async fn get_channel(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let sql = format!("SELECT {CHANNEL_COLUMNS} FROM notification_channels WHERE id = $1");
    match sqlx::query(&sql).bind(id).fetch_optional(&state.db).await {
        Ok(Some(row)) => match ChannelView::from_row(&row) {
            Ok(view) => Json(json!({"data": view})).into_response(),
            Err(e) => internal("get_channel", e),
        },
        Ok(None) => not_found(),
        Err(e) => internal("get_channel", e),
    }
}

/// Update a channel (admin). Kind is immutable.
#[utoipa::path(
    put,
    path = "/api/notification-channels/{id}",
    tag = "notifications",
    params(("id" = Uuid, Path, description = "Channel id")),
    request_body = UpdateChannelRequest,
    responses(
        (status = 200, description = "Updated channel", body = ChannelView),
        (status = 400, description = "Invalid input; `code` is a stable slug"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "Unknown channel (not_found)"),
    ),
)]
pub(crate) async fn update_channel(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChannelRequest>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let existing = match sqlx::query(
        "SELECT name, kind, config_encrypted, url_hint, has_hmac_secret, enabled \
         FROM notification_channels WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(e) => return internal("update_channel", e),
    };
    let read = |col: &str| existing.try_get::<String, _>(col);
    let (Ok(old_name), Ok(kind_str), Ok(old_config), Ok(old_hint)) = (
        read("name"),
        read("kind"),
        read("config_encrypted"),
        read("url_hint"),
    ) else {
        return internal("update_channel", "unreadable channel row");
    };
    let old_has_hmac: bool = existing.try_get("has_hmac_secret").unwrap_or(false);
    let old_enabled: bool = existing.try_get("enabled").unwrap_or(true);
    let Some(kind) = ChannelKind::parse(&kind_str) else {
        return internal("update_channel", "stored channel kind is invalid");
    };

    let name = match req.name.as_deref().map(validate_name).transpose() {
        Ok(n) => n.unwrap_or(old_name),
        Err(r) => return r,
    };
    let new_url = match req
        .url
        .as_deref()
        .map(|u| validate_channel_url(u, kind, state.config.alerts.allow_private_urls))
        .transpose()
    {
        Ok(u) => u,
        Err(e) => return invalid_url(e),
    };
    if let Some(secret) = req.hmac_secret.as_deref()
        && let Err(r) = validate_hmac(kind, secret)
    {
        return r;
    }

    let (config_encrypted, hint, has_hmac) = if new_url.is_some() || req.hmac_secret.is_some() {
        let Ok(crypto) = SecretsCrypto::try_for_system() else {
            return secrets_unavailable("update_channel");
        };
        let Ok(plain) = crypto.decrypt(&old_config) else {
            return secrets_unavailable("update_channel");
        };
        let Ok(mut secret) = serde_json::from_str::<ChannelSecret>(&plain) else {
            return internal("update_channel", "stored channel config is unreadable");
        };
        let mut hint = old_hint;
        if let Some(url) = &new_url {
            secret.url = url.to_string();
            hint = url_hint(url);
        }
        if let Some(new_secret) = req.hmac_secret {
            secret.hmac_secret = Some(new_secret).filter(|s| !s.is_empty());
        }
        let has_hmac = secret.hmac_secret.is_some();
        match serde_json::to_string(&secret) {
            Ok(json) => (crypto.encrypt(&json), hint, has_hmac),
            Err(e) => return internal("update_channel", e),
        }
    } else {
        (old_config, old_hint, old_has_hmac)
    };

    let sql = format!(
        "UPDATE notification_channels \
         SET name = $2, config_encrypted = $3, url_hint = $4, has_hmac_secret = $5, enabled = $6 \
         WHERE id = $1 RETURNING {CHANNEL_COLUMNS}"
    );
    match sqlx::query(&sql)
        .bind(id)
        .bind(&name)
        .bind(&config_encrypted)
        .bind(&hint)
        .bind(has_hmac)
        .bind(req.enabled.unwrap_or(old_enabled))
        .fetch_optional(&state.db)
        .await
    {
        Ok(Some(row)) => match ChannelView::from_row(&row) {
            Ok(view) => Json(json!({"data": view})).into_response(),
            Err(e) => internal("update_channel", e),
        },
        Ok(None) => not_found(),
        Err(e) => internal("update_channel", e),
    }
}

/// Delete a channel with its routes and delivery history (admin).
#[utoipa::path(
    delete,
    path = "/api/notification-channels/{id}",
    tag = "notifications",
    params(("id" = Uuid, Path, description = "Channel id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "Unknown channel (not_found)"),
    ),
)]
pub(crate) async fn delete_channel(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    match sqlx::query("DELETE FROM notification_channels WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
    {
        Ok(res) if res.rows_affected() > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => not_found(),
        Err(e) => internal("delete_channel", e),
    }
}

// ─── routes ──────────────────────────────────────────────────────────────────

/// List a channel's routes (admin).
#[utoipa::path(
    get,
    path = "/api/notification-channels/{id}/routes",
    tag = "notifications",
    params(("id" = Uuid, Path, description = "Channel id")),
    responses(
        (status = 200, description = "The channel's routes", body = Vec<RouteView>),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "Unknown channel (not_found)"),
    ),
)]
pub(crate) async fn get_routes(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    match channel_exists(&state, id).await {
        Ok(true) => {}
        Ok(false) => return not_found(),
        Err(e) => return internal("get_routes", e),
    }
    match load_routes(&state, id).await {
        Ok(routes) => Json(json!({"data": routes})).into_response(),
        Err(e) => internal("get_routes", e),
    }
}

async fn channel_exists(state: &AppState, id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM notification_channels WHERE id = $1)")
        .bind(id)
        .fetch_one(&state.db)
        .await
}

async fn load_routes(state: &AppState, id: Uuid) -> Result<Vec<RouteView>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, alert_kind, min_severity FROM notification_routes \
         WHERE channel_id = $1 ORDER BY COALESCE(alert_kind, ''), min_severity",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(RouteView {
                id: r.try_get("id")?,
                alert_kind: r.try_get("alert_kind")?,
                min_severity: r.try_get("min_severity")?,
            })
        })
        .collect()
}

/// Replace a channel's routes (admin).
#[utoipa::path(
    put,
    path = "/api/notification-channels/{id}/routes",
    tag = "notifications",
    params(("id" = Uuid, Path, description = "Channel id")),
    request_body = PutRoutesRequest,
    responses(
        (status = 200, description = "The new route set", body = Vec<RouteView>),
        (status = 400, description = "invalid_route or too_many_routes"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "Unknown channel (not_found)"),
    ),
)]
pub(crate) async fn put_routes(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
    Json(req): Json<PutRoutesRequest>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    if req.routes.len() > MAX_ROUTES {
        return bad_request("too_many_routes", "a channel can have at most 20 routes");
    }
    let mut wanted: BTreeSet<(Option<&'static str>, &'static str)> = BTreeSet::new();
    for route in &req.routes {
        let kind = match route.alert_kind.as_deref() {
            None => None,
            Some(k) => match AlertKind::parse(k) {
                Some(kind) => Some(kind.as_str()),
                None => return bad_request("invalid_route", "unknown alert_kind"),
            },
        };
        let Some(severity) = Severity::parse(&route.min_severity) else {
            return bad_request("invalid_route", "unknown min_severity");
        };
        wanted.insert((kind, severity.as_str()));
    }

    let replaced: Result<bool, sqlx::Error> = async {
        let mut tx = state.db.begin().await?;
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM notification_channels WHERE id = $1)")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Ok(false);
        }
        sqlx::query("DELETE FROM notification_routes WHERE channel_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for (kind, severity) in &wanted {
            sqlx::query(
                "INSERT INTO notification_routes (channel_id, alert_kind, min_severity) \
                 VALUES ($1, $2, $3)",
            )
            .bind(id)
            .bind(kind)
            .bind(severity)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }
    .await;
    match replaced {
        Ok(true) => match load_routes(&state, id).await {
            Ok(routes) => Json(json!({"data": routes})).into_response(),
            Err(e) => internal("put_routes", e),
        },
        Ok(false) => not_found(),
        Err(e) => internal("put_routes", e),
    }
}

// ─── test send ───────────────────────────────────────────────────────────────

/// Send a test notification synchronously (admin, rate limited per caller).
#[utoipa::path(
    post,
    path = "/api/notification-channels/{id}/test",
    tag = "notifications",
    params(("id" = Uuid, Path, description = "Channel id")),
    responses(
        (status = 200, description = "Delivery result; 200 even when the receiver failed", body = SendResult),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "Unknown channel (not_found)"),
        (status = 429, description = "Rate limited; see Retry-After"),
    ),
)]
pub(crate) async fn test_channel(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let row =
        match sqlx::query("SELECT kind, config_encrypted FROM notification_channels WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await
        {
            Ok(Some(row)) => row,
            Ok(None) => return not_found(),
            Err(e) => return internal("test_channel", e),
        };
    let (Ok(kind_str), Ok(config)) = (
        row.try_get::<String, _>("kind"),
        row.try_get::<String, _>("config_encrypted"),
    ) else {
        return internal("test_channel", "unreadable channel row");
    };
    let Some(kind) = ChannelKind::parse(&kind_str) else {
        return internal("test_channel", "stored channel kind is invalid");
    };
    let Ok(crypto) = SecretsCrypto::try_for_system() else {
        return secrets_unavailable("test_channel");
    };
    let secret = match crypto
        .decrypt(&config)
        .ok()
        .and_then(|plain| serde_json::from_str::<ChannelSecret>(&plain).ok())
    {
        Some(secret) => secret,
        None => return secrets_unavailable("test_channel"),
    };

    // The shared app client follows redirects and may reach internal hosts, so
    // test sends use the dispatcher's guarded client instead.
    let deps = DispatchDeps::from_config(&state.config.alerts);
    let snapshot = test_snapshot(Utc::now());
    // Send first, record after: the history row is born final, so the
    // dispatcher can never pick it up and send it a second time.
    let delivery_id = Uuid::new_v4();
    let result = send_once(&deps, kind, &secret, &snapshot, delivery_id).await;
    let inserted = sqlx::query(
        "INSERT INTO notification_outbox \
         (id, alert_id, channel_id, event, payload, status, attempts, last_error, delivered_at) \
         VALUES ($1, NULL, $2, 'test', $3, $4, 1, $5, CASE WHEN $6 THEN now() END)",
    )
    .bind(delivery_id)
    .bind(id)
    .bind(&snapshot)
    .bind(if result.delivered {
        "delivered"
    } else {
        "failed"
    })
    .bind(result.error)
    .bind(result.delivered)
    .execute(&state.db)
    .await;
    if let Err(e) = inserted {
        tracing::warn!(channel_id = %id, %e, "test_channel: recording history failed");
    }
    Json(json!({"data": result})).into_response()
}

// ─── deliveries ──────────────────────────────────────────────────────────────

/// Delivery history, newest first (admin). Never includes payloads.
#[utoipa::path(
    get,
    path = "/api/notification-deliveries",
    tag = "notifications",
    params(ListDeliveriesQuery),
    responses(
        (status = 200, description = "Outbox rows, newest first", body = Vec<DeliveryView>),
        (status = 403, description = "Caller is not an admin (admin_required)"),
    ),
)]
pub(crate) async fn list_deliveries(
    State(state): State<AppState>,
    claims: Claims,
    Query(q): Query<ListDeliveriesQuery>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT id, alert_id, channel_id, event, status, attempts, next_attempt_at, \
         last_error, delivered_at, created_at FROM notification_outbox WHERE true",
    );
    if let Some(alert_id) = q.alert_id {
        qb.push(" AND alert_id = ").push_bind(alert_id);
    }
    if let Some(channel_id) = q.channel_id {
        qb.push(" AND channel_id = ").push_bind(channel_id);
    }
    qb.push(" ORDER BY created_at DESC, id DESC LIMIT ")
        .push_bind(limit);
    let rows = match qb.build().fetch_all(&state.db).await {
        Ok(rows) => rows,
        Err(e) => return internal("list_deliveries", e),
    };
    let views: Result<Vec<DeliveryView>, sqlx::Error> = rows
        .iter()
        .map(|r| {
            Ok(DeliveryView {
                id: r.try_get("id")?,
                alert_id: r.try_get("alert_id")?,
                channel_id: r.try_get("channel_id")?,
                event: r.try_get("event")?,
                status: r.try_get("status")?,
                attempts: r.try_get("attempts")?,
                next_attempt_at: r.try_get("next_attempt_at")?,
                last_error: r.try_get("last_error")?,
                delivered_at: r.try_get("delivered_at")?,
                created_at: r.try_get("created_at")?,
            })
        })
        .collect();
    match views {
        Ok(views) => Json(json!({"data": views})).into_response(),
        Err(e) => internal("list_deliveries", e),
    }
}
