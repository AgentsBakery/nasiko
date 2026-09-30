/**
 * Usage and TokenOps data functions.
 *
 * Split from `data-functions.js` (Phase 3B) — usage summary, history,
 * by-agent/model breakdowns, and FinOps dashboard.
 *
 * The `/observability/finops/*` family below matches the backend handoff
 * (`tokensopsapis.md`): all five endpoints, `qs()` dropping any param that is
 * `undefined`/`null`/`''` so a caller can pass a whole options object without
 * hand-building a query string per call site. Confirmed against the Rust
 * handler (`oss/server/src/observability/handler.rs`): `model`/`provider`
 * really do filter `trace_usage` on every endpoint below except
 * `spend-calendar`'s neighbours that never declared them; `org_unit` is
 * EE-only (resolved to `user_id`s by the EE FinOps scope resolver's
 * middleware). All five read handlers honour the `FinopsUserScope` extension
 * and scope results to the caller's accessible agents. `fetchFinopsInsights`
 * is the exception to "read": a superuser-only POST that spends the platform
 * LLM key, withheld from generated surfaces.
 * OSS ignores `org_unit` outright (no org hierarchy).
 */

import { fetchApi, postJson } from '/common/services/api.js';
import { registerAll } from '/common/core/data-sources.js';

const fetchUsageSummary = async () => {
  return fetchApi('/usage/summary');
};

const FINOPS_BASE = '/observability/finops';

/** Drop empty params rather than send literal "undefined"/"null" query values. */
function qs(params) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params || {})) {
    if (v !== undefined && v !== null && v !== '') q.set(k, String(v));
  }
  return q.size ? `?${q}` : '';
}

// TokenOps dashboard — GET /api/observability/finops/dashboard
// KPI strip + fleet summary + attribution rows in one payload. `range`
// (`24h`|`7d`|`30d`) wins over `start_time`/`end_time` when both are sent —
// callers pick one or the other, not both, but the backend's own precedence
// is `range` first either way.
// `myAgent: true` sends `my_agent=true`, which narrows the dashboard's agent
// list to agents the caller OWNS (`agents.owner_id = claims.sub` — see
// `get_agent_names` in `oss/server/src/observability/service.rs`). It is
// declared on `FinopsFilterParams`, so `spend-timeseries` deserializes it
// too — but only the dashboard handler reads it, so do NOT pass it there and
// pretend the series narrowed.
const fetchTokenopsDashboard = async ({
  range, startTime, endTime, agentId, model, view, provider, orgUnit, myAgent,
} = {}) => {
  const params = qs({
    range, start_time: startTime, end_time: endTime,
    agent_id: agentId, model, view, provider, org_unit: orgUnit,
    my_agent: myAgent ? 'true' : undefined,
  });
  return fetchApi(`${FINOPS_BASE}/dashboard${params}`);
};

// "Spend over time" — GET /api/observability/finops/spend-timeseries.
// Dollar-only (`points[].spend_usd`, `.operations`) — no percent-of-window
// view exists against this endpoint. Each point also carries `tool_calls` and
// the fleet-wide `p50/p95/p99_latency_ms` percentiles (migration 0015), which
// is what Overview's Activity and Latency panels plot.
// No `my_agent`/`org_unit`: this handler ignores both.
const fetchSpendTimeseries = async ({ range, startTime, endTime, agentId, model, provider } = {}) => {
  const params = qs({ range, start_time: startTime, end_time: endTime, agent_id: agentId, model, provider });
  return fetchApi(`${FINOPS_BASE}/spend-timeseries${params}`);
};

// Day-of-month heatmap — GET /api/observability/finops/spend-calendar.
// `month` ("YYYY-MM") is required. Registered for forward-compat; no panel
// consumes it yet (see tokenops-page.js header note).
const fetchSpendCalendar = async ({ month, range, agentId, model } = {}) => {
  const params = qs({ month, range, agent_id: agentId, model });
  return fetchApi(`${FINOPS_BASE}/spend-calendar${params}`);
};

// Click-a-day hourly drill-down — GET /api/observability/finops/spend-calendar/day.
// `date` ("YYYY-MM-DD") is required. Powers "Spend concentration": `hours`
// is the real 24-point curve, `top_agents`/`others_spend_usd` are the
// pre-computed legend — no client-side aggregation needed.
const fetchSpendCalendarDay = async ({ date, agentId, model, provider } = {}) => {
  const params = qs({ date, agent_id: agentId, model, provider });
  return fetchApi(`${FINOPS_BASE}/spend-calendar/day${params}`);
};

// Standalone Attributions table source — GET /api/observability/finops/attributions.
// Prefer this over `dashboard`'s embedded `data.attributions` once the table
// grows real server-side sort/pagination (so a sort click does not re-fetch
// the whole dashboard); registered now, not yet called — the table still
// sorts in-memory over the one dashboard payload.
const fetchFinopsAttributions = async ({
  range, startTime, endTime, agentId, model, view, sortBy, sortDir, limit, offset,
} = {}) => {
  const params = qs({
    range, start_time: startTime, end_time: endTime, agent_id: agentId, model, view,
    sort_by: sortBy, sort_dir: sortDir, limit, offset,
  });
  return fetchApi(`${FINOPS_BASE}/attributions${params}`);
};

const fetchUsageHistory = async (days = 7) => {
  return fetchApi(`/usage/history?days=${days}`);
};

// `page`/`limit` are optional — a caller that omits them (e.g. a dashboard's
// first render, before any pagination control exists) used to serialize the
// literal strings "undefined"/"NaN" into the query string, which the real
// backend's `i64` deserializer always 400s on. Default to a full first page.
const DEFAULT_PAGE_LIMIT = 20;

// `??` (not a default param) so an explicit `null` — e.g. `Query(..., [null,
// null])` to skip both positionally — is defaulted too, not just an omitted
// (`undefined`) argument.
const fetchUsageByAgent = async (query, page, limit) => {
  const p = page ?? 1;
  const l = limit ?? DEFAULT_PAGE_LIMIT;
  const params = new URLSearchParams({ q: query || '', limit: l, offset: (p - 1) * l });
  return fetchApi(`/usage/by-agent?${params}`);
};

const fetchUsageByModel = async (query, page, limit) => {
  const p = page ?? 1;
  const l = limit ?? DEFAULT_PAGE_LIMIT;
  const params = new URLSearchParams({ q: query || '', limit: l, offset: (p - 1) * l });
  return fetchApi(`/usage/by-model?${params}`);
};

const fetchFinopsInsights = async ({ kpi, agentCosts } = {}) => {
  return postJson(`${FINOPS_BASE}/insights`, { kpi: kpi ?? {}, agent_costs: agentCosts ?? [] });
};

registerAll({
  fetchUsageSummary, fetchTokenopsDashboard, fetchSpendTimeseries,
  fetchSpendCalendar, fetchSpendCalendarDay, fetchFinopsAttributions,
  fetchUsageHistory, fetchUsageByAgent, fetchUsageByModel, fetchFinopsInsights,
}, { replace: true });
