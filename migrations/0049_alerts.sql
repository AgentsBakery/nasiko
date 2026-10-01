-- Phase 3: alerts, monitors and durable notifications.
--
-- One migration for the whole phase schema. Later plans only add code on top
-- of these tables. `created_by` / `acknowledged_by` deliberately have no
-- foreign key: superuser tokens (and EE identities) may have no `users` row.

CREATE TABLE alerts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    kind TEXT NOT NULL CHECK (kind IN ('budget_soft', 'budget_hard', 'spend_spike', 'monitor_breach')),
    severity TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
    scope TEXT NOT NULL CHECK (scope IN ('platform', 'agent', 'model', 'user')),
    scope_ref TEXT,
    dedup_key TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'acknowledged', 'resolved')),
    title TEXT NOT NULL,
    message TEXT NOT NULL,
    link TEXT NOT NULL DEFAULT '/tokenops',
    details JSONB NOT NULL DEFAULT '{}'::jsonb,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    occurrences INT NOT NULL DEFAULT 1,
    acknowledged_by UUID,
    acknowledged_at TIMESTAMPTZ,
    resolved_at TIMESTAMPTZ
);

-- This index IS the dedup mechanism (ALRT-04): at most one non-resolved alert
-- per dedup_key, so concurrent replicas raising the same condition collapse
-- into one row. An acknowledged alert still counts as live.
CREATE UNIQUE INDEX uq_alerts_open_dedup ON alerts (dedup_key) WHERE status <> 'resolved';
CREATE INDEX idx_alerts_first_seen ON alerts (first_seen_at DESC, id DESC);
CREATE INDEX idx_alerts_kind_status ON alerts (kind, status);

CREATE TABLE monitors (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    metric TEXT NOT NULL CHECK (metric IN ('error_rate', 'p95_latency_ms')),
    scope TEXT NOT NULL CHECK (scope IN ('agent', 'model', 'platform')),
    scope_ref TEXT,
    window_minutes INT NOT NULL CHECK (window_minutes BETWEEN 5 AND 1440),
    threshold NUMERIC NOT NULL CHECK (threshold > 0),
    min_samples INT NOT NULL DEFAULT 20 CHECK (min_samples >= 1),
    severity TEXT NOT NULL DEFAULT 'warning' CHECK (severity IN ('info', 'warning', 'critical')),
    enabled BOOLEAN NOT NULL DEFAULT true,
    created_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT monitors_ref_matches_scope CHECK (
        (scope = 'platform' AND scope_ref IS NULL)
        OR (scope <> 'platform' AND scope_ref IS NOT NULL)
    )
);

CREATE TRIGGER trg_monitors_updated_at BEFORE UPDATE ON monitors
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- The destination URL and optional HMAC secret live encrypted in
-- `config_encrypted`; the API only ever returns `url_hint` and
-- `has_hmac_secret`.
CREATE TABLE notification_channels (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('webhook', 'slack')),
    config_encrypted TEXT NOT NULL,
    url_hint TEXT NOT NULL,
    has_hmac_secret BOOLEAN NOT NULL DEFAULT false,
    enabled BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER trg_notification_channels_updated_at BEFORE UPDATE ON notification_channels
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- `alert_kind` NULL means "every kind". A surrogate key plus the COALESCE
-- unique index keeps NULL-kind routes unique per channel and severity.
CREATE TABLE notification_routes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    channel_id UUID NOT NULL REFERENCES notification_channels (id) ON DELETE CASCADE,
    alert_kind TEXT CHECK (alert_kind IN ('budget_soft', 'budget_hard', 'spend_spike', 'monitor_breach')),
    min_severity TEXT NOT NULL DEFAULT 'info' CHECK (min_severity IN ('info', 'warning', 'critical')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX uq_notification_routes_channel_kind_severity
    ON notification_routes (channel_id, COALESCE(alert_kind, '*'), min_severity);

-- Transactional outbox: rows are written in the same transaction as the alert
-- change, then delivered at-least-once by the dispatcher. `alert_id` is
-- nullable because channel test events have no alert.
CREATE TABLE notification_outbox (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    alert_id UUID REFERENCES alerts (id) ON DELETE CASCADE,
    channel_id UUID NOT NULL REFERENCES notification_channels (id) ON DELETE CASCADE,
    event TEXT NOT NULL CHECK (event IN ('opened', 'escalated', 'resolved', 'test')),
    payload JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'sending', 'delivered', 'failed')),
    attempts INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    claimed_at TIMESTAMPTZ,
    last_error TEXT,
    delivered_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_notification_outbox_status_next ON notification_outbox (status, next_attempt_at);
CREATE INDEX idx_notification_outbox_alert ON notification_outbox (alert_id);

-- Router-side LLM call failures (chat, messages, embeddings, responses), the
-- error-rate monitor's numerator. No foreign keys: a non-UUID owner or a
-- since-deleted agent must never block the write.
CREATE TABLE llm_call_failures (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id UUID,
    user_id UUID,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    status_code INT,
    error_kind TEXT NOT NULL,
    streaming BOOLEAN NOT NULL DEFAULT false,
    operation_type TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_llm_call_failures_created ON llm_call_failures (created_at);
CREATE INDEX idx_llm_call_failures_agent_time ON llm_call_failures (agent_id, created_at)
    WHERE agent_id IS NOT NULL;
CREATE INDEX idx_llm_call_failures_model_time ON llm_call_failures (model, created_at);

-- Cursor for the alert consumer: NULL means "not yet considered for alerting".
ALTER TABLE budget_events ADD COLUMN alerted_at TIMESTAMPTZ;
CREATE INDEX idx_budget_events_unalerted ON budget_events (created_at) WHERE alerted_at IS NULL;
