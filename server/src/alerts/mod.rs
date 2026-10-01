//! Alerting: alert records with dedup and auto-resolve, the sources that raise
//! them (budget events today), the outbox that notifications flow through, and
//! the background workers that drive it all.

use std::future::Future;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use chrono::Utc;
use nasiko_config::AlertsConfig;
use sqlx::PgPool;
use tokio::time::MissedTickBehavior;

use crate::notifications::dispatch::{DispatchDeps, tick_outbox_dispatch};
use crate::state::AppState;

pub mod budget_events;
pub mod engine;
pub mod models;
pub mod routes;
pub mod sweep;

pub use budget_events::tick_budget_events;
pub use sweep::tick_resolve_sweep;

/// Admin alert routes. `GET /alerts/{id}` is deliberately absent: later plans
/// add static siblings (`/alerts/spike-markers`) and axum rejects overlaps.
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/alerts", get(routes::list_alerts))
        .route("/alerts/{id}/acknowledge", post(routes::acknowledge_alert))
}

/// Spawn the alert workers. Each runs only when its interval is above 0 (tests
/// leave every interval at 0 and drive the `tick_*` functions directly).
pub fn spawn_workers(db: PgPool, cfg: AlertsConfig) {
    if cfg.budget_events_secs > 0 {
        let db = db.clone();
        tokio::spawn(run_every(
            "budget_events",
            Duration::from_secs(cfg.budget_events_secs),
            move || {
                let db = db.clone();
                async move { tick_budget_events(&db, Utc::now()).await.map(|_| ()) }
            },
        ));
    }
    if cfg.outbox_secs > 0 {
        let db = db.clone();
        let deps = DispatchDeps::from_config(&cfg);
        tokio::spawn(run_every(
            "outbox_dispatch",
            Duration::from_secs(cfg.outbox_secs),
            move || {
                let db = db.clone();
                let deps = deps.clone();
                async move { tick_outbox_dispatch(&db, &deps).await.map(|_| ()) }
            },
        ));
    }
    if cfg.resolve_sweep_secs > 0 {
        let db = db.clone();
        tokio::spawn(run_every(
            "resolve_sweep",
            Duration::from_secs(cfg.resolve_sweep_secs),
            move || {
                let db = db.clone();
                async move { tick_resolve_sweep(&db, Utc::now()).await.map(|_| ()) }
            },
        ));
    }
}

/// Interval loop modelled on the container-hours meter: a failed tick is
/// logged and skipped, and the loop never exits or panics.
async fn run_every<F, Fut>(name: &'static str, every: Duration, tick: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<(), sqlx::Error>>,
{
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Err(e) = tick().await {
            tracing::warn!(worker = name, error = %e, "alerts worker tick failed");
        }
    }
}
