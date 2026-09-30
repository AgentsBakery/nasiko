//! Dollar budgets: admin CRUD with live status, and a per-user read view.
//!
//! Definitions live in `budgets`; live spend comes from the router's shared
//! [`nasiko_llm_router::budget::BudgetEngine`] so this API and the router
//! enforcement read the same counters (rebuilt from `token_usage` on a miss).

pub mod models;
pub mod routes;

use axum::{Router, routing::get};

use crate::state::AppState;

/// Admin-only routes; the caller layers `require_user_manager` on top and every
/// handler additionally checks the admin role (the OSS layer is allow-all).
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route(
            "/budgets",
            get(routes::list_budgets).post(routes::create_budget),
        )
        .route(
            "/budgets/{id}",
            get(routes::get_budget)
                .put(routes::update_budget)
                .delete(routes::delete_budget),
        )
}

/// Any authenticated user: the budgets that apply to the caller.
pub fn me_router() -> Router<AppState> {
    Router::new().route("/budgets/me", get(routes::my_budgets))
}
