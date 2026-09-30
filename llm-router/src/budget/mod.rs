//! Dollar budgets: definitions, spend counters and status.
//!
//! Budgets are enforced pre-call and reconciled post-call. The true cost of a
//! call is known only after the response, so concurrent in-flight calls that
//! each pass the check may overshoot a limit by at most the sum of their costs
//! (ENF-06); sequential calls never pass after exhaustion.
//!
//! Definitions are cached as a whole-table snapshot (stale-while-revalidate,
//! 5 s). A mutation calls [`BudgetEngine::invalidate`] on the local replica;
//! other replicas converge within the TTL.
//!
//! Spend lives in Redis as micro-USD counters ([`keys`]). A missing counter is
//! rebuilt from router-metered `token_usage` only (`direct_llm` and `embedding`
//! rows; orchestrator-internal turns are not counted). The hot path never runs
//! a SUM except on such a miss.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use sqlx::PgPool;
use tokio::sync::{Mutex, RwLock};

pub mod defs;
pub mod keys;
pub mod period;
pub mod store;

pub use defs::{Budget, BudgetAction, Scope};
pub use period::Period;
pub use store::{RedisBudgetStore, StoreBound, StoreError, StoreStats};

use keys::{micros_to_usd, spend_key};
use period::{counter_ttl_secs, elapsed_fraction, period_bounds};

const DEFINITIONS_TTL: Duration = Duration::from_secs(5);
/// Below this fraction of the period a linear projection is noise.
const MIN_PROJECTION_FRACTION: f64 = 0.01;

const SUM_USER: &str = "SELECT COALESCE(ROUND(SUM(cost_usd) * 1000000), 0)::bigint FROM token_usage \
     WHERE operation_type IN ('direct_llm','embedding') AND created_at >= $2 AND created_at < $3 \
     AND user_id = $1";
const SUM_AGENT: &str = "SELECT COALESCE(ROUND(SUM(cost_usd) * 1000000), 0)::bigint FROM token_usage \
     WHERE operation_type IN ('direct_llm','embedding') AND created_at >= $2 AND created_at < $3 \
     AND agent_id = $1";
const SUM_PLATFORM: &str = "SELECT COALESCE(ROUND(SUM(cost_usd) * 1000000), 0)::bigint FROM token_usage \
     WHERE operation_type IN ('direct_llm','embedding') AND created_at >= $1 AND created_at < $2";

#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    /// The counter store (or the DB needed to rebuild from) could not answer.
    #[error("budget counter store unavailable: {0}")]
    StoreUnavailable(String),
    #[error("budget database error: {0}")]
    Db(#[from] sqlx::Error),
}

impl From<StoreError> for BudgetError {
    fn from(e: StoreError) -> Self {
        BudgetError::StoreUnavailable(e.0)
    }
}

type Snapshot = Option<(Instant, Arc<Vec<Budget>>)>;

pub struct BudgetEngine {
    db: Option<PgPool>,
    store: Option<RedisBudgetStore>,
    snapshot: Arc<RwLock<Snapshot>>,
    refreshing: Arc<AtomicBool>,
    /// Bumped by `invalidate`; a load that started before a bump must not
    /// overwrite the cleared snapshot with pre-mutation data.
    generation: Arc<AtomicU64>,
    rebuild_locks: DashMap<String, Arc<Mutex<()>>>,
}

impl BudgetEngine {
    pub fn new(db: PgPool, redis: Option<redis::Client>) -> Self {
        Self {
            db: Some(db),
            store: redis.map(RedisBudgetStore::new),
            snapshot: Arc::default(),
            refreshing: Arc::default(),
            generation: Arc::default(),
            rebuild_locks: DashMap::new(),
        }
    }

    /// An engine with no DB and no Redis: zero definitions, never errors.
    pub fn disabled() -> Self {
        Self {
            db: None,
            store: None,
            snapshot: Arc::default(),
            refreshing: Arc::default(),
            generation: Arc::default(),
            rebuild_locks: DashMap::new(),
        }
    }

    /// Enabled budget definitions (snapshot, stale-while-revalidate).
    pub async fn definitions(&self) -> Result<Arc<Vec<Budget>>, BudgetError> {
        let Some(db) = &self.db else {
            return Ok(Arc::new(Vec::new()));
        };
        let cached = self
            .snapshot
            .read()
            .await
            .as_ref()
            .map(|(at, defs)| (at.elapsed() > DEFINITIONS_TTL, defs.clone()));
        if let Some((stale, defs)) = cached {
            if stale && !self.refreshing.swap(true, Ordering::AcqRel) {
                let (db, snapshot, refreshing, generation) = (
                    db.clone(),
                    self.snapshot.clone(),
                    self.refreshing.clone(),
                    self.generation.clone(),
                );
                tokio::spawn(async move {
                    let started_at = generation.load(Ordering::Acquire);
                    match defs::load_budgets(&db, true).await {
                        Ok(fresh) if generation.load(Ordering::Acquire) == started_at => {
                            *snapshot.write().await = Some((Instant::now(), Arc::new(fresh)));
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(%e, "budget definitions refresh: db error"),
                    }
                    refreshing.store(false, Ordering::Release);
                });
            }
            return Ok(defs);
        }
        let started_at = self.generation.load(Ordering::Acquire);
        let fresh = Arc::new(defs::load_budgets(db, true).await?);
        if self.generation.load(Ordering::Acquire) == started_at {
            *self.snapshot.write().await = Some((Instant::now(), fresh.clone()));
        }
        Ok(fresh)
    }

    /// Drop the snapshot so the next read reloads synchronously.
    pub async fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        *self.snapshot.write().await = None;
    }

    #[doc(hidden)]
    pub fn store_stats(&self) -> StoreStats {
        self.store.as_ref().map(|s| s.stats()).unwrap_or_default()
    }

    /// Current-period spend (micro-USD) per budget, pre-call/status path
    /// (`StoreBound::Check`).
    pub async fn spend_micros(
        &self,
        budgets: &[&Budget],
        now: DateTime<Utc>,
    ) -> Result<Vec<i64>, BudgetError> {
        self.spend_micros_bound(budgets, now, StoreBound::Check)
            .await
    }

    /// As [`spend_micros`](Self::spend_micros) with an explicit store bound, so
    /// the post-call path can rebuild under `Record` (1 s) instead of 50 ms.
    pub async fn spend_micros_bound(
        &self,
        budgets: &[&Budget],
        now: DateTime<Utc>,
        bound: StoreBound,
    ) -> Result<Vec<i64>, BudgetError> {
        if budgets.is_empty() {
            return Ok(Vec::new());
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| BudgetError::StoreUnavailable("no counter store configured".into()))?;
        let keys: Vec<String> = budgets
            .iter()
            .map(|b| spend_key(b.id, period_bounds(b.period, now).0))
            .collect();
        let found = store.mget(&keys, bound).await?;
        let mut out = Vec::with_capacity(budgets.len());
        for ((budget, key), value) in budgets.iter().zip(&keys).zip(found) {
            out.push(match value {
                Some(v) => v,
                None => self.rebuild(store, budget, key, now, bound).await?,
            });
        }
        Ok(out)
    }

    async fn rebuild(
        &self,
        store: &RedisBudgetStore,
        budget: &Budget,
        key: &str,
        now: DateTime<Utc>,
        bound: StoreBound,
    ) -> Result<i64, BudgetError> {
        let (start, end) = period_bounds(budget.period, now);
        // A tokio mutex held across the rebuild await is intentional: it makes
        // concurrent misses on one key wait for the first rebuild instead of each
        // running the SUM. (The no-guard-across-await rule targets std guards.)
        let lock = self
            .rebuild_locks
            .entry(key.to_owned())
            .or_default()
            .clone();
        let guard = lock.lock().await;
        let value = match store.mget(&[key.to_owned()], bound).await?.first() {
            Some(Some(v)) => *v,
            _ => {
                let sum = self.sum_spend(budget, start, end).await?;
                let created = store
                    .set_nx_ex(key, sum, counter_ttl_secs(budget.period, start), bound)
                    .await?;
                if created {
                    // A counter first created at or above a threshold would never
                    // fire its event: no later increment crosses it.
                    self.emit_threshold_events(budget, start, sum).await;
                    sum
                } else {
                    // Lost the NX race: the winner emitted; use its value.
                    store
                        .mget(&[key.to_owned()], bound)
                        .await?
                        .first()
                        .copied()
                        .flatten()
                        .unwrap_or(sum)
                }
            }
        };
        drop(guard);
        self.rebuild_locks.remove(key);
        Ok(value)
    }

    async fn sum_spend(
        &self,
        budget: &Budget,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<i64, BudgetError> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| BudgetError::StoreUnavailable("no database configured".into()))?;
        let sum = match (budget.scope, budget.target_id) {
            (Scope::Platform, _) => {
                sqlx::query_scalar::<_, i64>(SUM_PLATFORM)
                    .bind(start)
                    .bind(end)
                    .fetch_one(db)
                    .await?
            }
            (scope, Some(target)) => {
                let sql = if scope == Scope::User {
                    SUM_USER
                } else {
                    SUM_AGENT
                };
                sqlx::query_scalar::<_, i64>(sql)
                    .bind(target)
                    .bind(start)
                    .bind(end)
                    .fetch_one(db)
                    .await?
            }
            // A user/agent budget without a target cannot exist (table CHECK).
            (_, None) => 0,
        };
        Ok(sum)
    }

    /// Record `soft_threshold` / `hard_limit` for every level `spend_micros` has
    /// reached. Best effort and exactly once per (budget, period, kind) via the
    /// table's UNIQUE constraint; never fails the caller.
    pub async fn emit_threshold_events(
        &self,
        budget: &Budget,
        period_start: DateTime<Utc>,
        spend_micros: i64,
    ) {
        let Some(db) = &self.db else { return };
        let levels = [
            ("soft_threshold", budget.soft_micros()),
            ("hard_limit", budget.limit_micros),
        ];
        for (kind, at) in levels {
            if spend_micros < at {
                continue;
            }
            let res = sqlx::query(
                "INSERT INTO budget_events (budget_id, period_start, kind, spend_usd, limit_usd) \
                 VALUES ($1, $2, $3, $4::float8, $5::float8) \
                 ON CONFLICT (budget_id, period_start, kind) DO NOTHING",
            )
            .bind(budget.id)
            .bind(period_start)
            .bind(kind)
            .bind(micros_to_usd(spend_micros))
            .bind(micros_to_usd(budget.limit_micros))
            .execute(db)
            .await;
            if let Err(e) = res {
                tracing::warn!(budget_id = %budget.id, kind, %e, "emit_threshold_events: db error");
            }
        }
    }
}

// ─── status ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetState {
    Ok,
    Soft,
    Downgrading,
    Blocked,
    Disabled,
    /// The counter store was unavailable; spend is not reported as zero.
    Unknown,
}

impl BudgetState {
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetState::Ok => "ok",
            BudgetState::Soft => "soft",
            BudgetState::Downgrading => "downgrading",
            BudgetState::Blocked => "blocked",
            BudgetState::Disabled => "disabled",
            BudgetState::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BudgetStatus {
    pub period_start: DateTime<Utc>,
    pub resets_at: DateTime<Utc>,
    pub spend_micros: Option<i64>,
    pub pct_used: Option<f64>,
    pub projected_micros: Option<i64>,
    pub state: BudgetState,
}

/// Status of `budget` at `now` given its counter value (`None` = unknown).
pub fn status_of(budget: &Budget, spend_micros: Option<i64>, now: DateTime<Utc>) -> BudgetStatus {
    let (period_start, resets_at) = period_bounds(budget.period, now);
    let Some(spend) = spend_micros else {
        return BudgetStatus {
            period_start,
            resets_at,
            spend_micros: None,
            pct_used: None,
            projected_micros: None,
            state: if budget.enabled {
                BudgetState::Unknown
            } else {
                BudgetState::Disabled
            },
        };
    };
    let pct = (spend as f64 / budget.limit_micros as f64 * 100.0 * 100.0).round() / 100.0;
    let fraction = elapsed_fraction(budget.period, now);
    let projected = if fraction < MIN_PROJECTION_FRACTION {
        spend
    } else {
        (spend as f64 / fraction).round() as i64
    };
    let state = if !budget.enabled {
        BudgetState::Disabled
    } else {
        match budget.action {
            BudgetAction::Block if spend >= budget.limit_micros => BudgetState::Blocked,
            BudgetAction::Downgrade if spend >= budget.ceiling_micros() => BudgetState::Blocked,
            BudgetAction::Downgrade if spend >= budget.limit_micros => BudgetState::Downgrading,
            _ if spend >= budget.soft_micros() => BudgetState::Soft,
            _ => BudgetState::Ok,
        }
    };
    BudgetStatus {
        period_start,
        resets_at,
        spend_micros: Some(spend),
        pct_used: Some(pct),
        projected_micros: Some(projected),
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn budget(action: BudgetAction) -> Budget {
        Budget {
            id: Uuid::new_v4(),
            name: "b".into(),
            scope: Scope::Platform,
            target_id: None,
            period: Period::Daily,
            limit_micros: 4_000_000,
            soft_threshold_pct: 80,
            action,
            downgrade_ceiling_pct: 125,
            enabled: true,
            created_by: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn noon() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap()
    }

    #[test]
    fn pct_and_projection() {
        let s = status_of(&budget(BudgetAction::Block), Some(1_000_000), noon());
        assert_eq!(s.pct_used, Some(25.0));
        assert_eq!(s.projected_micros, Some(2_000_000));
        assert_eq!(s.state, BudgetState::Ok);
    }

    #[test]
    fn projection_is_spend_when_period_just_started() {
        let now = Utc.with_ymd_and_hms(2026, 3, 15, 0, 5, 0).unwrap();
        let s = status_of(&budget(BudgetAction::Block), Some(700_000), now);
        assert_eq!(s.projected_micros, Some(700_000));
    }

    #[test]
    fn state_precedence() {
        let block = budget(BudgetAction::Block);
        let down = budget(BudgetAction::Downgrade);
        let state = |b: &Budget, spend| status_of(b, spend, noon()).state;
        assert_eq!(state(&block, Some(3_200_000)), BudgetState::Soft);
        assert_eq!(state(&block, Some(4_000_000)), BudgetState::Blocked);
        assert_eq!(state(&down, Some(4_000_000)), BudgetState::Downgrading);
        assert_eq!(state(&down, Some(4_500_000)), BudgetState::Downgrading);
        assert_eq!(state(&down, Some(5_000_000)), BudgetState::Blocked);
        assert_eq!(state(&block, None), BudgetState::Unknown);
        let mut off = budget(BudgetAction::Block);
        off.enabled = false;
        assert_eq!(state(&off, Some(9_000_000)), BudgetState::Disabled);
        assert_eq!(state(&off, None), BudgetState::Disabled);
    }

    #[tokio::test]
    async fn disabled_engine_never_touches_backends() {
        let engine = BudgetEngine::disabled();
        assert!(engine.definitions().await.unwrap().is_empty());
        assert!(engine.spend_micros(&[], noon()).await.unwrap().is_empty());
        assert_eq!(engine.store_stats().mget_calls, 0);
        engine.invalidate().await;
    }
}
