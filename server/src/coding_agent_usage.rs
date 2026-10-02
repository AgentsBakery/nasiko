//! Per-receipt coding-agent usage rollup (`coding_agent_turn_usage`).
//!
//! Every accepted coding-agent receipt gets exactly one derived row holding its
//! token classes, call counts, cost, agent scope (main, subagent, teammate) and
//! any read-time correction. Receipts and `trace_usage` are never rewritten:
//! receipts are immutable and `trace_usage` is re-upserted from Tempo spans, so
//! corrections live only here and are applied by readers (see the
//! `trace_usage_corrected` view in migration 0050).
//!
//! Subagent work arrives as its own scoped receipts and therefore as its own
//! rows. Nothing here folds a subagent's usage into its parent, so summing rows
//! counts every token exactly once.

use chrono::{DateTime, Utc};
use nasiko_pricing::{CostBreakdown, PricingContext, PricingEngine, PromptConvention, RawUsage};
use nasiko_types::{
    CapturePolicy, CodingAgentEventV1, CodingAgentLlmCall, CodingAgentScopeKind,
    CodingAgentToolCallStatus,
};
use rust_decimal::Decimal;
use sqlx::{PgPool, Postgres, Transaction};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

/// Correction tag for legacy Codex receipts whose input included cache reads.
pub const CODEX_INCLUSIVE_INPUT_CORRECTION: &str = "codex_inclusive_input_v1";

const CODEX_SOURCE_AGENT_ID: &str = "codex";

/// Usage derived from one receipt, mirroring a `coding_agent_turn_usage` row.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnUsageRollup {
    /// Server (scoped) session id, as stored in `chat_sessions`.
    pub session_id: String,
    /// Trace id the OTLP exporter uses, so rows join `trace_usage.trace_id`.
    pub trace_id: String,
    pub source_agent_id: String,
    pub capture_policy: &'static str,
    pub adapter_version: Option<u32>,
    pub agent_kind: &'static str,
    pub scope_agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub parent_tool_call_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub spawn_depth: Option<u32>,
    /// Content only; `None` for metadata-only receipts.
    pub description: Option<String>,
    /// Content only; `None` for metadata-only receipts.
    pub agent_display_name: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub llm_calls: u32,
    pub tool_calls: u32,
    /// Corrected (cache-exclusive) input.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Input as the receipt reported it.
    pub reported_input_tokens: u64,
    /// Cost of the corrected usage; `None` until priced or when unpriceable.
    pub cost_usd: Option<Decimal>,
    /// Cost of the reported usage.
    pub reported_cost_usd: Option<Decimal>,
    pub cost_estimated: Option<bool>,
    /// True when any call's output count is only a lower bound.
    pub output_incomplete: bool,
    /// Agent/Task tool calls that launched an (unnamed) subagent.
    pub spawned_agent_call_ids: Vec<String>,
    /// Agent/Task tool calls carrying a `name`: a teammate when agent teams are
    /// enabled, an ordinary named subagent otherwise. Not classifiable at ingest.
    pub named_agent_call_ids: Vec<String>,
    pub correction: Option<&'static str>,
}

/// Whether a receipt is a legacy Codex receipt whose input includes cache reads.
///
/// Codex reports `input_tokens` inclusive of cached input; CLIs before
/// `CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT` forwarded it unchanged, so cache
/// reads were counted twice. The rule keys only on the adapter marker. Value
/// heuristics such as `input >= cache_read` are forbidden: a fixed CLI's
/// exclusive input can legitimately exceed its cache reads, and subtracting
/// again would undercount real spend.
pub fn is_legacy_codex_inclusive(event: &CodingAgentEventV1) -> bool {
    event.source.agent_id == CODEX_SOURCE_AGENT_ID && event.source.adapter_version.is_none()
}

/// Agent/Task tool names that launch another agent.
const SPAWN_TOOL_NAMES: &[&str] = &["Agent", "Task"];

/// Tool-call outcomes after which no agent was launched.
const NOT_SPAWNED_STATUSES: &[CodingAgentToolCallStatus] = &[
    CodingAgentToolCallStatus::Failed,
    CodingAgentToolCallStatus::Denied,
    CodingAgentToolCallStatus::TimedOut,
    CodingAgentToolCallStatus::Cancelled,
];

fn corrected_input(call: &CodingAgentLlmCall, legacy_inclusive: bool) -> u64 {
    if legacy_inclusive {
        call.input_tokens.saturating_sub(call.cache_read_tokens)
    } else {
        call.input_tokens
    }
}

fn sum_calls(calls: &[CodingAgentLlmCall], tokens: impl Fn(&CodingAgentLlmCall) -> u64) -> u64 {
    calls
        .iter()
        .fold(0_u64, |total, call| total.saturating_add(tokens(call)))
}

fn saturating_count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// Build the rollup for one receipt. Pure; costs are filled by [`price_rollup`].
///
/// `server_session_id` is the scoped session id the receipt is stored under;
/// the trace id is derived from it exactly as the OTLP exporter does, so rows
/// join `trace_usage.trace_id`.
pub fn rollup_for_event(event: &CodingAgentEventV1, server_session_id: &str) -> TurnUsageRollup {
    let legacy_inclusive = is_legacy_codex_inclusive(event);
    let is_content = event.capture_policy == CapturePolicy::Content;
    let calls = &event.turn.llm_calls;

    let mut spawned_agent_call_ids = Vec::new();
    let mut named_agent_call_ids = Vec::new();
    for tool_call in &event.turn.tool_calls {
        if !SPAWN_TOOL_NAMES.contains(&tool_call.name.as_str())
            || NOT_SPAWNED_STATUSES.contains(&tool_call.status)
        {
            continue;
        }
        // Metadata-only receipts carry no arguments, so every spawn lands in
        // the unnamed list there; that is a documented limitation.
        let named = tool_call
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("name"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| !name.trim().is_empty());
        if named {
            named_agent_call_ids.push(tool_call.id.clone());
        } else {
            spawned_agent_call_ids.push(tool_call.id.clone());
        }
    }

    let scope = event.turn.agent_scope.as_ref();
    let agent_kind = match scope.map(|scope| scope.kind) {
        None => "main",
        Some(CodingAgentScopeKind::Subagent) => "subagent",
        Some(CodingAgentScopeKind::Teammate) => "teammate",
        Some(CodingAgentScopeKind::Unknown) => "unknown",
    };
    // Description and display name are content: never copied from a
    // metadata-only receipt (validation already rejects them there; this keeps
    // the derived row safe even if that ever changes).
    let content_only = |value: Option<&String>| value.filter(|_| is_content).cloned();

    TurnUsageRollup {
        session_id: server_session_id.to_owned(),
        trace_id: trace_id_for_server_session(event, server_session_id),
        source_agent_id: event.source.agent_id.clone(),
        capture_policy: if is_content {
            "content"
        } else {
            "metadata_only"
        },
        adapter_version: event.source.adapter_version,
        agent_kind,
        scope_agent_id: scope.map(|scope| scope.agent_id.clone()),
        agent_type: scope.and_then(|scope| scope.agent_type.clone()),
        parent_tool_call_id: scope.and_then(|scope| scope.parent_tool_call_id.clone()),
        parent_agent_id: scope.and_then(|scope| scope.parent_agent_id.clone()),
        spawn_depth: scope.and_then(|scope| scope.spawn_depth),
        description: content_only(scope.and_then(|scope| scope.description.as_ref())),
        agent_display_name: content_only(scope.and_then(|scope| scope.name.as_ref())),
        started_at: event.turn.started_at,
        ended_at: event.turn.ended_at,
        llm_calls: saturating_count(calls.len()),
        tool_calls: saturating_count(event.turn.tool_calls.len()),
        input_tokens: sum_calls(calls, |call| corrected_input(call, legacy_inclusive)),
        output_tokens: sum_calls(calls, |call| call.output_tokens),
        cache_read_tokens: sum_calls(calls, |call| call.cache_read_tokens),
        cache_creation_tokens: sum_calls(calls, |call| call.cache_creation_tokens),
        reported_input_tokens: sum_calls(calls, |call| call.input_tokens),
        cost_usd: None,
        reported_cost_usd: None,
        cost_estimated: None,
        output_incomplete: calls.iter().any(|call| {
            call.accounting
                .as_ref()
                .is_some_and(|accounting| accounting.output_tokens_final == Some(false))
        }),
        spawned_agent_call_ids,
        named_agent_call_ids,
        correction: legacy_inclusive.then_some(CODEX_INCLUSIVE_INPUT_CORRECTION),
    }
}

/// The OTLP trace id for a receipt stored under `server_session_id`.
///
/// The exporter derives trace ids from the server (scoped) session id, not the
/// client's, so the event is re-keyed first. Content fields are dropped from
/// the clone because the id never depends on them.
fn trace_id_for_server_session(event: &CodingAgentEventV1, server_session_id: &str) -> String {
    let keyed = CodingAgentEventV1 {
        version: event.version,
        event_id: event.event_id.clone(),
        captured_at: event.captured_at,
        source: event.source.clone(),
        session: nasiko_types::CodingAgentSession {
            id: server_session_id.to_owned(),
            source_id: event.session.source_id.clone(),
            title: None,
        },
        turn: nasiko_types::CodingAgentTurn {
            id: event.turn.id.clone(),
            prompt: None,
            response: None,
            started_at: event.turn.started_at,
            ended_at: event.turn.ended_at,
            llm_calls: Vec::new(),
            tool_calls: Vec::new(),
            agent_scope: None,
        },
        capture_policy: event.capture_policy.clone(),
    };
    crate::coding_agent_otlp::trace_id_for_event(&keyed)
}

/// Price one call's usage with `input` substituted for its reported input.
async fn price_call(
    pricing: &PricingEngine,
    call: &CodingAgentLlmCall,
    input: u64,
) -> CostBreakdown {
    let context = call
        .accounting
        .as_ref()
        .map(|accounting| PricingContext {
            cache_creation_5m: accounting.cache_creation_5m_tokens,
            cache_creation_1h: accounting.cache_creation_1h_tokens,
            speed: accounting.speed.as_deref(),
            service_tier: accounting.service_tier.as_deref(),
            inference_geo: accounting.inference_geo.as_deref(),
            conflicting_observations: accounting.conflicting_observations,
        })
        .unwrap_or_default();
    pricing
        .price_with_context(
            Some(&call.provider),
            &call.model,
            RawUsage {
                input,
                output: call.output_tokens,
                cache_read: call.cache_read_tokens,
                cache_creation: call.cache_creation_tokens,
                total: None,
            },
            // Coding-agent usage is cache-exclusive once corrected, which is
            // Anthropic's convention and what the adapters normalize to.
            PromptConvention::Exclusive,
            call.started_at,
            context,
        )
        .await
        .cost
}

/// Round to the pricing engine's micro-dollar precision, keeping real zeros.
fn usd(total: &CostBreakdown) -> Option<Decimal> {
    Decimal::from_f64_retain(total.total_usd).map(|cost| cost.round_dp(6))
}

/// Price the rollup's reported and (when corrected) corrected usage.
///
/// Priced per call, not per turn: a turn can switch models mid-way, and one
/// model's rate must not be applied to another model's tokens. A turn with no
/// calls stays unpriced (`None`), which is distinct from a priced zero.
/// Pricing takes its own pool connection, so never call this while holding a
/// write transaction.
pub async fn price_rollup(
    pricing: &PricingEngine,
    event: &CodingAgentEventV1,
    rollup: &mut TurnUsageRollup,
) {
    if event.turn.llm_calls.is_empty() {
        rollup.cost_usd = None;
        rollup.reported_cost_usd = None;
        rollup.cost_estimated = None;
        return;
    }
    let corrected = rollup.correction.is_some();
    let mut reported = CostBreakdown::default();
    let mut actual = CostBreakdown::default();
    for call in &event.turn.llm_calls {
        let reported_cost = price_call(pricing, call, call.input_tokens).await;
        if corrected {
            actual.add(price_call(pricing, call, corrected_input(call, true)).await);
        } else {
            actual.add(reported_cost);
        }
        reported.add(reported_cost);
    }
    rollup.reported_cost_usd = usd(&reported);
    rollup.cost_usd = usd(&actual);
    rollup.cost_estimated = Some(reported.estimated || actual.estimated);
}

/// The reported cost and estimate flag the chat transcript stores for a turn.
///
/// Chat keeps the receipt's reported figure (today's semantics); read paths
/// apply the correction overlay.
pub fn reported_cost_for_chat(rollup: &TurnUsageRollup) -> Option<(Decimal, bool)> {
    rollup
        .reported_cost_usd
        .map(|cost| (cost, rollup.cost_estimated.unwrap_or(false)))
}

impl TurnUsageRollup {
    /// Re-key the rollup to the server session the receipt is stored under.
    ///
    /// Ingest prices the rollup before its write transaction, when the server
    /// session id (which needs the agent lookup) is not yet known.
    pub fn assign_session(&mut self, event: &CodingAgentEventV1, server_session_id: &str) {
        self.session_id = server_session_id.to_owned();
        self.trace_id = trace_id_for_server_session(event, server_session_id);
    }
}

// ─── persistence ────────────────────────────────────────────────────────────

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn saturating_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Insert the rollup row for a receipt. Idempotent: an existing row wins.
pub async fn insert_rollup(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    event_id: &str,
    rollup: &TurnUsageRollup,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO coding_agent_turn_usage
             (user_id, event_id, session_id, trace_id, source_agent_id, capture_policy,
              adapter_version, agent_kind, scope_agent_id, agent_type, parent_tool_call_id,
              parent_agent_id, spawn_depth, description, agent_display_name, started_at,
              ended_at, llm_calls, tool_calls, input_tokens, output_tokens,
              cache_read_tokens, cache_creation_tokens, reported_input_tokens, cost_usd,
              reported_cost_usd, cost_estimated, output_incomplete, spawned_agent_call_ids,
              named_agent_call_ids, correction)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                   $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30, $31)
           ON CONFLICT (user_id, event_id) DO NOTHING"#,
    )
    .bind(user_id)
    .bind(event_id)
    .bind(&rollup.session_id)
    .bind(&rollup.trace_id)
    .bind(&rollup.source_agent_id)
    .bind(rollup.capture_policy)
    .bind(rollup.adapter_version.map(saturating_i32))
    .bind(rollup.agent_kind)
    .bind(&rollup.scope_agent_id)
    .bind(&rollup.agent_type)
    .bind(&rollup.parent_tool_call_id)
    .bind(&rollup.parent_agent_id)
    .bind(rollup.spawn_depth.map(saturating_i32))
    .bind(&rollup.description)
    .bind(&rollup.agent_display_name)
    .bind(rollup.started_at)
    .bind(rollup.ended_at)
    .bind(saturating_i32(rollup.llm_calls))
    .bind(saturating_i32(rollup.tool_calls))
    .bind(saturating_i64(rollup.input_tokens))
    .bind(saturating_i64(rollup.output_tokens))
    .bind(saturating_i64(rollup.cache_read_tokens))
    .bind(saturating_i64(rollup.cache_creation_tokens))
    .bind(saturating_i64(rollup.reported_input_tokens))
    .bind(rollup.cost_usd)
    .bind(rollup.reported_cost_usd)
    .bind(rollup.cost_estimated)
    .bind(rollup.output_incomplete)
    .bind(&rollup.spawned_agent_call_ids)
    .bind(&rollup.named_agent_call_ids)
    .bind(rollup.correction)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ─── backfill ───────────────────────────────────────────────────────────────

/// Receipts per backfill tick.
const BACKFILL_BATCH: i64 = 200;

/// Receipts with neither a rollup nor a recorded failure, oldest first.
///
/// The payload is projected in SQL so large content never leaves Postgres:
/// prompt and response are dropped, and each tool call loses its arguments,
/// output, raw text and error. Only a string `arguments.name` survives, because
/// the rollup's named-vs-unnamed spawn split reads it. Every JSON operation is
/// guarded by `jsonb_typeof` so a malformed payload decodes to an error in Rust
/// (and is recorded as a failure) instead of failing the whole query.
const BACKFILL_SELECT: &str = r#"
SELECT e.user_id, e.event_id, e.session_id,
       CASE
         WHEN jsonb_typeof(e.payload) <> 'object' THEN e.payload
         WHEN jsonb_typeof(e.payload #> '{turn,tool_calls}') = 'array' THEN
           jsonb_set(
             e.payload #- '{turn,prompt}' #- '{turn,response}',
             '{turn,tool_calls}',
             (SELECT COALESCE(jsonb_agg(
                       CASE WHEN jsonb_typeof(tc) = 'object' THEN
                         (tc - 'arguments' - 'output' - 'raw' - 'error')
                         || CASE WHEN jsonb_typeof(tc #> '{arguments,name}') = 'string'
                                 THEN jsonb_build_object('arguments',
                                        jsonb_build_object('name', tc #> '{arguments,name}'))
                                 ELSE '{}'::jsonb END
                       ELSE tc END
                       ORDER BY ord), '[]'::jsonb)
              FROM jsonb_array_elements(e.payload #> '{turn,tool_calls}')
                   WITH ORDINALITY AS t(tc, ord)))
         ELSE e.payload #- '{turn,prompt}' #- '{turn,response}'
       END AS payload
FROM coding_agent_telemetry_events e
LEFT JOIN coding_agent_turn_usage u
       ON u.user_id = e.user_id AND u.event_id = e.event_id
LEFT JOIN coding_agent_usage_backfill_failures f
       ON f.user_id = e.user_id AND f.event_id = e.event_id
WHERE u.event_id IS NULL AND f.event_id IS NULL AND e.received_at <= $2
ORDER BY e.received_at, e.user_id, e.event_id
LIMIT $1"#;

/// Record a receipt the backfill cannot roll up, so later ticks skip it.
async fn record_backfill_failure(
    db: &PgPool,
    user_id: Uuid,
    event_id: &str,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO coding_agent_usage_backfill_failures (user_id, event_id, error)
           VALUES ($1, $2, $3)
           ON CONFLICT (user_id, event_id) DO NOTHING"#,
    )
    .bind(user_id)
    .bind(event_id)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

/// Whether an insert failed on the row's own data (a data exception, SQLSTATE
/// class 22, or an integrity violation, class 23), so retrying cannot help.
fn is_permanent_row_error(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|db_error| db_error.code())
        .is_some_and(|code| code.starts_with("22") || code.starts_with("23"))
}

/// Fill rollups for up to `limit` receipts received at or before `now` that
/// have none (history stored before migration 0050). Returns the rows filled.
///
/// Read-only on `coding_agent_telemetry_events`. Each receipt is priced
/// outside any transaction and inserted in its own short one, so the tick
/// never holds a write transaction while pricing takes a pool connection.
/// Receipts that cannot be decoded or stored are recorded in
/// `coding_agent_usage_backfill_failures` and skipped from then on, so one bad
/// row cannot stall the queue. Transient database errors propagate.
pub async fn tick_usage_backfill(
    db: &PgPool,
    pricing: &PricingEngine,
    limit: i64,
    now: DateTime<Utc>,
) -> Result<usize, sqlx::Error> {
    let pending: Vec<(Uuid, String, String, serde_json::Value)> = sqlx::query_as(BACKFILL_SELECT)
        .bind(limit)
        .bind(now)
        .fetch_all(db)
        .await?;
    let mut filled = 0;
    for (user_id, event_id, session_id, payload) in pending {
        let event: CodingAgentEventV1 = match serde_json::from_value(payload) {
            Ok(event) => event,
            Err(error) => {
                // The serde message can quote payload values; it goes only to
                // the admin-side failure table, never to the log.
                tracing::warn!(%user_id, %event_id, "tick_usage_backfill: undecodable receipt skipped");
                record_backfill_failure(db, user_id, &event_id, &error.to_string()).await?;
                continue;
            }
        };
        let mut rollup = rollup_for_event(&event, &session_id);
        price_rollup(pricing, &event, &mut rollup).await;
        let mut tx = db.begin().await?;
        match insert_rollup(&mut tx, user_id, &event_id, &rollup).await {
            Ok(()) => {
                tx.commit().await?;
                filled += 1;
            }
            Err(error) if is_permanent_row_error(&error) => {
                tx.rollback().await?;
                tracing::warn!(%user_id, %event_id, %error, "tick_usage_backfill: rollup rejected by database");
                record_backfill_failure(db, user_id, &event_id, &error.to_string()).await?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Backfill loop: one bounded batch per interval. Missed ticks are skipped and
/// a failed tick is logged and retried on the next one; the loop never exits.
pub async fn run_usage_backfill(db: PgPool, pricing: Arc<PricingEngine>, every: Duration) {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        match tick_usage_backfill(&db, &pricing, BACKFILL_BATCH, Utc::now()).await {
            Ok(0) => {}
            Ok(filled) => tracing::info!(filled, "run_usage_backfill: filled usage rollups"),
            Err(error) => tracing::warn!(%error, "run_usage_backfill: tick failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nasiko_types::{
        CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT, CodingAgentCallAccounting, CodingAgentScope,
        CodingAgentSession, CodingAgentSource, CodingAgentTimestampQuality,
        CodingAgentToolAssociation, CodingAgentToolCall, CodingAgentTurn,
    };
    use serde_json::json;

    const SERVER_SESSION: &str = "server-session";

    fn call(id: &str, input: u64, cache_read: u64) -> CodingAgentLlmCall {
        let at = Utc::now();
        CodingAgentLlmCall {
            id: id.into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            input_tokens: input,
            output_tokens: 50,
            cache_read_tokens: cache_read,
            cache_creation_tokens: 10,
            accounting: None,
            started_at: at,
            ended_at: at,
        }
    }

    fn event(source: &str, calls: Vec<CodingAgentLlmCall>) -> CodingAgentEventV1 {
        let at = Utc::now();
        CodingAgentEventV1 {
            version: nasiko_types::CODING_AGENT_EVENT_VERSION,
            event_id: "event".into(),
            captured_at: at,
            source: CodingAgentSource {
                agent_id: source.into(),
                agent_name: "coding-agent".into(),
                adapter_version: None,
            },
            session: CodingAgentSession {
                id: "client-session".into(),
                source_id: "source-session".into(),
                title: None,
            },
            turn: CodingAgentTurn {
                id: "turn".into(),
                prompt: Some("question".into()),
                response: Some("answer".into()),
                started_at: at,
                ended_at: at,
                llm_calls: calls,
                tool_calls: vec![],
                agent_scope: None,
            },
            capture_policy: CapturePolicy::Content,
        }
    }

    fn tool(
        id: &str,
        name: &str,
        status: CodingAgentToolCallStatus,
        arguments: Option<serde_json::Value>,
    ) -> CodingAgentToolCall {
        CodingAgentToolCall {
            id: id.into(),
            name: name.into(),
            kind: "function".into(),
            model_call_id: None,
            status,
            arguments,
            output: None,
            raw: None,
            error: None,
            started_at: None,
            ended_at: None,
            duration_ms: None,
            association: CodingAgentToolAssociation::Exact,
            timestamp_quality: CodingAgentTimestampQuality::Unknown,
        }
    }

    fn scope() -> CodingAgentScope {
        CodingAgentScope {
            kind: CodingAgentScopeKind::Subagent,
            agent_id: "agent-1".into(),
            agent_type: Some("Explore".into()),
            parent_tool_call_id: Some("toolu_1".into()),
            parent_agent_id: Some("agent-0".into()),
            spawn_depth: Some(2),
            description: Some("Find the config loader".into()),
            name: Some("scout".into()),
        }
    }

    // ─── attribution ────────────────────────────────────────────────────────

    #[test]
    fn unscoped_claude_event_is_main_with_summed_classes() {
        let mut event = event("claude", vec![call("a", 100, 1000), call("b", 200, 2000)]);
        event.source.adapter_version = Some(1);
        event.turn.tool_calls = vec![tool(
            "t1",
            "Read",
            CodingAgentToolCallStatus::Succeeded,
            None,
        )];
        let rollup = rollup_for_event(&event, SERVER_SESSION);
        assert_eq!(rollup.agent_kind, "main");
        assert_eq!(rollup.scope_agent_id, None);
        assert_eq!(rollup.agent_type, None);
        assert_eq!(rollup.parent_tool_call_id, None);
        assert_eq!(rollup.parent_agent_id, None);
        assert_eq!(rollup.description, None);
        assert_eq!(rollup.llm_calls, 2);
        assert_eq!(rollup.tool_calls, 1);
        assert_eq!(rollup.input_tokens, 300);
        assert_eq!(rollup.reported_input_tokens, 300);
        assert_eq!(rollup.cache_read_tokens, 3000);
        assert_eq!(rollup.cache_creation_tokens, 20);
        assert_eq!(rollup.output_tokens, 100);
        assert_eq!(rollup.correction, None);
        assert_eq!(rollup.adapter_version, Some(1));
        assert_eq!(rollup.capture_policy, "content");
        assert_eq!(rollup.session_id, SERVER_SESSION);
        assert_eq!(rollup.source_agent_id, "claude");
    }

    #[test]
    fn scoped_content_event_records_scope_and_intent() {
        let mut event = event("claude", vec![call("a", 10, 0)]);
        event.turn.agent_scope = Some(scope());
        let rollup = rollup_for_event(&event, SERVER_SESSION);
        assert_eq!(rollup.agent_kind, "subagent");
        assert_eq!(rollup.scope_agent_id.as_deref(), Some("agent-1"));
        assert_eq!(rollup.agent_type.as_deref(), Some("Explore"));
        assert_eq!(rollup.parent_tool_call_id.as_deref(), Some("toolu_1"));
        assert_eq!(rollup.parent_agent_id.as_deref(), Some("agent-0"));
        assert_eq!(rollup.spawn_depth, Some(2));
        assert_eq!(
            rollup.description.as_deref(),
            Some("Find the config loader")
        );
        assert_eq!(rollup.agent_display_name.as_deref(), Some("scout"));
    }

    #[test]
    fn scoped_metadata_event_never_records_intent() {
        let mut event = event("claude", vec![call("a", 10, 0)]);
        event.capture_policy = CapturePolicy::MetadataOnly;
        event.turn.agent_scope = Some(scope());
        let rollup = rollup_for_event(&event, SERVER_SESSION);
        assert_eq!(rollup.agent_kind, "subagent");
        assert_eq!(rollup.scope_agent_id.as_deref(), Some("agent-1"));
        assert_eq!(rollup.capture_policy, "metadata_only");
        assert_eq!(rollup.description, None);
        assert_eq!(rollup.agent_display_name, None);
    }

    #[test]
    fn teammate_and_unknown_kinds_map_to_their_slugs() {
        let mut event = event("claude", vec![]);
        let mut teammate = scope();
        teammate.kind = CodingAgentScopeKind::Teammate;
        event.turn.agent_scope = Some(teammate);
        assert_eq!(
            rollup_for_event(&event, SERVER_SESSION).agent_kind,
            "teammate"
        );
        let mut unknown = scope();
        unknown.kind = CodingAgentScopeKind::Unknown;
        event.turn.agent_scope = Some(unknown);
        assert_eq!(
            rollup_for_event(&event, SERVER_SESSION).agent_kind,
            "unknown"
        );
    }

    #[test]
    fn trace_id_matches_otlp_trace_id_for_server_session() {
        let event = event("claude", vec![call("a", 10, 0)]);
        let mut scoped = event.clone();
        scoped.session.id = SERVER_SESSION.into();
        assert_eq!(
            rollup_for_event(&event, SERVER_SESSION).trace_id,
            crate::coding_agent_otlp::trace_id_for_event(&scoped)
        );
    }

    // ─── Codex legacy correction ────────────────────────────────────────────

    #[test]
    fn legacy_codex_input_excludes_cache_reads() {
        let event = event("codex", vec![call("a", 1000, 800)]);
        assert!(is_legacy_codex_inclusive(&event));
        let rollup = rollup_for_event(&event, SERVER_SESSION);
        assert_eq!(rollup.input_tokens, 200);
        assert_eq!(rollup.reported_input_tokens, 1000);
        assert_eq!(rollup.cache_read_tokens, 800);
        assert_eq!(rollup.correction, Some(CODEX_INCLUSIVE_INPUT_CORRECTION));
    }

    #[test]
    fn legacy_codex_correction_saturates_at_zero_per_call() {
        let event = event("codex", vec![call("a", 100, 800), call("b", 1000, 400)]);
        let rollup = rollup_for_event(&event, SERVER_SESSION);
        assert_eq!(rollup.input_tokens, 600);
        assert_eq!(rollup.reported_input_tokens, 1100);
    }

    #[test]
    fn marked_codex_is_not_corrected() {
        let mut event = event("codex", vec![call("a", 1000, 800)]);
        event.source.adapter_version = Some(CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT);
        assert!(!is_legacy_codex_inclusive(&event));
        let rollup = rollup_for_event(&event, SERVER_SESSION);
        assert_eq!(rollup.input_tokens, 1000);
        assert_eq!(rollup.input_tokens, rollup.reported_input_tokens);
        assert_eq!(rollup.correction, None);
        assert_eq!(
            rollup.adapter_version,
            Some(CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT)
        );
    }

    #[test]
    fn non_codex_sources_are_never_corrected() {
        for source in ["claude", "cursor", "opencode"] {
            let event = event(source, vec![call("a", 1000, 800)]);
            assert!(!is_legacy_codex_inclusive(&event), "{source}");
            let rollup = rollup_for_event(&event, SERVER_SESSION);
            assert_eq!(rollup.input_tokens, 1000, "{source}");
            assert_eq!(rollup.correction, None, "{source}");
        }
    }

    // ─── output completeness and spawn calls ────────────────────────────────

    #[test]
    fn output_incomplete_only_when_a_call_is_not_final() {
        let mut event = event("claude", vec![call("a", 10, 0), call("b", 10, 0)]);
        assert!(!rollup_for_event(&event, SERVER_SESSION).output_incomplete);
        event.turn.llm_calls[0].accounting = Some(CodingAgentCallAccounting {
            version: 2,
            output_tokens_final: Some(true),
            ..Default::default()
        });
        assert!(!rollup_for_event(&event, SERVER_SESSION).output_incomplete);
        event.turn.llm_calls[1].accounting = Some(CodingAgentCallAccounting {
            version: 2,
            output_tokens_final: Some(false),
            ..Default::default()
        });
        assert!(rollup_for_event(&event, SERVER_SESSION).output_incomplete);
    }

    #[test]
    fn spawn_calls_split_by_name_argument_and_skip_failed() {
        let mut event = event("claude", vec![]);
        event.turn.tool_calls = vec![
            tool(
                "plain",
                "Agent",
                CodingAgentToolCallStatus::Succeeded,
                Some(json!({"subagent_type": "Explore"})),
            ),
            tool(
                "task",
                "Task",
                CodingAgentToolCallStatus::Running,
                Some(json!({"name": ""})),
            ),
            tool(
                "named",
                "Agent",
                CodingAgentToolCallStatus::Succeeded,
                Some(json!({"name": "scout"})),
            ),
            tool("failed", "Agent", CodingAgentToolCallStatus::Failed, None),
            tool("denied", "Task", CodingAgentToolCallStatus::Denied, None),
            tool(
                "timeout",
                "Agent",
                CodingAgentToolCallStatus::TimedOut,
                None,
            ),
            tool(
                "cancel",
                "Agent",
                CodingAgentToolCallStatus::Cancelled,
                None,
            ),
            tool("read", "Read", CodingAgentToolCallStatus::Succeeded, None),
        ];
        let rollup = rollup_for_event(&event, SERVER_SESSION);
        assert_eq!(rollup.spawned_agent_call_ids, vec!["plain", "task"]);
        assert_eq!(rollup.named_agent_call_ids, vec!["named"]);
        assert_eq!(rollup.tool_calls, 8);
    }

    #[test]
    fn metadata_only_spawns_without_arguments_count_as_spawned() {
        let mut event = event("claude", vec![]);
        event.capture_policy = CapturePolicy::MetadataOnly;
        event.turn.tool_calls = vec![tool(
            "agent",
            "Agent",
            CodingAgentToolCallStatus::Succeeded,
            None,
        )];
        let rollup = rollup_for_event(&event, SERVER_SESSION);
        assert_eq!(rollup.spawned_agent_call_ids, vec!["agent"]);
        assert!(rollup.named_agent_call_ids.is_empty());
    }

    // ─── pricing ────────────────────────────────────────────────────────────

    async fn reported_total(pricing: &PricingEngine, event: &CodingAgentEventV1) -> Decimal {
        let mut total = CostBreakdown::default();
        for call in &event.turn.llm_calls {
            total.add(
                pricing
                    .price(
                        Some(&call.provider),
                        &call.model,
                        RawUsage {
                            input: call.input_tokens,
                            output: call.output_tokens,
                            cache_read: call.cache_read_tokens,
                            cache_creation: call.cache_creation_tokens,
                            total: None,
                        },
                        PromptConvention::Exclusive,
                        call.started_at,
                    )
                    .await
                    .cost,
            );
        }
        Decimal::from_f64_retain(total.total_usd)
            .unwrap()
            .round_dp(6)
    }

    #[tokio::test]
    async fn legacy_codex_costs_less_after_correction_and_keeps_reported_cost() {
        let pricing = PricingEngine::offline();
        let event = event("codex", vec![call("a", 1000, 800)]);
        let mut rollup = rollup_for_event(&event, SERVER_SESSION);
        price_rollup(&pricing, &event, &mut rollup).await;
        let reported = rollup.reported_cost_usd.unwrap();
        let corrected = rollup.cost_usd.unwrap();
        assert_eq!(reported, reported_total(&pricing, &event).await);
        assert!(corrected < reported, "{corrected} < {reported}");
        assert!(rollup.cost_estimated.is_some());
        assert_eq!(
            reported_cost_for_chat(&rollup),
            Some((reported, rollup.cost_estimated.unwrap()))
        );
    }

    #[tokio::test]
    async fn uncorrected_rollup_costs_equal_reported() {
        let pricing = PricingEngine::offline();
        let event = event("claude", vec![call("a", 1000, 800), call("b", 10, 5)]);
        let mut rollup = rollup_for_event(&event, SERVER_SESSION);
        price_rollup(&pricing, &event, &mut rollup).await;
        assert_eq!(rollup.cost_usd, rollup.reported_cost_usd);
        assert_eq!(
            rollup.reported_cost_usd,
            Some(reported_total(&pricing, &event).await)
        );
    }

    #[tokio::test]
    async fn turn_without_calls_is_unpriced() {
        let pricing = PricingEngine::offline();
        let event = event("claude", vec![]);
        let mut rollup = rollup_for_event(&event, SERVER_SESSION);
        price_rollup(&pricing, &event, &mut rollup).await;
        assert_eq!(rollup.cost_usd, None);
        assert_eq!(rollup.reported_cost_usd, None);
        assert_eq!(rollup.cost_estimated, None);
        assert_eq!(reported_cost_for_chat(&rollup), None);
    }
}
