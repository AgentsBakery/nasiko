//! Request/response shapes and input validation for `/api/budgets`.

use chrono::{DateTime, Utc};
use nasiko_llm_router::budget::defs::{Budget, BudgetAction, Scope};
use nasiko_llm_router::budget::keys::micros_to_usd;
use nasiko_llm_router::budget::period::Period;
use nasiko_llm_router::budget::{BudgetStatus, status_of};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

pub const DEFAULT_SOFT_THRESHOLD_PCT: i64 = 80;
pub const DEFAULT_DOWNGRADE_CEILING_PCT: i64 = 125;
const MIN_LIMIT_USD: f64 = 0.0001;
/// `limit_usd` is NUMERIC(14,4); stay far below its range.
const MAX_LIMIT_USD: f64 = 1_000_000_000.0;
const MAX_CEILING_PCT: i64 = 10_000;
const MAX_NAME_LEN: usize = 200;

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateBudgetRequest {
    pub name: String,
    /// `user`, `agent` or `platform`.
    pub scope: String,
    pub target_id: Option<Uuid>,
    /// `daily`, `weekly` or `monthly` (UTC-aligned).
    pub period: String,
    pub limit_usd: f64,
    pub soft_threshold_pct: Option<i64>,
    /// `block` or `downgrade`.
    pub action: String,
    pub downgrade_ceiling_pct: Option<i64>,
    pub enabled: Option<bool>,
}

/// Partial update. `scope` and `target_id` are accepted only so a change can be
/// rejected (`scope_immutable`): the counter key would otherwise count someone
/// else's spend.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct UpdateBudgetRequest {
    pub name: Option<String>,
    pub scope: Option<String>,
    pub target_id: Option<Uuid>,
    pub period: Option<String>,
    pub limit_usd: Option<f64>,
    pub soft_threshold_pct: Option<i64>,
    pub action: Option<String>,
    pub downgrade_ceiling_pct: Option<i64>,
    pub enabled: Option<bool>,
}

/// A budget with its live status. Admin responses carry every field; `/me`
/// redacts platform rows for non-admins through [`BudgetView::to_json`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BudgetView {
    pub id: Uuid,
    pub name: String,
    pub scope: String,
    pub target_id: Option<Uuid>,
    pub period: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_usd: Option<f64>,
    pub soft_threshold_pct: i16,
    pub action: String,
    pub downgrade_ceiling_pct: i16,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `null` when the counter store was unavailable or the budget is disabled.
    pub spend_usd: Option<f64>,
    pub pct_used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_usd: Option<f64>,
    pub period_start: DateTime<Utc>,
    pub resets_at: DateTime<Utc>,
    /// `ok`, `soft`, `downgrading`, `blocked`, `disabled` or `unknown`.
    pub state: String,
}

impl BudgetView {
    pub fn new(budget: &Budget, spend_micros: Option<i64>, now: DateTime<Utc>) -> Self {
        let BudgetStatus {
            period_start,
            resets_at,
            spend_micros,
            pct_used,
            projected_micros,
            state,
        } = status_of(budget, spend_micros, now);
        Self {
            id: budget.id,
            name: budget.name.clone(),
            scope: budget.scope.as_str().into(),
            target_id: budget.target_id,
            period: budget.period.as_str().into(),
            limit_usd: Some(micros_to_usd(budget.limit_micros)),
            soft_threshold_pct: budget.soft_threshold_pct,
            action: budget.action.as_str().into(),
            downgrade_ceiling_pct: budget.downgrade_ceiling_pct,
            enabled: budget.enabled,
            created_at: budget.created_at,
            updated_at: budget.updated_at,
            spend_usd: spend_micros.map(micros_to_usd),
            pct_used,
            projected_usd: projected_micros.map(micros_to_usd),
            period_start,
            resets_at,
            state: state.as_str().into(),
        }
    }

    /// JSON for the wire. With `redact`, the dollar amounts are dropped (keys
    /// absent, not null) so a non-admin cannot read platform-wide spend.
    pub fn to_json(&self, redact: bool) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if redact && let Value::Object(map) = &mut v {
            for k in ["limit_usd", "spend_usd", "projected_usd"] {
                map.remove(k);
            }
        }
        v
    }
}

/// Validation failure: a stable slug and a human message.
pub type Invalid = (&'static str, String);

/// Validate a fully specified budget. Target existence is checked by the
/// handler (it needs the DB).
pub fn validate_create(req: &CreateBudgetRequest) -> Result<(), Invalid> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err((
            "invalid_name",
            format!("name must be 1-{MAX_NAME_LEN} characters"),
        ));
    }
    let scope = Scope::parse(&req.scope).ok_or((
        "invalid_scope",
        "scope must be user, agent or platform".to_string(),
    ))?;
    if Period::parse(&req.period).is_none() {
        return Err((
            "invalid_period",
            "period must be daily, weekly or monthly".into(),
        ));
    }
    if BudgetAction::parse(&req.action).is_none() {
        return Err(("invalid_action", "action must be block or downgrade".into()));
    }
    if !req.limit_usd.is_finite() || !(MIN_LIMIT_USD..=MAX_LIMIT_USD).contains(&req.limit_usd) {
        return Err((
            "invalid_limit",
            format!("limit_usd must be between {MIN_LIMIT_USD} and {MAX_LIMIT_USD}"),
        ));
    }
    let soft = req.soft_threshold_pct.unwrap_or(DEFAULT_SOFT_THRESHOLD_PCT);
    if !(1..=100).contains(&soft) {
        return Err((
            "invalid_threshold",
            "soft_threshold_pct must be between 1 and 100".into(),
        ));
    }
    let ceiling = req
        .downgrade_ceiling_pct
        .unwrap_or(DEFAULT_DOWNGRADE_CEILING_PCT);
    if !(100..=MAX_CEILING_PCT).contains(&ceiling) {
        return Err((
            "invalid_ceiling",
            format!("downgrade_ceiling_pct must be between 100 and {MAX_CEILING_PCT}"),
        ));
    }
    match (scope, req.target_id) {
        (Scope::Platform, Some(_)) => Err((
            "target_not_allowed",
            "platform budgets have no target_id".into(),
        )),
        (Scope::User | Scope::Agent, None) => Err((
            "target_required",
            "user and agent budgets require target_id".into(),
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> CreateBudgetRequest {
        CreateBudgetRequest {
            name: "b".into(),
            scope: "user".into(),
            target_id: Some(Uuid::new_v4()),
            period: "monthly".into(),
            limit_usd: 10.0,
            soft_threshold_pct: None,
            action: "block".into(),
            downgrade_ceiling_pct: None,
            enabled: None,
        }
    }

    fn code(req: CreateBudgetRequest) -> &'static str {
        validate_create(&req).err().map(|e| e.0).unwrap_or("ok")
    }

    #[test]
    fn accepts_a_valid_request() {
        assert_eq!(code(valid()), "ok");
    }

    #[test]
    fn rejects_each_bad_field_with_its_slug() {
        let mut r = valid();
        r.name = "  ".into();
        assert_eq!(code(r), "invalid_name");
        let mut r = valid();
        r.scope = "team".into();
        assert_eq!(code(r), "invalid_scope");
        let mut r = valid();
        r.period = "yearly".into();
        assert_eq!(code(r), "invalid_period");
        let mut r = valid();
        r.action = "notify".into();
        assert_eq!(code(r), "invalid_action");
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e12] {
            let mut r = valid();
            r.limit_usd = bad;
            assert_eq!(code(r), "invalid_limit");
        }
        for bad in [0, 101] {
            let mut r = valid();
            r.soft_threshold_pct = Some(bad);
            assert_eq!(code(r), "invalid_threshold");
        }
        let mut r = valid();
        r.downgrade_ceiling_pct = Some(99);
        assert_eq!(code(r), "invalid_ceiling");
    }

    #[test]
    fn target_must_match_scope() {
        let mut r = valid();
        r.target_id = None;
        assert_eq!(code(r), "target_required");
        let mut r = valid();
        r.scope = "platform".into();
        assert_eq!(code(r), "target_not_allowed");
        let mut r = valid();
        r.scope = "platform".into();
        r.target_id = None;
        assert_eq!(code(r), "ok");
    }
}
