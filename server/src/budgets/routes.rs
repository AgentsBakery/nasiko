//! `/api/budgets` handlers.
//!
//! Every admin handler starts with `authz::require_admin_caller`: the outer
//! `require_user_manager` layer is allow-all in OSS, so the role check has to
//! live here. Errors use the `{error, code}` envelope; 5xx bodies never carry
//! internal text.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use nasiko_llm_router::budget::defs::{self, Budget, Scope};
use nasiko_llm_router::budget::keys::micros_to_usd;
use serde_json::{Value, json};
use uuid::Uuid;

use super::models::{
    BudgetView, CreateBudgetRequest, DEFAULT_DOWNGRADE_CEILING_PCT, DEFAULT_SOFT_THRESHOLD_PCT,
    UpdateBudgetRequest, validate_create,
};
use crate::auth::Claims;
use crate::state::AppState;
use crate::users::authz;

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": message, "code": code}))).into_response()
}

fn bad_request(code: &str, message: &str) -> Response {
    api_error(StatusCode::BAD_REQUEST, code, message)
}

fn not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "not_found", "budget not found")
}

fn internal(site: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!(%e, "{site}: error");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "internal error",
    )
}

/// Views for `budgets` with spend read in one MGET. Disabled budgets are not
/// counted by the router, so their spend is reported unknown rather than stale.
/// A store failure is logged and reported as unknown spend, never as zero.
async fn views_for(state: &AppState, budgets: &[Budget]) -> Vec<BudgetView> {
    let now = Utc::now();
    let live: Vec<&Budget> = budgets.iter().filter(|b| b.enabled).collect();
    let spends = match state.budgets.spend_micros(&live, now).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(%e, "views_for: counter read failed, reporting spend unknown");
            Vec::new()
        }
    };
    let mut spend_by_id = std::collections::HashMap::new();
    if spends.len() == live.len() {
        for (b, s) in live.iter().zip(spends) {
            spend_by_id.insert(b.id, s);
        }
    }
    budgets
        .iter()
        .map(|b| BudgetView::new(b, spend_by_id.get(&b.id).copied(), now))
        .collect()
}

async fn view_one(state: &AppState, budget: &Budget) -> BudgetView {
    views_for(state, std::slice::from_ref(budget))
        .await
        .remove(0)
}

async fn target_exists(state: &AppState, scope: Scope, target: Uuid) -> Result<bool, Response> {
    let sql = match scope {
        Scope::User => "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND deleted_at IS NULL)",
        Scope::Agent => "SELECT EXISTS(SELECT 1 FROM agents WHERE id = $1 AND deleted_at IS NULL)",
        Scope::Platform => return Ok(true),
    };
    sqlx::query_scalar(sql)
        .bind(target)
        .fetch_one(&state.db)
        .await
        .map_err(|e| internal("target_exists", e))
}

// ─── admin CRUD ──────────────────────────────────────────────────────────────

/// List every budget with live status (admin).
#[utoipa::path(
    get,
    path = "/api/budgets",
    tag = "budgets",
    responses(
        (status = 200, description = "All budgets with live status", body = Vec<BudgetView>),
        (status = 403, description = "Caller is not an admin (admin_required)"),
    ),
)]
pub(crate) async fn list_budgets(State(state): State<AppState>, claims: Claims) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    match defs::load_budgets(&state.db, false).await {
        Ok(budgets) => Json(json!({"data": views_for(&state, &budgets).await})).into_response(),
        Err(e) => internal("list_budgets", e),
    }
}

/// Create a budget (admin).
#[utoipa::path(
    post,
    path = "/api/budgets",
    tag = "budgets",
    request_body = CreateBudgetRequest,
    responses(
        (status = 201, description = "Created budget", body = BudgetView),
        (status = 400, description = "Invalid input; `code` is a stable slug"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
    ),
)]
pub(crate) async fn create_budget(
    State(state): State<AppState>,
    claims: Claims,
    Json(req): Json<CreateBudgetRequest>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    if let Err((code, msg)) = validate_create(&req) {
        return bad_request(code, &msg);
    }
    // validate_create accepted the scope.
    let Some(scope) = Scope::parse(&req.scope) else {
        return bad_request("invalid_scope", "scope must be user, agent or platform");
    };
    if let Some(target) = req.target_id {
        match target_exists(&state, scope, target).await {
            Ok(true) => {}
            Ok(false) => return bad_request("target_not_found", "target does not exist"),
            Err(r) => return r,
        }
    }
    let created_by = claims.user_uuid().ok();
    let inserted: Result<Uuid, _> = sqlx::query_scalar(
        "INSERT INTO budgets (name, scope, target_id, period, limit_usd, soft_threshold_pct, \
         action, downgrade_ceiling_pct, enabled, created_by) \
         VALUES ($1, $2, $3, $4, $5::float8::numeric, $6, $7, $8, $9, $10) RETURNING id",
    )
    .bind(req.name.trim())
    .bind(&req.scope)
    .bind(req.target_id)
    .bind(&req.period)
    .bind(req.limit_usd)
    .bind(req.soft_threshold_pct.unwrap_or(DEFAULT_SOFT_THRESHOLD_PCT) as i16)
    .bind(&req.action)
    .bind(
        req.downgrade_ceiling_pct
            .unwrap_or(DEFAULT_DOWNGRADE_CEILING_PCT) as i16,
    )
    .bind(req.enabled.unwrap_or(true))
    .bind(created_by)
    .fetch_one(&state.db)
    .await;
    let id = match inserted {
        Ok(id) => id,
        Err(e) => return internal("create_budget", e),
    };
    state.budgets.invalidate().await;
    match defs::load_budget(&state.db, id).await {
        Ok(Some(b)) => (
            StatusCode::CREATED,
            Json(json!({"data": view_one(&state, &b).await})),
        )
            .into_response(),
        Ok(None) => internal("create_budget", "row vanished after insert"),
        Err(e) => internal("create_budget", e),
    }
}

/// Get one budget with live status (admin).
#[utoipa::path(
    get,
    path = "/api/budgets/{id}",
    tag = "budgets",
    params(("id" = Uuid, Path, description = "Budget id")),
    responses(
        (status = 200, description = "Budget with live status", body = BudgetView),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "No such budget"),
    ),
)]
pub(crate) async fn get_budget(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    match defs::load_budget(&state.db, id).await {
        Ok(Some(b)) => Json(json!({"data": view_one(&state, &b).await})).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal("get_budget", e),
    }
}

/// Update a budget (admin). `scope` and `target_id` cannot change.
#[utoipa::path(
    put,
    path = "/api/budgets/{id}",
    tag = "budgets",
    params(("id" = Uuid, Path, description = "Budget id")),
    request_body = UpdateBudgetRequest,
    responses(
        (status = 200, description = "Updated budget", body = BudgetView),
        (status = 400, description = "Invalid input or scope_immutable"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "No such budget"),
    ),
)]
pub(crate) async fn update_budget(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateBudgetRequest>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    let existing = match defs::load_budget(&state.db, id).await {
        Ok(Some(b)) => b,
        Ok(None) => return not_found(),
        Err(e) => return internal("update_budget", e),
    };
    let scope_changed = req
        .scope
        .as_deref()
        .is_some_and(|s| s != existing.scope.as_str());
    let target_changed = req.target_id.is_some_and(|t| Some(t) != existing.target_id);
    if scope_changed || target_changed {
        return bad_request(
            "scope_immutable",
            "scope and target_id cannot be changed; create a new budget instead",
        );
    }
    let merged = CreateBudgetRequest {
        name: req.name.unwrap_or(existing.name.clone()),
        scope: existing.scope.as_str().into(),
        target_id: existing.target_id,
        period: req.period.unwrap_or(existing.period.as_str().into()),
        limit_usd: req
            .limit_usd
            .unwrap_or(micros_to_usd(existing.limit_micros)),
        soft_threshold_pct: Some(
            req.soft_threshold_pct
                .unwrap_or(i64::from(existing.soft_threshold_pct)),
        ),
        action: req.action.unwrap_or(existing.action.as_str().into()),
        downgrade_ceiling_pct: Some(
            req.downgrade_ceiling_pct
                .unwrap_or(i64::from(existing.downgrade_ceiling_pct)),
        ),
        enabled: Some(req.enabled.unwrap_or(existing.enabled)),
    };
    if let Err((code, msg)) = validate_create(&merged) {
        return bad_request(code, &msg);
    }
    let updated = sqlx::query(
        "UPDATE budgets SET name = $2, period = $3, limit_usd = $4::float8::numeric, \
         soft_threshold_pct = $5, action = $6, downgrade_ceiling_pct = $7, enabled = $8, \
         updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(merged.name.trim())
    .bind(&merged.period)
    .bind(merged.limit_usd)
    .bind(
        merged
            .soft_threshold_pct
            .unwrap_or(DEFAULT_SOFT_THRESHOLD_PCT) as i16,
    )
    .bind(&merged.action)
    .bind(
        merged
            .downgrade_ceiling_pct
            .unwrap_or(DEFAULT_DOWNGRADE_CEILING_PCT) as i16,
    )
    .bind(merged.enabled.unwrap_or(true))
    .execute(&state.db)
    .await;
    match updated {
        Ok(r) if r.rows_affected() == 0 => return not_found(),
        Ok(_) => {}
        Err(e) => return internal("update_budget", e),
    }
    state.budgets.invalidate().await;
    match defs::load_budget(&state.db, id).await {
        Ok(Some(b)) => Json(json!({"data": view_one(&state, &b).await})).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal("update_budget", e),
    }
}

/// Hard-delete a budget and its events (admin).
#[utoipa::path(
    delete,
    path = "/api/budgets/{id}",
    tag = "budgets",
    params(("id" = Uuid, Path, description = "Budget id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "Caller is not an admin (admin_required)"),
        (status = 404, description = "No such budget"),
    ),
)]
pub(crate) async fn delete_budget(
    State(state): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = authz::require_admin_caller(&state, &claims).await {
        return r;
    }
    match sqlx::query("DELETE FROM budgets WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
    {
        Ok(r) if r.rows_affected() == 0 => not_found(),
        Ok(_) => {
            state.budgets.invalidate().await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal("delete_budget", e),
    }
}

// ─── self-service ────────────────────────────────────────────────────────────

/// Budgets that apply to the caller: their own, their agents', and platform-wide.
/// Non-admins see platform rows without dollar amounts.
#[utoipa::path(
    get,
    path = "/api/budgets/me",
    tag = "budgets",
    responses((status = 200, description = "Budgets applying to the caller", body = Vec<BudgetView>)),
)]
pub(crate) async fn my_budgets(State(state): State<AppState>, claims: Claims) -> Response {
    let caller = match claims.user_uuid() {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let is_admin = match authz::caller_is_admin(&state, &claims).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let owned: Result<Vec<Uuid>, _> =
        sqlx::query_scalar("SELECT id FROM agents WHERE owner_id = $1 AND deleted_at IS NULL")
            .bind(caller)
            .fetch_all(&state.db)
            .await;
    let owned = match owned {
        Ok(v) => v,
        Err(e) => return internal("my_budgets", e),
    };
    let all = match defs::load_budgets(&state.db, true).await {
        Ok(v) => v,
        Err(e) => return internal("my_budgets", e),
    };
    let mine: Vec<Budget> = all
        .into_iter()
        .filter(|b| match (b.scope, b.target_id) {
            (Scope::Platform, _) => true,
            (Scope::User, Some(t)) => t == caller,
            (Scope::Agent, Some(t)) => owned.contains(&t),
            _ => false,
        })
        .collect();
    let data: Vec<Value> = views_for(&state, &mine)
        .await
        .iter()
        .zip(&mine)
        .map(|(view, b)| view.to_json(b.scope == Scope::Platform && !is_admin))
        .collect();
    Json(json!({"data": data})).into_response()
}
