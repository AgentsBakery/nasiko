//! Budget definitions: the row model, loaders and the applicability filter.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row, postgres::PgRow};
use uuid::Uuid;

use super::period::Period;

/// Whose spend a budget counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Agent,
    Platform,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Agent => "agent",
            Scope::Platform => "platform",
        }
    }

    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "user" => Some(Scope::User),
            "agent" => Some(Scope::Agent),
            "platform" => Some(Scope::Platform),
            _ => None,
        }
    }
}

/// What the router does once spend reaches the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetAction {
    Block,
    Downgrade,
}

impl BudgetAction {
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetAction::Block => "block",
            BudgetAction::Downgrade => "downgrade",
        }
    }

    pub fn parse(s: &str) -> Option<BudgetAction> {
        match s {
            "block" => Some(BudgetAction::Block),
            "downgrade" => Some(BudgetAction::Downgrade),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Budget {
    pub id: Uuid,
    pub name: String,
    pub scope: Scope,
    pub target_id: Option<Uuid>,
    pub period: Period,
    pub limit_micros: i64,
    pub soft_threshold_pct: i16,
    pub action: BudgetAction,
    pub downgrade_ceiling_pct: i16,
    pub enabled: bool,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn pct_of(micros: i64, pct: i16) -> i64 {
    // i128: limit (up to 1e14 USD * 1e6) times a percentage would overflow i64.
    let v = i128::from(micros) * i128::from(pct) / 100;
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl Budget {
    /// Spend at which the `soft_threshold` event fires.
    pub fn soft_micros(&self) -> i64 {
        pct_of(self.limit_micros, self.soft_threshold_pct)
    }

    /// Spend at which a downgrade budget starts blocking.
    pub fn ceiling_micros(&self) -> i64 {
        pct_of(self.limit_micros, self.downgrade_ceiling_pct)
    }
}

const SELECT_BUDGETS: &str = "SELECT id, name, scope, target_id, period, \
     (limit_usd * 1000000)::bigint AS limit_micros, soft_threshold_pct, action, \
     downgrade_ceiling_pct, enabled, created_by, created_at, updated_at FROM budgets";

fn from_row(row: &PgRow) -> Result<Option<Budget>, sqlx::Error> {
    let id: Uuid = row.try_get("id")?;
    let scope: String = row.try_get("scope")?;
    let period: String = row.try_get("period")?;
    let action: String = row.try_get("action")?;
    let (Some(scope), Some(period), Some(action)) = (
        Scope::parse(&scope),
        Period::parse(&period),
        BudgetAction::parse(&action),
    ) else {
        // Unreachable while the table CHECKs hold; skip rather than fail every call.
        tracing::warn!(%id, %scope, %period, %action, "budget from_row: unknown enum value, skipping");
        return Ok(None);
    };
    Ok(Some(Budget {
        id,
        name: row.try_get("name")?,
        scope,
        target_id: row.try_get("target_id")?,
        period,
        limit_micros: row.try_get("limit_micros")?,
        soft_threshold_pct: row.try_get("soft_threshold_pct")?,
        action,
        downgrade_ceiling_pct: row.try_get("downgrade_ceiling_pct")?,
        enabled: row.try_get("enabled")?,
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    }))
}

/// All budgets ordered by creation, optionally only the enabled ones.
pub async fn load_budgets(db: &PgPool, only_enabled: bool) -> Result<Vec<Budget>, sqlx::Error> {
    let sql = if only_enabled {
        format!("{SELECT_BUDGETS} WHERE enabled ORDER BY created_at, id")
    } else {
        format!("{SELECT_BUDGETS} ORDER BY created_at, id")
    };
    let rows = sqlx::query(&sql).fetch_all(db).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Some(b) = from_row(row)? {
            out.push(b);
        }
    }
    Ok(out)
}

pub async fn load_budget(db: &PgPool, id: Uuid) -> Result<Option<Budget>, sqlx::Error> {
    let sql = format!("{SELECT_BUDGETS} WHERE id = $1");
    match sqlx::query(&sql).bind(id).fetch_optional(db).await? {
        Some(row) => from_row(&row),
        None => Ok(None),
    }
}

/// Budgets that count a call by `user_id` / `agent_id`. Platform budgets always
/// apply; a missing id matches nothing but platform.
pub fn applicable(
    budgets: &[Budget],
    user_id: Option<Uuid>,
    agent_id: Option<Uuid>,
) -> Vec<&Budget> {
    budgets
        .iter()
        .filter(|b| match b.scope {
            Scope::Platform => true,
            Scope::User => user_id.is_some() && b.target_id == user_id,
            Scope::Agent => agent_id.is_some() && b.target_id == agent_id,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(scope: Scope, target: Option<Uuid>) -> Budget {
        Budget {
            id: Uuid::new_v4(),
            name: "b".into(),
            scope,
            target_id: target,
            period: Period::Monthly,
            limit_micros: 4_000_000,
            soft_threshold_pct: 80,
            action: BudgetAction::Block,
            downgrade_ceiling_pct: 125,
            enabled: true,
            created_by: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn applicable_filters_by_scope_and_target() {
        let (u, a) = (Uuid::new_v4(), Uuid::new_v4());
        let all = vec![
            budget(Scope::Platform, None),
            budget(Scope::User, Some(u)),
            budget(Scope::User, Some(Uuid::new_v4())),
            budget(Scope::Agent, Some(a)),
            budget(Scope::Agent, Some(Uuid::new_v4())),
        ];
        assert_eq!(applicable(&all, Some(u), Some(a)).len(), 3);
        assert_eq!(applicable(&all, Some(u), None).len(), 2);
        let only_platform = applicable(&all, None, None);
        assert_eq!(only_platform.len(), 1);
        assert_eq!(only_platform[0].scope, Scope::Platform);
    }

    #[test]
    fn thresholds_do_not_overflow() {
        let mut b = budget(Scope::Platform, None);
        assert_eq!(b.soft_micros(), 3_200_000);
        assert_eq!(b.ceiling_micros(), 5_000_000);
        b.limit_micros = i64::MAX;
        assert_eq!(b.ceiling_micros(), i64::MAX);
    }
}
