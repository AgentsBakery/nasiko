//! Handler-level authorization for user management.
//!
//! The check lives in the handlers, not in `require_user_manager`, because the
//! OSS `AuthService::can_manage_users` default is allow-all and that outer layer
//! also wraps member self-service routes (`/users/me`). EE reuses the `pub`
//! handlers (`update_user`, `change_role`, `management_router`), so a check
//! inside them travels with those handlers.
//!
//! "Admin" is `is_superuser OR role = 'admin'` on an active, non-deleted row
//! (same predicate as `check_last_admin`). Only a superuser may mint or touch
//! a superuser.

use axum::{Json, http::StatusCode, response::IntoResponse, response::Response};
use uuid::Uuid;

use crate::auth::Claims;
use crate::state::AppState;

pub const ADMIN_REQUIRED: &str = "admin_required";
pub const SUPERUSER_REQUIRED: &str = "superuser_required";
const INTERNAL: &str = "internal";

fn forbidden(message: &str, code: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({"error": message, "code": code})),
    )
        .into_response()
}

/// Fail-closed response for a DB error while resolving authorization.
fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "internal error", "code": INTERNAL})),
    )
        .into_response()
}

/// Whether the caller is an active admin or superuser. A DB failure yields an
/// error response, never an implicit allow.
pub async fn caller_is_admin(state: &AppState, claims: &Claims) -> Result<bool, Response> {
    if claims.is_superuser {
        return Ok(true);
    }
    let user_id = claims.user_uuid().map_err(|e| e.into_response())?;
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND deleted_at IS NULL \
         AND is_active = true AND (role = 'admin' OR is_superuser))",
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(%e, "caller_is_admin: db error");
        internal_error()
    })
}

/// The 403 `admin_required` response.
pub fn admin_required() -> Response {
    forbidden("requires admin role", ADMIN_REQUIRED)
}

/// Require the caller to be an admin (403 `admin_required` otherwise).
pub async fn require_admin_caller(state: &AppState, claims: &Claims) -> Result<(), Response> {
    if caller_is_admin(state, claims).await? {
        Ok(())
    } else {
        Err(admin_required())
    }
}

/// Require the caller to be a superuser (403 `superuser_required` otherwise).
pub fn require_superuser_caller(claims: &Claims) -> Result<(), Response> {
    if claims.is_superuser {
        Ok(())
    } else {
        Err(forbidden("requires superuser", SUPERUSER_REQUIRED))
    }
}

/// Non-superusers may not act on a superuser target. A missing target passes
/// so the handler's own 404 path applies.
pub async fn require_superuser_for_target(
    state: &AppState,
    claims: &Claims,
    target_id: Uuid,
) -> Result<(), Response> {
    if claims.is_superuser {
        return Ok(());
    }
    let target_is_superuser: Option<bool> =
        sqlx::query_scalar("SELECT is_superuser FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(target_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| {
                tracing::error!(%e, %target_id, "require_superuser_for_target: db error");
                internal_error()
            })?;
    if target_is_superuser == Some(true) {
        Err(forbidden("requires superuser", SUPERUSER_REQUIRED))
    } else {
        Ok(())
    }
}
