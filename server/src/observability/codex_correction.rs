//! D2 read-time correction of legacy Codex usage (TELE-02).
//!
//! Codex CLIs before adapter_version 1 reported input tokens inclusive of
//! cache reads, so every figure derived from those receipts (Tempo spans,
//! `trace_usage`, `chat_messages`) counts cached input twice. The receipts are
//! immutable and `trace_usage` is re-upserted from Tempo by the materializer,
//! so neither can be rewritten: a correction written there would be forbidden
//! or overwritten. Corrections are overlays instead, read from
//! `coding_agent_turn_usage`, whose `correction` column is set only by the
//! server-side adapter marker rule (`source_agent_id = 'codex'` with no
//! `adapter_version`). Nothing here writes; every figure it corrects is
//! flagged `usage_corrected` so history is never silently rewritten.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use sqlx::PgPool;

use super::service::{RootSpanEntry, SessionDetail, SessionSummary};

/// The legacy double count on one trace or one session: what to subtract from
/// the reported figures. `cache_read_tokens` is the cached input of the
/// corrected receipts, used to split the cost delta across spans.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Delta {
    pub input_tokens: u64,
    pub cost_usd: f64,
    pub cache_read_tokens: u64,
}

impl Delta {
    fn add(&mut self, other: Delta) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cost_usd += other.cost_usd;
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
    }
}

/// Corrections for a set of traces and the sessions they belong to, plus the
/// session's scoped (subagent / teammate) trace ids when loaded by session.
/// Empty when nothing in the requested set was corrected or scoped.
#[derive(Debug, Clone, Default)]
pub struct CorrectionOverlay {
    by_trace: HashMap<String, Delta>,
    by_session: HashMap<String, Delta>,
    scoped_traces: HashSet<String>,
}

impl CorrectionOverlay {
    /// The correction for one trace, `None` for uncorrected or unknown traces.
    pub fn trace(&self, trace_id: &str) -> Option<&Delta> {
        self.by_trace.get(trace_id)
    }

    /// The summed correction for one session, `None` when nothing in it was corrected.
    pub fn session(&self, session_id: &str) -> Option<&Delta> {
        self.by_session.get(session_id)
    }

    /// The summed correction over exactly `trace_ids`, `None` when none of them
    /// was corrected. Session views sum over the traces they actually show, so
    /// a trace outside the provider's window is never subtracted.
    pub fn sum_for_traces<'a>(
        &self,
        trace_ids: impl IntoIterator<Item = &'a str>,
    ) -> Option<Delta> {
        let mut total: Option<Delta> = None;
        for delta in trace_ids.into_iter().filter_map(|t| self.trace(t)) {
            total.get_or_insert_with(Delta::default).add(*delta);
        }
        total
    }

    /// Trace ids the rollup attributes to a subagent or teammate (only filled
    /// by [`load_for_sessions`]).
    pub fn scoped_traces(&self) -> &HashSet<String> {
        &self.scoped_traces
    }

    /// An overlay built in memory, for unit tests of the appliers.
    #[cfg(test)]
    pub(crate) fn for_test(deltas: &[(&str, &str, Delta)], scoped: &[&str]) -> Self {
        let mut overlay = Self::default();
        for (trace_id, session_id, delta) in deltas {
            overlay.insert((*trace_id).into(), (*session_id).into(), *delta);
        }
        overlay.scoped_traces = scoped.iter().map(|t| (*t).to_owned()).collect();
        overlay
    }

    fn insert(&mut self, trace_id: String, session_id: String, delta: Delta) {
        self.by_trace.entry(trace_id).or_default().add(delta);
        self.by_session.entry(session_id).or_default().add(delta);
    }
}

#[derive(sqlx::FromRow)]
struct DeltaRow {
    trace_id: String,
    session_id: String,
    corrected: bool,
    scoped: bool,
    input_delta: i64,
    cost_delta: f64,
    cache_read_tokens: i64,
}

impl From<&DeltaRow> for Delta {
    fn from(row: &DeltaRow) -> Self {
        Delta {
            input_tokens: row.input_delta.max(0) as u64,
            cost_usd: row.cost_delta.max(0.0),
            cache_read_tokens: row.cache_read_tokens.max(0) as u64,
        }
    }
}

/// Deltas count only rows the marker rule corrected and are clamped at zero
/// per row (T-04-18): a correction can only ever lower a figure.
const DELTA_SELECT: &str = r#"SELECT trace_id, session_id,
              bool_or(correction IS NOT NULL) AS corrected,
              bool_or(agent_kind <> 'main') AS scoped,
              COALESCE(SUM(GREATEST(reported_input_tokens - input_tokens, 0))
                  FILTER (WHERE correction IS NOT NULL), 0)::BIGINT AS input_delta,
              COALESCE(SUM(GREATEST(COALESCE(reported_cost_usd - cost_usd, 0), 0))
                  FILTER (WHERE correction IS NOT NULL), 0)::FLOAT8 AS cost_delta,
              COALESCE(SUM(cache_read_tokens)
                  FILTER (WHERE correction IS NOT NULL), 0)::BIGINT AS cache_read_tokens
       FROM coding_agent_turn_usage"#;

async fn load(db: &PgPool, filter: &str, ids: &[String]) -> Result<CorrectionOverlay, sqlx::Error> {
    let mut overlay = CorrectionOverlay::default();
    if ids.is_empty() {
        return Ok(overlay);
    }
    let query = format!("{DELTA_SELECT} WHERE {filter} GROUP BY trace_id, session_id");
    let rows: Vec<DeltaRow> = sqlx::query_as(&query).bind(ids).fetch_all(db).await?;
    for row in &rows {
        if row.scoped {
            overlay.scoped_traces.insert(row.trace_id.clone());
        }
        if row.corrected {
            overlay.insert(
                row.trace_id.clone(),
                row.session_id.clone(),
                Delta::from(row),
            );
        }
    }
    Ok(overlay)
}

/// Corrections and scoped trace ids for every trace in `session_ids`, in one
/// query. Callers pass only session ids already in an authorized response
/// (T-04-19).
pub async fn load_for_sessions(
    db: &PgPool,
    session_ids: &[String],
) -> Result<CorrectionOverlay, sqlx::Error> {
    load(
        db,
        "session_id = ANY($1) AND (correction IS NOT NULL OR agent_kind <> 'main')",
        session_ids,
    )
    .await
}

/// Corrections for `trace_ids` (served by the partial trace index on corrected
/// rows). Callers pass only trace ids already in an authorized response
/// (T-04-19).
pub async fn load_for_traces(
    db: &PgPool,
    trace_ids: &[String],
) -> Result<CorrectionOverlay, sqlx::Error> {
    load(
        db,
        "correction IS NOT NULL AND trace_id = ANY($1)",
        trace_ids,
    )
    .await
}

// ─── pure appliers ──────────────────────────────────────────────────────────

/// Corrected token count: `tokens - delta`, never below zero.
pub fn apply_tokens(tokens: u64, delta: u64) -> u64 {
    tokens.saturating_sub(delta)
}

/// Corrected cost: `cost - delta`, never negative.
pub fn apply_cost(cost: f64, delta: f64) -> f64 {
    (cost - delta).max(0.0)
}

/// The share of a trace's cost delta that belongs to one span, proportional to
/// the span's cached input (the double-counted part). Zero when the trace has
/// no cached input to split by.
pub fn span_share(span_cache_read: u64, trace_cache_read: u64) -> f64 {
    if trace_cache_read == 0 {
        return 0.0;
    }
    (span_cache_read as f64 / trace_cache_read as f64).min(1.0)
}

// ─── session views ──────────────────────────────────────────────────────────

/// Root-span attribute the OTLP export sets on subagent / teammate turns.
pub const AGENT_KIND_ATTRIBUTE: &str = "coding_agent.agent.kind";

/// True when a trace is a subagent / teammate turn rather than a user ask:
/// its root span carries [`AGENT_KIND_ATTRIBUTE`] (other than `main`), or the
/// rollup lists it as scoped. The rollup fallback covers traces whose root
/// attributes the trace store does not return.
pub fn is_scoped_trace(
    trace_id: &str,
    root_attributes: &HashMap<String, Value>,
    scoped_ids: &HashSet<String>,
) -> bool {
    root_attributes
        .get(AGENT_KIND_ATTRIBUTE)
        .is_some_and(|kind| kind.as_str() != Some("main"))
        || scoped_ids.contains(trace_id)
}

/// Subtract a session's legacy delta from a session-list row. `None` leaves
/// the row untouched (and unflagged).
pub fn correct_session_summary(summary: &mut SessionSummary, delta: Option<&Delta>) {
    let Some(delta) = delta else { return };
    if let Some(total) = summary.token_usage.total.as_mut() {
        *total = apply_tokens(*total, delta.input_tokens);
    }
    if let Some(cost) = summary.cost_summary.total.cost.as_mut() {
        *cost = apply_cost(*cost, delta.cost_usd);
    }
    summary.usage_corrected = true;
}

/// Subtract a session's legacy delta from the session-detail totals. `None`
/// leaves the detail untouched (and unflagged).
pub fn correct_session_detail(detail: &mut SessionDetail, delta: Option<&Delta>) {
    let Some(delta) = delta else { return };
    if let Some(total) = detail.token_usage.total.as_mut() {
        *total = apply_tokens(*total, delta.input_tokens);
    }
    let costs = &mut detail.cost_summary;
    costs.total.cost = apply_cost(costs.total.cost, delta.cost_usd);
    costs.total.tokens = apply_tokens(costs.total.tokens, delta.input_tokens);
    costs.prompt.cost = apply_cost(costs.prompt.cost, delta.cost_usd);
    costs.prompt.tokens = apply_tokens(costs.prompt.tokens, delta.input_tokens);
    detail.usage_corrected = true;
}

/// Subtract one trace's legacy delta from its per-turn root entry. `None`
/// leaves the entry untouched (and unflagged).
pub fn correct_root_entry(entry: &mut RootSpanEntry, delta: Option<&Delta>) {
    let Some(delta) = delta else { return };
    entry.input_tokens = apply_tokens(entry.input_tokens, delta.input_tokens);
    entry.cumulative_token_count_total =
        apply_tokens(entry.cumulative_token_count_total, delta.input_tokens);
    if let Some(cost) = entry
        .trace
        .cost_summary
        .get_mut("total")
        .and_then(|total| total.get_mut("cost"))
        && let Some(value) = cost.as_f64()
    {
        *cost = Value::from(apply_cost(value, delta.cost_usd));
    }
    entry.usage_corrected = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_tokens_saturates_at_zero() {
        assert_eq!(apply_tokens(1000, 800), 200);
        assert_eq!(apply_tokens(100, 800), 0);
        assert_eq!(apply_tokens(0, 0), 0);
    }

    #[test]
    fn apply_cost_never_negative() {
        assert!((apply_cost(0.5, 0.2) - 0.3).abs() < 1e-12);
        assert_eq!(apply_cost(0.1, 0.5), 0.0);
    }

    #[test]
    fn span_share_is_proportional_and_bounded() {
        assert_eq!(span_share(0, 0), 0.0);
        assert_eq!(span_share(400, 800), 0.5);
        assert_eq!(span_share(800, 800), 1.0);
        assert_eq!(span_share(900, 800), 1.0);
    }

    #[test]
    fn overlay_lookup_unknown_trace_is_none() {
        let mut overlay = CorrectionOverlay::default();
        let delta = Delta {
            input_tokens: 800,
            cost_usd: 0.002,
            cache_read_tokens: 800,
        };
        overlay.insert("t1".into(), "s1".into(), delta);
        overlay.insert("t2".into(), "s1".into(), delta);
        assert_eq!(overlay.trace("t1"), Some(&delta));
        assert!(overlay.trace("unknown").is_none());
        assert!(overlay.session("other").is_none());
        assert_eq!(overlay.session("s1").unwrap().input_tokens, 1600);
        assert_eq!(
            overlay
                .sum_for_traces(["t1", "unknown"])
                .unwrap()
                .input_tokens,
            800
        );
        assert!(overlay.sum_for_traces(["unknown"]).is_none());
    }

    // ── session appliers ────────────────────────────────────────────────────

    use crate::observability::service::{
        ContentField, CostEntry, CostWithTokens, FullCostSummary, Pagination, ProjectRef,
        SimpleCostSummary, TokenUsageSummary, TraceRef,
    };

    fn delta() -> Delta {
        Delta {
            input_tokens: 800,
            cost_usd: 0.004,
            cache_read_tokens: 800,
        }
    }

    fn summary() -> SessionSummary {
        SessionSummary {
            id: "s".into(),
            session_id: "s".into(),
            agent_id: "codex".into(),
            num_traces: Some(1),
            start_time: None,
            end_time: None,
            duration_ms: None,
            first_input: Some("ask".into()),
            last_output: Some("answer".into()),
            token_usage: TokenUsageSummary { total: Some(1850) },
            trace_latency_ms_p50: None,
            trace_latency_ms_p99: None,
            cost_summary: SimpleCostSummary {
                total: CostEntry { cost: Some(0.01) },
            },
            session_annotations: vec![],
            session_annotation_summaries: vec![],
            usage_corrected: false,
        }
    }

    fn cost(cost: f64, tokens: u64) -> CostWithTokens {
        CostWithTokens { cost, tokens }
    }

    fn detail() -> SessionDetail {
        SessionDetail {
            id: "s".into(),
            session_id: "s".into(),
            title: None,
            agent_name: None,
            num_traces: 1,
            token_usage: TokenUsageSummary { total: Some(1850) },
            cost_summary: FullCostSummary {
                total: cost(0.01, 1850),
                prompt: cost(0.006, 1000),
                completion: cost(0.002, 50),
                cache_read: cost(0.002, 800),
                cache_creation: cost(0.0, 0),
            },
            latency_p50: None,
            latency_p99: None,
            latency_avg: None,
            cache_read_tokens: 800,
            cache_creation_tokens: 0,
            metrics_complete: true,
            traces: vec![],
            pagination: Pagination {
                end_cursor: None,
                has_next_page: false,
            },
            usage_corrected: false,
        }
    }

    fn content() -> ContentField {
        ContentField {
            value: String::new(),
            mime_type: "text".into(),
            parsed_value: None,
        }
    }

    fn root_entry() -> RootSpanEntry {
        RootSpanEntry {
            id: "r".into(),
            span_id: "r".into(),
            attributes: "{}".into(),
            cumulative_token_count_total: 1850,
            input_tokens: 1000,
            output_tokens: 50,
            cache_read_tokens: 800,
            cache_creation_tokens: 0,
            latency_ms: 1.0,
            start_time: None,
            span_annotations: vec![],
            span_annotation_summaries: vec![],
            project: ProjectRef { id: String::new() },
            input: content(),
            output: content(),
            trace: TraceRef {
                id: "t".into(),
                cost_summary: serde_json::json!({"total": {"cost": 0.01}}),
            },
            usage_corrected: false,
        }
    }

    fn json<T: serde::Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn appliers_without_delta_are_byte_identical() {
        let (mut s, mut d, mut r) = (summary(), detail(), root_entry());
        let before = (json(&s), json(&d), json(&r));
        correct_session_summary(&mut s, None);
        correct_session_detail(&mut d, None);
        correct_root_entry(&mut r, None);
        assert_eq!((json(&s), json(&d), json(&r)), before);
        assert!(!before.0.contains("usage_corrected"));
        assert!(!before.2.contains("usage_corrected"));
    }

    #[test]
    fn correct_session_summary_subtracts_and_flags() {
        let mut s = summary();
        correct_session_summary(&mut s, Some(&delta()));
        assert_eq!(s.token_usage.total, Some(1050));
        assert!((s.cost_summary.total.cost.unwrap() - 0.006).abs() < 1e-12);
        assert!(s.usage_corrected);
        assert!(json(&s).contains(r#""usage_corrected":true"#));
    }

    #[test]
    fn correct_session_summary_keeps_unknown_figures_unknown() {
        let mut s = summary();
        s.token_usage.total = None;
        s.cost_summary.total.cost = None;
        correct_session_summary(&mut s, Some(&delta()));
        assert_eq!(s.token_usage.total, None);
        assert_eq!(s.cost_summary.total.cost, None);
    }

    #[test]
    fn correct_session_detail_adjusts_totals_and_prompt() {
        let mut d = detail();
        correct_session_detail(&mut d, Some(&delta()));
        assert_eq!(d.token_usage.total, Some(1050));
        assert_eq!(d.cost_summary.total.tokens, 1050);
        assert_eq!(d.cost_summary.prompt.tokens, 200);
        assert!((d.cost_summary.total.cost - 0.006).abs() < 1e-12);
        assert!((d.cost_summary.prompt.cost - 0.002).abs() < 1e-12);
        // Cache classes are always correct and stay untouched.
        assert_eq!(d.cost_summary.cache_read.tokens, 800);
        assert_eq!(d.cache_read_tokens, 800);
        assert!(d.usage_corrected);
    }

    #[test]
    fn correct_root_entry_adjusts_turn_figures() {
        let mut r = root_entry();
        correct_root_entry(&mut r, Some(&delta()));
        assert_eq!(r.input_tokens, 200);
        assert_eq!(r.cumulative_token_count_total, 1050);
        assert_eq!(r.cache_read_tokens, 800);
        let cost = r.trace.cost_summary["total"]["cost"].as_f64().unwrap();
        assert!((cost - 0.006).abs() < 1e-12);
        assert!(r.usage_corrected);
    }

    #[test]
    fn is_scoped_trace_attribute_rollup_or_neither() {
        let scoped_attrs = HashMap::from([(
            AGENT_KIND_ATTRIBUTE.to_owned(),
            serde_json::json!("subagent"),
        )]);
        let main_attrs =
            HashMap::from([(AGENT_KIND_ATTRIBUTE.to_owned(), serde_json::json!("main"))]);
        let none = HashMap::new();
        let rollup: HashSet<String> = ["t-rollup".to_owned()].into();
        let empty = HashSet::new();
        assert!(
            is_scoped_trace("t", &scoped_attrs, &empty),
            "attribute only"
        );
        assert!(is_scoped_trace("t-rollup", &none, &rollup), "rollup only");
        assert!(!is_scoped_trace("t", &none, &rollup), "neither");
        assert!(!is_scoped_trace("t", &main_attrs, &empty), "main kind");
    }
}
