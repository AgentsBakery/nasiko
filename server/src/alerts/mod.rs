//! Alerting: alert records with dedup and auto-resolve, the sources that raise
//! them (budget events today), the outbox that notifications flow through, and
//! the background workers that drive it all.

pub mod engine;
pub mod models;
