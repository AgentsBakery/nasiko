//! Alerting: alert records with dedup and auto-resolve, the sources that raise
//! them (budget events and error/latency monitors), the outbox that notifications flow through, and
//! the background workers that drive it all.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
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
pub mod markers;
pub mod models;
pub mod monitors;
pub mod routes;
pub mod spike;
pub mod sweep;

pub use budget_events::tick_budget_events;
pub use monitors::tick_monitors;
pub use spike::{SpikeSettings, tick_spike};
pub use sweep::tick_resolve_sweep;

/// Admin alert routes. `GET /alerts/{id}` is deliberately absent: later plans
/// add static siblings (`/alerts/spike-markers`, see `markers_router`) and axum rejects overlaps.
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/alerts", get(routes::list_alerts))
        .route("/alerts/{id}/acknowledge", post(routes::acknowledge_alert))
        .route(
            "/monitors",
            get(monitors::list_monitors).post(monitors::create_monitor),
        )
        .route(
            "/monitors/{id}",
            get(monitors::get_monitor)
                .put(monitors::update_monitor)
                .delete(monitors::delete_monitor),
        )
}

/// Auth-only routes any signed-in user may call; scoping happens in the handler.
pub fn markers_router() -> Router<AppState> {
    Router::new().route("/alerts/spike-markers", get(markers::spike_markers))
}

/// Wall-clock cap on one spike evaluation (the baseline scan is the heavy part).
const SPIKE_TICK_TIMEOUT: Duration = Duration::from_secs(30);

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
    if cfg.monitors_secs > 0 {
        let db = db.clone();
        tokio::spawn(run_every(
            "monitors",
            Duration::from_secs(cfg.monitors_secs),
            move || {
                let db = db.clone();
                async move { tick_monitors(&db, Utc::now()).await.map(|_| ()) }
            },
        ));
    }
    if cfg.spike_secs > 0 {
        let db = db.clone();
        let settings = SpikeSettings {
            sigma: cfg.spike_sigma,
            floor_usd: cfg.spike_floor_usd,
        };
        // Once per complete hour per replica; re-evaluating after a restart is
        // idempotent because spike dedup keys carry the hour.
        let last_hour = Arc::new(AtomicI64::new(i64::MIN));
        tokio::spawn(run_every(
            "spike",
            Duration::from_secs(cfg.spike_secs),
            move || {
                let db = db.clone();
                let last_hour = last_hour.clone();
                async move {
                    let now = Utc::now();
                    let hour = spike::latest_complete_hour(now).timestamp();
                    if hour <= last_hour.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    match tokio::time::timeout(SPIKE_TICK_TIMEOUT, tick_spike(&db, &settings, now))
                        .await
                    {
                        Ok(res) => {
                            res?;
                            last_hour.store(hour, Ordering::Relaxed);
                        }
                        Err(_) => tracing::warn!("spike worker: tick timed out, will retry"),
                    }
                    Ok(())
                }
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
