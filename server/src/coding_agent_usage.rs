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
pub fn is_legacy_codex_inclusive(event: &CodingAgentEventV1) -> bool {
    todo!()
}

/// Build the rollup for one receipt. Pure; costs are filled by [`price_rollup`].
pub fn rollup_for_event(event: &CodingAgentEventV1, server_session_id: &str) -> TurnUsageRollup {
    todo!()
}

/// Price the rollup's reported and (when corrected) corrected usage.
pub async fn price_rollup(
    pricing: &PricingEngine,
    event: &CodingAgentEventV1,
    rollup: &mut TurnUsageRollup,
) {
    todo!()
}

/// The reported cost and estimate flag the chat transcript stores for a turn.
pub fn reported_cost_for_chat(rollup: &TurnUsageRollup) -> Option<(Decimal, bool)> {
    todo!()
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
