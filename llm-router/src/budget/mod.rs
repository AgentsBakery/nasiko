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
//!
//! Ordering of the post-call path (`usage::log_usage`): counters are incremented
//! *before* the `token_usage` INSERT, which shortens the window in which a
//! sequential follow-up call can still see the old spend (ENF-06); keys that were
//! missing are rebuilt *after* the INSERT so the SUM includes the new row (and
//! are not also incremented). Store calls are bounded by [`StoreBound`]: the
//! pre-call check gets 50 ms and fails closed, post-call work gets 1 s.
//!
//! A lost increment never leaves a stale-low counter trusted: the affected keys
//! are deleted (or, if even that fails, marked dirty in-process) so the next read
//! rebuilds them from `token_usage`.
//!
//! Known bounded UNDERCOUNT: when a key is missing (after a Redis flush, or for
//! a brand-new budget or period), another process's SUM snapshot can be taken
//! before a concurrent row commits and still win the `SET NX`; that row's cost is
//! then absent from the counter until the key next expires. This only happens in
//! the flush/new-key window, is at most the cost of calls committing during it,
//! and is the same class as the ENF-06 overshoot.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use sqlx::PgPool;
use tokio::sync::{Mutex, RwLock};

pub mod defs;
pub mod denial;
pub mod keys;
pub mod period;
pub mod store;

pub use defs::{Budget, BudgetAction, Scope};
pub use period::Period;
pub use store::{RedisBudgetStore, StoreBound, StoreError, StoreStats};

use crate::error::GatewayError;
use crate::inbound::InboundFormat;
use denial::{BudgetDenial, DenialKind};
use keys::{micros_to_usd, spend_key};
use period::{counter_ttl_secs, elapsed_fraction, period_bounds};
use uuid::Uuid;

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
    /// Counter keys whose increment was lost and could not be deleted; the next
    /// read treats them as missing and rebuilds from `token_usage`.
    dirty: DashMap<String, ()>,
    /// `(budget, period start)` pairs whose `hard_limit` event this process has
    /// already ensured, so repeated blocked calls do not hit the DB each time.
    hard_emitted: DashMap<(Uuid, i64), ()>,
    /// Test hook, see [`BudgetEngine::fail_next_increment`].
    fail_next_increment: AtomicBool,
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
            dirty: DashMap::new(),
            hard_emitted: DashMap::new(),
            fail_next_increment: AtomicBool::new(false),
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
            dirty: DashMap::new(),
            hard_emitted: DashMap::new(),
            fail_next_increment: AtomicBool::new(false),
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
            let dirty = self.dirty.contains_key(key);
            out.push(match value {
                Some(v) if !dirty => v,
                _ => {
                    if dirty {
                        // The stored value may be stale-low; drop it so the
                        // rebuild below recomputes instead of re-reading it.
                        store.del(std::slice::from_ref(key), bound).await?;
                    }
                    let v = self.rebuild(store, budget, key, now, bound).await?;
                    self.dirty.remove(key);
                    v
                }
            });
        }
        Ok(out)
    }

    /// Test hook: make the next counter increment fail as if the store were
    /// unreachable. Never called by production code.
    #[doc(hidden)]
    pub fn fail_next_increment(&self) {
        self.fail_next_increment.store(true, Ordering::Release);
    }

    /// Pre-call decision for `subject` at `now`. Makes no Redis call when no
    /// enabled budget applies, otherwise exactly one MGET (plus rebuilds on miss).
    pub async fn check(
        &self,
        subject: &BudgetSubject,
        now: DateTime<Utc>,
    ) -> Result<BudgetDecision, BudgetError> {
        let defs = self.definitions().await?;
        let applicable = defs::applicable(&defs, subject.user_id, subject.agent_id);
        if applicable.is_empty() {
            return Ok(BudgetDecision::Allow);
        }
        let spends = self.spend_micros(&applicable, now).await?;
        let entries: Vec<(&Budget, i64)> = applicable.iter().copied().zip(spends).collect();
        Ok(decide(&entries, subject.downgrade, now))
    }

    /// Post-call: add `cost_micros` to every applicable counter that exists.
    /// Never fails the caller; a lost increment invalidates the affected keys.
    pub async fn record(
        &self,
        user_id: Uuid,
        agent_id: Option<Uuid>,
        cost_micros: i64,
        now: DateTime<Utc>,
    ) -> RecordOutcome {
        let mut outcome = RecordOutcome::default();
        if cost_micros <= 0 {
            return outcome;
        }
        let defs = match self.definitions().await {
            Ok(defs) => defs,
            Err(e) => {
                tracing::warn!(%e, "budget record: definitions unavailable, counters not incremented");
                return outcome;
            }
        };
        let applicable: Vec<Budget> = defs::applicable(&defs, Some(user_id), agent_id)
            .into_iter()
            .cloned()
            .collect();
        let Some(store) = self.store.as_ref().filter(|_| !applicable.is_empty()) else {
            return outcome;
        };
        let keys: Vec<String> = applicable
            .iter()
            .map(|b| spend_key(b.id, period_bounds(b.period, now).0))
            .collect();
        let result = if self.fail_next_increment.swap(false, Ordering::AcqRel) {
            Err(StoreError("injected increment failure".into()))
        } else {
            store
                .incr_if_exists(&keys, cost_micros, StoreBound::Record)
                .await
        };
        match result {
            Ok(values) => {
                for (budget, value) in applicable.into_iter().zip(values) {
                    if value < 0 {
                        outcome.missing.push(budget);
                    } else {
                        outcome.applied.push((budget, value, cost_micros));
                    }
                }
                // A crossing is only observable here: the Lua increment returns the
                // post-value, so `pre = post - delta`. Rebuilt counters (`missing`) cannot
                // show a crossing; their shared rebuild emits what it finds instead.
                for (budget, post, delta) in &outcome.applied {
                    let kinds = crossings(budget, post - delta, *post);
                    if !kinds.is_empty() {
                        let start = period_bounds(budget.period, now).0;
                        self.emit_events(budget, start, *post, &kinds).await;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(%e, "budget record: increment failed, invalidating counters");
                if store.del(&keys, StoreBound::Record).await.is_err() {
                    for key in &keys {
                        self.dirty.insert(key.clone(), ());
                    }
                }
                outcome.failed = applicable;
            }
        }
        outcome
    }

    /// Post-INSERT reconciliation: rebuild counters that were missing (the SUM
    /// now includes the new row, so they are not also incremented) and counters
    /// whose increment failed.
    pub async fn finish_record(&self, outcome: RecordOutcome, now: DateTime<Utc>) {
        if let Some(store) = &self.store {
            // The first invalidation may have raced a concurrent rebuild that ran
            // before the row committed; drop the key again now that it has.
            let keys: Vec<String> = outcome
                .failed
                .iter()
                .map(|b| spend_key(b.id, period_bounds(b.period, now).0))
                .collect();
            if store.del(&keys, StoreBound::Record).await.is_err() {
                for key in keys {
                    self.dirty.insert(key, ());
                }
            }
        }
        let to_rebuild: Vec<&Budget> = outcome.missing.iter().chain(&outcome.failed).collect();
        if to_rebuild.is_empty() {
            return;
        }
        if let Err(e) = self
            .spend_micros_bound(&to_rebuild, now, StoreBound::Record)
            .await
        {
            tracing::warn!(%e, "budget finish_record: rebuild failed");
        }
    }

    /// Make sure a `hard_limit` event exists for a budget that just blocked a
    /// call, even when its counter was never observed crossing the limit (e.g.
    /// restored or materialized elsewhere). Idempotent across replicas via the
    /// table's UNIQUE constraint; once per budget-period per process.
    async fn ensure_hard_limit_event(&self, info: &ExceededInfo, now: DateTime<Utc>) {
        let period_start = period_bounds(info.period, now).0;
        let marker = (info.budget_id, period_start.timestamp());
        if self.hard_emitted.contains_key(&marker) {
            return;
        }
        let defs = match self.definitions().await {
            Ok(defs) => defs,
            Err(e) => {
                tracing::warn!(budget_id = %info.budget_id, %e, "ensure_hard_limit_event: definitions unavailable");
                return;
            }
        };
        let Some(budget) = defs.iter().find(|b| b.id == info.budget_id) else {
            return;
        };
        self.emit_threshold_events(budget, period_start, info.spend_micros)
            .await;
        self.hard_emitted.insert(marker, ());
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
        let kinds = reached(budget, spend_micros);
        self.emit_events(budget, period_start, spend_micros, &kinds)
            .await;
    }

    /// Insert one `budget_events` row per kind. The UNIQUE constraint plus
    /// `ON CONFLICT DO NOTHING` is the exactly-once mechanism across replicas and
    /// concurrent callers (D-06). Failures are logged, never propagated.
    async fn emit_events(
        &self,
        budget: &Budget,
        period_start: DateTime<Utc>,
        spend_micros: i64,
        kinds: &[EventKind],
    ) {
        let Some(db) = &self.db else { return };
        for kind in kinds {
            let res = sqlx::query(
                "INSERT INTO budget_events (budget_id, period_start, kind, spend_usd, limit_usd) \
                 VALUES ($1, $2, $3, $4::float8, $5::float8) \
                 ON CONFLICT (budget_id, period_start, kind) DO NOTHING",
            )
            .bind(budget.id)
            .bind(period_start)
            .bind(kind.as_str())
            .bind(micros_to_usd(spend_micros))
            .bind(micros_to_usd(budget.limit_micros))
            .execute(db)
            .await;
            if let Err(e) = res {
                tracing::warn!(budget_id = %budget.id, kind = kind.as_str(), %e, "budget::record: event insert failed");
            }
        }
    }
}

// ─── events ──────────────────────────────────────────────────────────────────

/// The two durable levels a budget reports; Phase 3 alerting consumes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    SoftThreshold,
    HardLimit,
}

impl EventKind {
    /// The `budget_events.kind` value.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::SoftThreshold => "soft_threshold",
            EventKind::HardLimit => "hard_limit",
        }
    }
}

/// Levels a counter moving from `pre` to `post` crossed (`pre < level <= post`).
pub fn crossings(budget: &Budget, pre: i64, post: i64) -> Vec<EventKind> {
    let mut kinds = Vec::new();
    if pre < budget.soft_micros() && budget.soft_micros() <= post {
        kinds.push(EventKind::SoftThreshold);
    }
    if pre < budget.limit_micros && budget.limit_micros <= post {
        kinds.push(EventKind::HardLimit);
    }
    kinds
}

/// Levels a counter value has already reached; used where no `pre` exists (a
/// freshly rebuilt counter, or a blocked call).
fn reached(budget: &Budget, spend_micros: i64) -> Vec<EventKind> {
    crossings(budget, i64::MIN, spend_micros)
}

// ─── decision ────────────────────────────────────────────────────────────────

/// What the router may do when a `Downgrade` budget is past its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DowngradePolicy {
    /// Chat and responses: a cheaper model can be substituted.
    Allowed,
    /// Embeddings: there is no cheaper model; serve unchanged until the ceiling.
    ServeAsIs,
    /// Pinned or compliance-locked agents: the model must not change, so block.
    BlockInstead,
}

/// Whose spend a call counts against.
#[derive(Debug, Clone, Copy)]
pub struct BudgetSubject {
    pub user_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub downgrade: DowngradePolicy,
}

#[derive(Debug, Clone)]
pub struct ExceededInfo {
    pub budget_id: Uuid,
    pub scope: Scope,
    pub period: Period,
    pub limit_micros: i64,
    pub spend_micros: i64,
    pub resets_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct DowngradeInfo {
    pub budget_id: Uuid,
}

#[derive(Debug, Clone)]
pub enum BudgetDecision {
    Allow,
    Downgrade(DowngradeInfo),
    Block(ExceededInfo),
}

/// Combine every applicable budget's `(budget, spend)` into one decision. Any
/// block beats a downgrade beats allow; among several blocking budgets the one
/// resetting last is reported so `Retry-After` is honest.
pub fn decide(
    entries: &[(&Budget, i64)],
    policy: DowngradePolicy,
    now: DateTime<Utc>,
) -> BudgetDecision {
    let mut block: Option<ExceededInfo> = None;
    let mut downgrade: Option<DowngradeInfo> = None;
    for &(budget, spend) in entries {
        let blocks = match budget.action {
            BudgetAction::Block => spend >= budget.limit_micros,
            BudgetAction::Downgrade => match policy {
                DowngradePolicy::BlockInstead => spend >= budget.limit_micros,
                DowngradePolicy::Allowed | DowngradePolicy::ServeAsIs => {
                    spend >= budget.ceiling_micros()
                }
            },
        };
        if blocks {
            let resets_at = period_bounds(budget.period, now).1;
            if block.as_ref().is_none_or(|b| resets_at > b.resets_at) {
                block = Some(ExceededInfo {
                    budget_id: budget.id,
                    scope: budget.scope,
                    period: budget.period,
                    limit_micros: budget.limit_micros,
                    spend_micros: spend,
                    resets_at,
                });
            }
        } else if budget.action == BudgetAction::Downgrade
            && policy == DowngradePolicy::Allowed
            && spend >= budget.limit_micros
            && downgrade.is_none()
        {
            downgrade = Some(DowngradeInfo {
                budget_id: budget.id,
            });
        }
    }
    match (block, downgrade) {
        (Some(info), _) => BudgetDecision::Block(info),
        (None, Some(info)) => BudgetDecision::Downgrade(info),
        (None, None) => BudgetDecision::Allow,
    }
}

/// Counters touched by one [`BudgetEngine::record`].
#[derive(Debug, Default)]
pub struct RecordOutcome {
    /// Budgets whose counter did not exist; rebuilt by `finish_record`.
    pub missing: Vec<Budget>,
    /// `(budget, value after increment, delta)` for counters that were bumped.
    pub applied: Vec<(Budget, i64, i64)>,
    /// Budgets whose increment failed; their counters were invalidated.
    pub failed: Vec<Budget>,
}

/// Router-facing gate: `Ok` is `Allow` or `Downgrade`; a block or an unreadable
/// counter store becomes the dialect-correct refusal. An error here means at
/// least one enabled budget may apply, so it fails closed (ENF-04).
pub async fn enforce(
    engine: &BudgetEngine,
    subject: &BudgetSubject,
    format: InboundFormat,
) -> Result<BudgetDecision, GatewayError> {
    let now = Utc::now();
    match engine.check(subject, now).await {
        Ok(BudgetDecision::Block(info)) => {
            engine.ensure_hard_limit_event(&info, now).await;
            let remaining_ms = (info.resets_at - now).num_milliseconds().max(0) as u64;
            let retry_after_secs = remaining_ms.div_ceil(1000).max(1);
            Err(GatewayError::BudgetDenied(Box::new(BudgetDenial {
                kind: DenialKind::Exceeded(info),
                format,
                retry_after_secs,
            })))
        }
        Ok(decision) => Ok(decision),
        Err(e) => {
            tracing::error!(error = %e, "budget::enforce: counter store unavailable, failing closed");
            Err(GatewayError::BudgetDenied(Box::new(BudgetDenial {
                kind: DenialKind::StoreUnavailable,
                format,
                retry_after_secs: 1,
            })))
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

    fn entry(budget: &Budget, spend: i64) -> (&Budget, i64) {
        (budget, spend)
    }

    fn is_block(d: &BudgetDecision) -> bool {
        matches!(d, BudgetDecision::Block(_))
    }

    #[test]
    fn block_budget_blocks_at_limit() {
        let b = budget(BudgetAction::Block);
        let at = |spend| decide(&[entry(&b, spend)], DowngradePolicy::Allowed, noon());
        assert!(matches!(at(3_999_999), BudgetDecision::Allow));
        assert!(is_block(&at(4_000_000)));
    }

    #[test]
    fn downgrade_budget_with_allowed_policy() {
        let b = budget(BudgetAction::Downgrade);
        let at = |spend| decide(&[entry(&b, spend)], DowngradePolicy::Allowed, noon());
        assert!(matches!(at(3_999_999), BudgetDecision::Allow));
        assert!(matches!(at(4_000_000), BudgetDecision::Downgrade(_)));
        assert!(matches!(at(4_999_999), BudgetDecision::Downgrade(_)));
        assert!(is_block(&at(5_000_000)));
    }

    #[test]
    fn serve_as_is_ignores_downgrade_until_ceiling() {
        let b = budget(BudgetAction::Downgrade);
        let at = |spend| decide(&[entry(&b, spend)], DowngradePolicy::ServeAsIs, noon());
        assert!(matches!(at(4_500_000), BudgetDecision::Allow));
        assert!(is_block(&at(5_000_000)));
    }

    #[test]
    fn block_instead_blocks_at_limit() {
        let b = budget(BudgetAction::Downgrade);
        let at = |spend| decide(&[entry(&b, spend)], DowngradePolicy::BlockInstead, noon());
        assert!(matches!(at(3_999_999), BudgetDecision::Allow));
        assert!(is_block(&at(4_000_000)));
    }

    #[test]
    fn block_beats_downgrade_beats_allow() {
        let down = budget(BudgetAction::Downgrade);
        let block = budget(BudgetAction::Block);
        let ok = budget(BudgetAction::Block);
        let policy = DowngradePolicy::Allowed;
        let d = decide(&[entry(&ok, 0), entry(&down, 4_000_000)], policy, noon());
        assert!(matches!(d, BudgetDecision::Downgrade(_)));
        let d = decide(
            &[entry(&down, 4_000_000), entry(&block, 4_000_000)],
            policy,
            noon(),
        );
        assert!(is_block(&d));
        let d = decide(&[entry(&ok, 0), entry(&down, 0)], policy, noon());
        assert!(matches!(d, BudgetDecision::Allow));
    }

    #[test]
    fn latest_reset_is_reported_among_blocking_budgets() {
        let daily = budget(BudgetAction::Block);
        let mut monthly = budget(BudgetAction::Block);
        monthly.period = Period::Monthly;
        let d = decide(
            &[entry(&daily, 9_000_000), entry(&monthly, 9_000_000)],
            DowngradePolicy::Allowed,
            noon(),
        );
        let BudgetDecision::Block(info) = d else {
            panic!("expected block");
        };
        assert_eq!(info.budget_id, monthly.id);
        assert_eq!(info.period, Period::Monthly);
        assert_eq!(info.resets_at, period_bounds(Period::Monthly, noon()).1);
    }

    #[test]
    fn exceeded_info_carries_budget_numbers() {
        let b = budget(BudgetAction::Block);
        let BudgetDecision::Block(info) =
            decide(&[entry(&b, 4_200_000)], DowngradePolicy::Allowed, noon())
        else {
            panic!("expected block");
        };
        assert_eq!(info.limit_micros, 4_000_000);
        assert_eq!(info.spend_micros, 4_200_000);
        assert_eq!(info.scope, Scope::Platform);
    }

    #[test]
    fn crossings_soft_only() {
        let b = budget(BudgetAction::Block);
        // soft = 3_200_000, limit = 4_000_000
        assert_eq!(
            crossings(&b, 3_100_000, 3_200_000),
            vec![EventKind::SoftThreshold]
        );
        assert_eq!(
            crossings(&b, 3_199_999, 3_999_999),
            vec![EventKind::SoftThreshold]
        );
    }

    #[test]
    fn crossings_hard_only() {
        let b = budget(BudgetAction::Block);
        assert_eq!(
            crossings(&b, 3_900_000, 4_000_000),
            vec![EventKind::HardLimit]
        );
    }

    #[test]
    fn crossings_both_at_once() {
        let b = budget(BudgetAction::Block);
        assert_eq!(
            crossings(&b, 3_000_000, 4_500_000),
            vec![EventKind::SoftThreshold, EventKind::HardLimit]
        );
    }

    #[test]
    fn crossings_none_when_already_past_or_short() {
        let b = budget(BudgetAction::Block);
        assert!(crossings(&b, 0, 3_199_999).is_empty());
        assert!(crossings(&b, 3_200_000, 3_900_000).is_empty());
        assert!(crossings(&b, 4_000_000, 5_000_000).is_empty());
        assert!(crossings(&b, 5_000_000, 5_000_000).is_empty());
    }

    #[test]
    fn reached_reports_every_level_at_or_below_spend() {
        let b = budget(BudgetAction::Block);
        assert!(reached(&b, 3_199_999).is_empty());
        assert_eq!(reached(&b, 3_200_000), vec![EventKind::SoftThreshold]);
        assert_eq!(
            reached(&b, 4_000_000),
            vec![EventKind::SoftThreshold, EventKind::HardLimit]
        );
    }

    #[test]
    fn event_kind_names_match_the_table_check() {
        assert_eq!(EventKind::SoftThreshold.as_str(), "soft_threshold");
        assert_eq!(EventKind::HardLimit.as_str(), "hard_limit");
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
