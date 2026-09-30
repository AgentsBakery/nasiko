-- Dollar budgets (user / agent / platform) enforced by the LLM router, and the
-- durable events Phase 3 alerting consumes.
--
-- `created_by` deliberately has no foreign key: superuser tokens (and EE
-- identities) may have no `users` row.

CREATE TABLE budgets (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('user', 'agent', 'platform')),
    target_id UUID,
    period TEXT NOT NULL CHECK (period IN ('daily', 'weekly', 'monthly')),
    limit_usd NUMERIC(14, 4) NOT NULL CHECK (limit_usd > 0),
    soft_threshold_pct SMALLINT NOT NULL DEFAULT 80
        CHECK (soft_threshold_pct BETWEEN 1 AND 100),
    action TEXT NOT NULL CHECK (action IN ('block', 'downgrade')),
    downgrade_ceiling_pct SMALLINT NOT NULL DEFAULT 125
        CHECK (downgrade_ceiling_pct >= 100),
    enabled BOOLEAN NOT NULL DEFAULT true,
    created_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT budgets_target_matches_scope CHECK (
        (scope = 'platform' AND target_id IS NULL)
        OR (scope IN ('user', 'agent') AND target_id IS NOT NULL)
    )
);

CREATE INDEX idx_budgets_scope_target ON budgets (scope, target_id) WHERE enabled;
CREATE TRIGGER trg_budgets_updated_at BEFORE UPDATE ON budgets
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- One row per (budget, period, kind). The UNIQUE constraint is the exactly-once
-- mechanism: every writer (router post-call, counter rebuild) inserts with
-- ON CONFLICT DO NOTHING, so concurrent replicas cannot duplicate an event.
CREATE TABLE budget_events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    budget_id UUID NOT NULL REFERENCES budgets (id) ON DELETE CASCADE,
    period_start TIMESTAMPTZ NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('soft_threshold', 'hard_limit')),
    spend_usd NUMERIC(14, 6) NOT NULL,
    limit_usd NUMERIC(14, 4) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (budget_id, period_start, kind)
);
