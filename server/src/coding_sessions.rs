//! Per-agent attribution read API for coding sessions.
//!
//! `GET /api/coding-sessions/{session_id}/agents` groups the session's usage
//! rollup rows (`coding_agent_turn_usage`, one per receipt) into one main row
//! plus one row per subagent or teammate across its resumed runs, and reports
//! whether subagent and team activity was captured at all. It reads Postgres
//! only, so it renders without Tempo. Access follows the session-detail rule:
//! owner or superuser; anyone else gets the same 404 as a missing session.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use nasiko_observability::ObservabilityError;
use nasiko_types::CLAUDE_ADAPTER_VERSION_SUBAGENTS;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;

use crate::auth::Claims;
use crate::observability::service::ObservabilityService;
use crate::state::AppState;

/// The only source whose receipts can carry subagent and teammate scopes.
const CLAUDE_SOURCE: &str = "claude";

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/coding-sessions/{session_id}/agents",
        get(get_session_agents),
    )
}

// ─── response shape ─────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize)]
pub struct Tokens {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
}

impl Tokens {
    fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }
}

#[derive(Debug, Serialize)]
pub struct Totals {
    pub llm_calls: i64,
    pub tool_calls: i64,
    pub tokens: Tokens,
    pub cost_usd: Option<f64>,
    pub cost_estimated: bool,
    pub output_incomplete: bool,
    pub usage_corrected: bool,
}

#[derive(Debug, Serialize)]
pub struct AgentBreakdownRow {
    pub key: String,
    pub kind: String,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub name: Option<String>,
    pub intent: Option<String>,
    pub intent_hidden: bool,
    pub parent_tool_call_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub runs: i64,
    pub llm_calls: i64,
    pub tool_calls: i64,
    pub tokens: Tokens,
    pub cost_usd: Option<f64>,
    pub cost_estimated: bool,
    pub output_incomplete: bool,
    pub usage_corrected: bool,
    pub share: f64,
    /// Whether every run of this row was priced; decides the share basis.
    #[serde(skip)]
    fully_priced: bool,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct SubagentCapture {
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub spawned: Option<usize>,
    pub captured: Option<usize>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct TeamsCapture {
    pub status: &'static str,
    pub reason: Option<&'static str>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Capture {
    pub subagents: SubagentCapture,
    pub teams: TeamsCapture,
}

// ─── queries ────────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct GroupRow {
    agent_kind: String,
    scope_agent_id: Option<String>,
    agent_type: Option<String>,
    agent_display_name: Option<String>,
    parent_tool_call_id: Option<String>,
    parent_agent_id: Option<String>,
    description: Option<String>,
    any_metadata_only: bool,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    runs: i64,
    llm_calls: i64,
    tool_calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    cost_usd: Option<Decimal>,
    any_unpriced: bool,
    cost_estimated: bool,
    output_incomplete: bool,
    usage_corrected: bool,
}

/// Inputs to the capture status. Spawn ids come from main rows that carry the
/// subagent marker only: an unmarked CLI could not capture subagents, so its
/// spawns say nothing about what was captured.
#[derive(Debug, Default, sqlx::FromRow)]
pub struct CaptureInputs {
    pub source_agent_id: Option<String>,
    pub marked_main: i64,
    pub unmarked_main: i64,
    pub unnamed_spawns: Vec<String>,
    pub named_spawns: Vec<String>,
    pub parent_ids: Vec<String>,
}

const GROUP_SQL: &str = r#"
SELECT agent_kind, scope_agent_id,
       MAX(agent_type) AS agent_type,
       MAX(agent_display_name) AS agent_display_name,
       MAX(parent_tool_call_id) AS parent_tool_call_id,
       MAX(parent_agent_id) AS parent_agent_id,
       (array_agg(description ORDER BY started_at)
           FILTER (WHERE description IS NOT NULL))[1] AS description,
       bool_or(capture_policy = 'metadata_only') AS any_metadata_only,
       MIN(started_at) AS started_at,
       MAX(ended_at) AS ended_at,
       COUNT(*) AS runs,
       SUM(llm_calls)::BIGINT AS llm_calls,
       SUM(tool_calls)::BIGINT AS tool_calls,
       SUM(input_tokens)::BIGINT AS input_tokens,
       SUM(output_tokens)::BIGINT AS output_tokens,
       SUM(cache_read_tokens)::BIGINT AS cache_read_tokens,
       SUM(cache_creation_tokens)::BIGINT AS cache_creation_tokens,
       SUM(cost_usd) AS cost_usd,
       bool_or(cost_usd IS NULL) AS any_unpriced,
       COALESCE(bool_or(cost_estimated), false) AS cost_estimated,
       bool_or(output_incomplete) AS output_incomplete,
       bool_or(correction IS NOT NULL) AS usage_corrected
FROM coding_agent_turn_usage
WHERE session_id = $1
GROUP BY agent_kind, scope_agent_id"#;

const CAPTURE_SQL: &str = r#"
WITH u AS (SELECT * FROM coding_agent_turn_usage WHERE session_id = $1),
     marked AS (SELECT * FROM u WHERE agent_kind = 'main' AND adapter_version >= $2)
SELECT
    (SELECT MAX(source_agent_id) FROM u) AS source_agent_id,
    (SELECT COUNT(*) FROM marked) AS marked_main,
    (SELECT COUNT(*) FROM u WHERE agent_kind = 'main'
        AND (adapter_version IS NULL OR adapter_version < $2)) AS unmarked_main,
    ARRAY(SELECT DISTINCT unnest(spawned_agent_call_ids) FROM marked) AS unnamed_spawns,
    ARRAY(SELECT DISTINCT unnest(named_agent_call_ids) FROM marked) AS named_spawns,
    ARRAY(SELECT DISTINCT parent_tool_call_id FROM u
          WHERE agent_kind IN ('subagent', 'teammate')
            AND parent_tool_call_id IS NOT NULL) AS parent_ids"#;

// ─── handler ────────────────────────────────────────────────────────────────

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": message, "code": code}))).into_response()
}

fn internal(session_id: &str, site: &str, e: impl std::fmt::Display) -> Response {
    tracing::warn!(%session_id, error = %e, "get_session_agents: {site} failed");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "internal error",
    )
}

/// Per-agent breakdown and capture status of one coding session.
#[utoipa::path(
    get,
    path = "/api/coding-sessions/{session_id}/agents",
    tag = "coding-sessions",
    params(("session_id" = String, Path, description = "Server session id")),
    responses(
        (status = 200, description = "`{data: {session_id, source_agent_id, share_basis, totals, agents, capture}}`"),
        (status = 404, description = "Session missing or not visible to the caller (`not_found`)"),
    ),
)]
pub(crate) async fn get_session_agents(
    State(state): State<AppState>,
    claims: Claims,
    Path(session_id): Path<String>,
) -> Response {
    match ObservabilityService::from_state(&state)
        .authorize_session_access(&session_id, &claims.sub, claims.is_superuser)
        .await
    {
        Ok(()) => {}
        Err(ObservabilityError::NotFound(_)) => {
            return api_error(StatusCode::NOT_FOUND, "not_found", "session not found");
        }
        Err(e) => return internal(&session_id, "authorize", e),
    }
    let groups: Vec<GroupRow> = match sqlx::query_as(GROUP_SQL)
        .bind(&session_id)
        .fetch_all(&state.db)
        .await
    {
        Ok(groups) => groups,
        Err(e) => return internal(&session_id, "group query", e),
    };
    let inputs: CaptureInputs = match sqlx::query_as(CAPTURE_SQL)
        .bind(&session_id)
        .bind(CLAUDE_ADAPTER_VERSION_SUBAGENTS as i32)
        .fetch_one(&state.db)
        .await
    {
        Ok(inputs) => inputs,
        Err(e) => return internal(&session_id, "capture query", e),
    };

    let mut agents: Vec<AgentBreakdownRow> = groups.into_iter().map(breakdown_row).collect();
    agents.sort_by(|a, b| {
        (a.kind != "main", a.started_at, &a.key).cmp(&(b.kind != "main", b.started_at, &b.key))
    });
    let basis = share_basis(&agents);
    apply_shares(&mut agents, basis);
    let has_teammates = agents.iter().any(|row| row.kind == "teammate");
    Json(json!({"data": {
        "session_id": session_id,
        "source_agent_id": inputs.source_agent_id,
        "share_basis": basis,
        "totals": totals(&agents),
        "capture": capture_status(&inputs, has_teammates),
        "agents": agents,
    }}))
    .into_response()
}

// ─── shaping ────────────────────────────────────────────────────────────────

fn breakdown_row(group: GroupRow) -> AgentBreakdownRow {
    let key = match &group.scope_agent_id {
        Some(id) if group.agent_kind != "main" => format!("{}:{id}", group.agent_kind),
        _ => "main".to_owned(),
    };
    // Task text exists only on content receipts. When it is absent and any run
    // was metadata-only, the UI says so instead of implying there was none.
    let intent_hidden = group.description.is_none() && group.any_metadata_only;
    AgentBreakdownRow {
        key,
        intent: group.description.or_else(|| group.agent_type.clone()),
        intent_hidden,
        kind: group.agent_kind,
        agent_id: group.scope_agent_id,
        agent_type: group.agent_type,
        name: group.agent_display_name,
        parent_tool_call_id: group.parent_tool_call_id,
        parent_agent_id: group.parent_agent_id,
        started_at: group.started_at,
        ended_at: group.ended_at,
        runs: group.runs,
        llm_calls: group.llm_calls,
        tool_calls: group.tool_calls,
        tokens: Tokens {
            input: group.input_tokens,
            output: group.output_tokens,
            cache_read: group.cache_read_tokens,
            cache_creation: group.cache_creation_tokens,
        },
        cost_usd: group.cost_usd.and_then(|cost| cost.to_f64()),
        cost_estimated: group.cost_estimated,
        output_incomplete: group.output_incomplete,
        usage_corrected: group.usage_corrected,
        share: 0.0,
        fully_priced: !group.any_unpriced,
    }
}

/// Shares are of cost when every row is fully priced and there is cost to
/// share; otherwise of tokens, since mixing priced and unpriced rows would
/// understate the unpriced ones.
fn share_basis(agents: &[AgentBreakdownRow]) -> &'static str {
    let total: f64 = agents.iter().filter_map(|row| row.cost_usd).sum();
    if !agents.is_empty() && agents.iter().all(|row| row.fully_priced) && total > 0.0 {
        "cost"
    } else {
        "tokens"
    }
}

fn apply_shares(agents: &mut [AgentBreakdownRow], basis: &str) {
    let weight = |row: &AgentBreakdownRow| match basis {
        "cost" => row.cost_usd.unwrap_or(0.0),
        _ => row.tokens.total() as f64,
    };
    let total: f64 = agents.iter().map(weight).sum();
    for row in agents.iter_mut() {
        row.share = if total > 0.0 {
            weight(row) / total
        } else {
            0.0
        };
    }
}

fn totals(agents: &[AgentBreakdownRow]) -> Totals {
    let costs: Vec<f64> = agents.iter().filter_map(|row| row.cost_usd).collect();
    Totals {
        llm_calls: agents.iter().map(|row| row.llm_calls).sum(),
        tool_calls: agents.iter().map(|row| row.tool_calls).sum(),
        tokens: agents.iter().fold(Tokens::default(), |sum, row| Tokens {
            input: sum.input + row.tokens.input,
            output: sum.output + row.tokens.output,
            cache_read: sum.cache_read + row.tokens.cache_read,
            cache_creation: sum.cache_creation + row.tokens.cache_creation,
        }),
        cost_usd: (!costs.is_empty()).then(|| costs.iter().sum()),
        cost_estimated: agents.iter().any(|row| row.cost_estimated),
        output_incomplete: agents.iter().any(|row| row.output_incomplete),
        usage_corrected: agents.iter().any(|row| row.usage_corrected),
    }
}

/// Captured / partial / not-captured status of subagent and team activity.
///
/// Named spawns that nothing links to may be uncaptured teammates, which the
/// teams status reports, so they are not counted as missing subagents.
pub fn capture_status(inputs: &CaptureInputs, has_teammates: bool) -> Capture {
    if inputs.source_agent_id.as_deref() != Some(CLAUDE_SOURCE) {
        return Capture {
            subagents: SubagentCapture {
                status: "not_applicable",
                reason: Some("agent_unsupported"),
                spawned: None,
                captured: None,
            },
            teams: TeamsCapture {
                status: "not_applicable",
                reason: Some("agent_unsupported"),
            },
        };
    }
    let teams = if has_teammates {
        TeamsCapture {
            status: "captured",
            reason: None,
        }
    } else {
        TeamsCapture {
            status: "not_captured",
            reason: Some("agent_teams_unsupported"),
        }
    };
    if inputs.marked_main == 0 {
        return Capture {
            subagents: SubagentCapture {
                status: "not_captured",
                reason: Some("cli_version"),
                spawned: None,
                captured: None,
            },
            teams,
        };
    }
    let parents: HashSet<&str> = inputs.parent_ids.iter().map(String::as_str).collect();
    let unnamed: HashSet<&str> = inputs.unnamed_spawns.iter().map(String::as_str).collect();
    let named: HashSet<&str> = inputs.named_spawns.iter().map(String::as_str).collect();
    let linked_named = named.intersection(&parents).count();
    let spawned = unnamed.len() + linked_named;
    let captured = unnamed.intersection(&parents).count() + linked_named;
    let (status, reason) = if inputs.unmarked_main > 0 {
        // Activity before the marker may have spawned agents nobody captured.
        ("partial", Some("cli_version_mixed"))
    } else if spawned == captured {
        ("captured", None)
    } else {
        ("partial", None)
    };
    Capture {
        subagents: SubagentCapture {
            status,
            reason,
            spawned: Some(spawned),
            captured: Some(captured),
        },
        teams,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn claude(marked: i64, unmarked: i64) -> CaptureInputs {
        CaptureInputs {
            source_agent_id: Some(CLAUDE_SOURCE.into()),
            marked_main: marked,
            unmarked_main: unmarked,
            ..Default::default()
        }
    }

    #[test]
    fn capture_status_covers_every_reason() {
        let codex = CaptureInputs {
            source_agent_id: Some("codex".into()),
            ..Default::default()
        };
        assert_eq!(
            capture_status(&codex, false).subagents.status,
            "not_applicable"
        );
        let old = capture_status(&claude(0, 2), false);
        assert_eq!(old.subagents.reason, Some("cli_version"));
        assert_eq!(old.teams.reason, Some("agent_teams_unsupported"));

        let mut partial = claude(1, 0);
        partial.unnamed_spawns = ids(&["a", "b"]);
        partial.named_spawns = ids(&["n", "m"]);
        partial.parent_ids = ids(&["a", "m"]);
        let status = capture_status(&partial, true);
        assert_eq!(
            (status.subagents.spawned, status.subagents.captured),
            (Some(3), Some(2))
        );
        assert_eq!(status.subagents.status, "partial");
        assert_eq!(status.teams.status, "captured");

        let mixed = capture_status(&claude(1, 1), false);
        assert_eq!(mixed.subagents.status, "partial");
        assert_eq!(mixed.subagents.reason, Some("cli_version_mixed"));
        assert_eq!(
            capture_status(&claude(1, 0), false).subagents.status,
            "captured"
        );
    }

    fn row(cost: Option<f64>, tokens: i64, fully_priced: bool) -> AgentBreakdownRow {
        AgentBreakdownRow {
            key: "main".into(),
            kind: "main".into(),
            agent_id: None,
            agent_type: None,
            name: None,
            intent: None,
            intent_hidden: false,
            parent_tool_call_id: None,
            parent_agent_id: None,
            started_at: DateTime::UNIX_EPOCH,
            ended_at: DateTime::UNIX_EPOCH,
            runs: 1,
            llm_calls: 1,
            tool_calls: 0,
            tokens: Tokens {
                input: tokens,
                ..Default::default()
            },
            cost_usd: cost,
            cost_estimated: false,
            output_incomplete: false,
            usage_corrected: false,
            share: 0.0,
            fully_priced,
        }
    }

    #[test]
    fn share_basis_falls_back_to_tokens() {
        let mut priced = vec![row(Some(3.0), 10, true), row(Some(1.0), 30, true)];
        assert_eq!(share_basis(&priced), "cost");
        apply_shares(&mut priced, "cost");
        assert_eq!((priced[0].share, priced[1].share), (0.75, 0.25));

        let mut unpriced = vec![row(Some(3.0), 10, true), row(None, 30, false)];
        assert_eq!(share_basis(&unpriced), "tokens");
        apply_shares(&mut unpriced, "tokens");
        assert_eq!((unpriced[0].share, unpriced[1].share), (0.25, 0.75));

        let mut empty = vec![row(Some(0.0), 0, true)];
        assert_eq!(share_basis(&empty), "tokens");
        apply_shares(&mut empty, "tokens");
        assert_eq!(empty[0].share, 0.0);
    }
}
