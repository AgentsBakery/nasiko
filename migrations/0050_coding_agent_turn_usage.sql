-- Phase 4: per-receipt coding-agent usage rollup and the Codex read-time
-- correction overlay.
--
-- Why a derived table: per-agent attribution (main vs subagent vs teammate)
-- and the Codex history correction both need token and cost figures that the
-- receipts and trace_usage cannot carry. Receipts in
-- coding_agent_telemetry_events are immutable (trigger in 0017), and
-- trace_usage is re-upserted from Tempo spans by the materializer, so any
-- in-place rewrite of either would be lost or forbidden. Corrections
-- therefore live only in these derived rows and are applied at read time.
--
-- One row per accepted receipt (user_id, event_id). Subagent receipts are
-- their own rows and are never folded into the parent turn, so summing rows
-- counts every token exactly once.

CREATE TABLE coding_agent_turn_usage (
    user_id UUID NOT NULL,
    event_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES chat_sessions(session_id) ON DELETE CASCADE,
    trace_id TEXT NOT NULL,
    source_agent_id TEXT NOT NULL,
    capture_policy TEXT NOT NULL CHECK (capture_policy IN ('content', 'metadata_only')),
    adapter_version INTEGER,
    agent_kind TEXT NOT NULL CHECK (agent_kind IN ('main', 'subagent', 'teammate', 'unknown')),
    -- NULL for main-agent rows.
    scope_agent_id TEXT,
    agent_type TEXT,
    parent_tool_call_id TEXT,
    parent_agent_id TEXT,
    spawn_depth INTEGER,
    -- Content only: copied from content receipts, never from metadata-only ones.
    description TEXT,
    agent_display_name TEXT,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ NOT NULL,
    llm_calls INTEGER NOT NULL,
    tool_calls INTEGER NOT NULL,
    -- Corrected (cache-exclusive) input; equals reported_input_tokens unless
    -- `correction` is set.
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    cache_read_tokens BIGINT NOT NULL,
    cache_creation_tokens BIGINT NOT NULL,
    reported_input_tokens BIGINT NOT NULL,
    -- NULL = unpriced (a turn without LLM calls).
    cost_usd NUMERIC(18, 6),
    reported_cost_usd NUMERIC(18, 6),
    cost_estimated BOOLEAN,
    output_incomplete BOOLEAN NOT NULL DEFAULT false,
    spawned_agent_call_ids TEXT[] NOT NULL DEFAULT '{}',
    named_agent_call_ids TEXT[] NOT NULL DEFAULT '{}',
    -- e.g. 'codex_inclusive_input_v1'; NULL when the reported figures were stored as-is.
    correction TEXT,
    computed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, event_id),
    FOREIGN KEY (user_id, event_id)
        REFERENCES coding_agent_telemetry_events(user_id, event_id) ON DELETE CASCADE,
    CONSTRAINT coding_agent_turn_usage_content_only_intent CHECK (
        (description IS NULL AND agent_display_name IS NULL) OR capture_policy = 'content'
    )
);

CREATE INDEX idx_coding_agent_turn_usage_session ON coding_agent_turn_usage (session_id);
CREATE INDEX idx_coding_agent_turn_usage_trace
    ON coding_agent_turn_usage (trace_id) WHERE correction IS NOT NULL;

-- Receipts the backfill could not decode. Recorded once so the bounded
-- backfill tick advances past them instead of reselecting them forever. These
-- rows never enter any usage aggregate.
CREATE TABLE coding_agent_usage_backfill_failures (
    user_id UUID NOT NULL,
    event_id TEXT NOT NULL,
    error TEXT NOT NULL,
    failed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, event_id),
    FOREIGN KEY (user_id, event_id)
        REFERENCES coding_agent_telemetry_events(user_id, event_id) ON DELETE CASCADE
);

-- trace_usage with legacy Codex double-counted cached input removed.
--
-- Codex CLIs before adapter_version 1 reported input inclusive of cache reads,
-- so trace_usage (materialized from those spans) counts cached input twice in
-- input_tokens and in the prompt cost. The rollup rows carry both reported and
-- corrected figures; this view subtracts the per-trace difference. Traces with
-- no corrected rollup row come through unchanged with usage_corrected = false.
CREATE VIEW trace_usage_corrected AS
SELECT
    t.trace_id,
    t.agent_name,
    t.session_id,
    t.agent_id,
    t.user_id,
    t.model,
    t.provider,
    GREATEST(0, t.input_tokens - COALESCE(d.input_delta, 0))::BIGINT AS input_tokens,
    t.output_tokens,
    t.cache_read_tokens,
    t.cache_creation_tokens,
    GREATEST(0, t.cost_usd - COALESCE(d.cost_delta, 0))::DOUBLE PRECISION AS cost_usd,
    GREATEST(0, t.prompt_cost_usd - COALESCE(d.cost_delta, 0))::DOUBLE PRECISION
        AS prompt_cost_usd,
    t.completion_cost_usd,
    t.latency_ms,
    t.started_at,
    t.materialized_at,
    t.tool_call_count,
    t.cost_estimated,
    (d.trace_id IS NOT NULL) AS usage_corrected
FROM trace_usage t
LEFT JOIN (
    SELECT
        trace_id,
        SUM(reported_input_tokens - input_tokens)::BIGINT AS input_delta,
        SUM(COALESCE(reported_cost_usd - cost_usd, 0))::DOUBLE PRECISION AS cost_delta
    FROM coding_agent_turn_usage
    WHERE correction IS NOT NULL
    GROUP BY trace_id
) d ON d.trace_id = t.trace_id;
