//! Alert domain types: kinds, severities, scopes, the raise input and the API view.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

/// Severity names in ascending order. The SQL route match uses the same
/// ordering (`array_position`), so keep this list and the engine queries in sync.
pub const SEVERITY_ORDER: [&str; 3] = ["info", "warning", "critical"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    BudgetSoft,
    BudgetHard,
    SpendSpike,
    MonitorBreach,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::BudgetSoft => "budget_soft",
            AlertKind::BudgetHard => "budget_hard",
            AlertKind::SpendSpike => "spend_spike",
            AlertKind::MonitorBreach => "monitor_breach",
        }
    }

    pub fn parse(s: &str) -> Option<AlertKind> {
        match s {
            "budget_soft" => Some(AlertKind::BudgetSoft),
            "budget_hard" => Some(AlertKind::BudgetHard),
            "spend_spike" => Some(AlertKind::SpendSpike),
            "monitor_breach" => Some(AlertKind::MonitorBreach),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => SEVERITY_ORDER[0],
            Severity::Warning => SEVERITY_ORDER[1],
            Severity::Critical => SEVERITY_ORDER[2],
        }
    }

    pub fn parse(s: &str) -> Option<Severity> {
        match s {
            "info" => Some(Severity::Info),
            "warning" => Some(Severity::Warning),
            "critical" => Some(Severity::Critical),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertScope {
    Platform,
    Agent,
    Model,
    User,
}

impl AlertScope {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertScope::Platform => "platform",
            AlertScope::Agent => "agent",
            AlertScope::Model => "model",
            AlertScope::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<AlertScope> {
        match s {
            "platform" => Some(AlertScope::Platform),
            "agent" => Some(AlertScope::Agent),
            "model" => Some(AlertScope::Model),
            "user" => Some(AlertScope::User),
            _ => None,
        }
    }
}

/// What a notification announces about an alert (or a channel test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyEvent {
    Opened,
    Escalated,
    Resolved,
    Test,
}

impl NotifyEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            NotifyEvent::Opened => "opened",
            NotifyEvent::Escalated => "escalated",
            NotifyEvent::Resolved => "resolved",
            NotifyEvent::Test => "test",
        }
    }
}

/// Input to `engine::raise`. `dedup_key` identifies the condition: while an
/// alert with this key is open or acknowledged, raising it again only counts
/// an occurrence.
#[derive(Debug, Clone)]
pub struct NewAlert {
    pub kind: AlertKind,
    pub severity: Severity,
    pub scope: AlertScope,
    pub scope_ref: Option<String>,
    pub dedup_key: String,
    pub title: String,
    pub message: String,
    pub link: String,
    pub details: Value,
}

/// Result of `engine::raise`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaiseOutcome {
    /// A new open alert was created.
    Opened(Uuid),
    /// The live alert was bumped and its severity went up.
    Escalated(Uuid),
    /// The live alert was bumped; nothing else changed.
    Repeated(Uuid),
}

/// Every `alerts` column, as returned by the API.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct AlertView {
    pub id: Uuid,
    /// `budget_soft`, `budget_hard`, `spend_spike` or `monitor_breach`.
    pub kind: String,
    /// `info`, `warning` or `critical`.
    pub severity: String,
    /// `platform`, `agent`, `model` or `user`.
    pub scope: String,
    pub scope_ref: Option<String>,
    pub dedup_key: String,
    /// `open`, `acknowledged` or `resolved`.
    pub status: String,
    pub title: String,
    pub message: String,
    /// In-app path the alert deep-links to.
    pub link: String,
    #[schema(value_type = Object)]
    pub details: Value,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub occurrences: i32,
    pub acknowledged_by: Option<Uuid>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub resolved_at: Option<DateTime<Utc>>,
}

/// Deep link into the TokenOps page, optionally preselecting an agent and range.
pub fn tokenops_link(agent_id: Option<Uuid>, range: Option<&str>) -> String {
    let mut params = Vec::new();
    if let Some(id) = agent_id {
        params.push(format!("agent={id}"));
    }
    if let Some(r) = range {
        params.push(format!("range={r}"));
    }
    if params.is_empty() {
        "/tokenops".to_owned()
    } else {
        format!("/tokenops?{}", params.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_parses_and_orders() {
        assert_eq!(Severity::parse("warning"), Some(Severity::Warning));
        assert_eq!(Severity::parse("nope"), None);
        assert!(Severity::Info < Severity::Warning && Severity::Warning < Severity::Critical);
        for s in [Severity::Info, Severity::Warning, Severity::Critical] {
            assert_eq!(Severity::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn kinds_and_scopes_round_trip() {
        for k in [
            AlertKind::BudgetSoft,
            AlertKind::BudgetHard,
            AlertKind::SpendSpike,
            AlertKind::MonitorBreach,
        ] {
            assert_eq!(AlertKind::parse(k.as_str()), Some(k));
        }
        for s in [
            AlertScope::Platform,
            AlertScope::Agent,
            AlertScope::Model,
            AlertScope::User,
        ] {
            assert_eq!(AlertScope::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn notify_event_names() {
        assert_eq!(NotifyEvent::Opened.as_str(), "opened");
        assert_eq!(NotifyEvent::Escalated.as_str(), "escalated");
        assert_eq!(NotifyEvent::Resolved.as_str(), "resolved");
        assert_eq!(NotifyEvent::Test.as_str(), "test");
    }

    #[test]
    fn tokenops_links() {
        let id = Uuid::nil();
        assert_eq!(tokenops_link(None, None), "/tokenops");
        assert_eq!(
            tokenops_link(Some(id), None),
            format!("/tokenops?agent={id}")
        );
        assert_eq!(
            tokenops_link(Some(id), Some("24h")),
            format!("/tokenops?agent={id}&range=24h")
        );
        assert_eq!(tokenops_link(None, Some("24h")), "/tokenops?range=24h");
    }
}
