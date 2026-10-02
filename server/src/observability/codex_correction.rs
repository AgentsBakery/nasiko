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

use std::collections::HashMap;

use sqlx::PgPool;

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

/// Corrections for a set of traces and the sessions they belong to. Empty
/// when nothing in the requested set was corrected.
#[derive(Debug, Clone, Default)]
pub struct CorrectionOverlay {
    by_trace: HashMap<String, Delta>,
    by_session: HashMap<String, Delta>,
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

    fn insert(&mut self, trace_id: String, session_id: String, delta: Delta) {
        self.by_trace.entry(trace_id).or_default().add(delta);
        self.by_session.entry(session_id).or_default().add(delta);
    }
}

#[derive(sqlx::FromRow)]
struct DeltaRow {
    trace_id: String,
    session_id: String,
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

/// Deltas are clamped at zero per row (T-04-18): a corrected row can only
/// ever lower a figure, never raise it.
const DELTA_SELECT: &str = r#"SELECT trace_id, session_id,
              SUM(GREATEST(reported_input_tokens - input_tokens, 0))::BIGINT AS input_delta,
              SUM(GREATEST(COALESCE(reported_cost_usd - cost_usd, 0), 0))::FLOAT8 AS cost_delta,
              SUM(cache_read_tokens)::BIGINT AS cache_read_tokens
       FROM coding_agent_turn_usage
       WHERE correction IS NOT NULL"#;

async fn load(db: &PgPool, column: &str, ids: &[String]) -> Result<CorrectionOverlay, sqlx::Error> {
    let mut overlay = CorrectionOverlay::default();
    if ids.is_empty() {
        return Ok(overlay);
    }
    let query = format!("{DELTA_SELECT} AND {column} = ANY($1) GROUP BY trace_id, session_id");
    let rows: Vec<DeltaRow> = sqlx::query_as(&query).bind(ids).fetch_all(db).await?;
    for row in &rows {
        overlay.insert(
            row.trace_id.clone(),
            row.session_id.clone(),
            Delta::from(row),
        );
    }
    Ok(overlay)
}

/// Corrections for every trace in `session_ids`. Callers pass only session ids
/// already in an authorized response (T-04-19).
pub async fn load_for_sessions(
    db: &PgPool,
    session_ids: &[String],
) -> Result<CorrectionOverlay, sqlx::Error> {
    load(db, "session_id", session_ids).await
}

/// Corrections for `trace_ids`. Callers pass only trace ids already in an
/// authorized response (T-04-19).
pub async fn load_for_traces(
    db: &PgPool,
    trace_ids: &[String],
) -> Result<CorrectionOverlay, sqlx::Error> {
    load(db, "trace_id", trace_ids).await
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
    }
}
