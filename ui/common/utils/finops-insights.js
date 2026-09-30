/**
 * Pure helpers for the admin-only TokenOps Insights panel.
 *
 * No DOM and no window access so they run under node:test. The insights text
 * is LLM output and therefore untrusted: this module only normalises shape,
 * the page is responsible for escaping before it renders.
 */

export const INSIGHTS_NOT_CONFIGURED_MESSAGE = 'Insights need an LLM configured (OPENAI_API_KEY)';

const MAX_AGENT_COSTS = 20;
const BULLET_PREFIX_RE = /^\s*•\s*/;

/**
 * @param {{ data?: { available?: boolean, insights?: string[] } } | null | undefined} resp
 * @returns {{ kind: 'bullets', items: string[] } | { kind: 'not_configured', message: string } | { kind: 'empty' }}
 */
export function insightsViewModel(resp) {
  const data = resp?.data;
  if (!data) return { kind: 'empty' };
  if (data.available === false) {
    return { kind: 'not_configured', message: INSIGHTS_NOT_CONFIGURED_MESSAGE };
  }
  const items = (Array.isArray(data.insights) ? data.insights : [])
    .map((s) => String(s).replace(BULLET_PREFIX_RE, '').trim())
    .filter(Boolean);
  return items.length ? { kind: 'bullets', items } : { kind: 'empty' };
}

/**
 * Request body for POST /observability/finops/insights: the top spenders only,
 * trimmed to the fields the prompt uses.
 *
 * @param {object | null | undefined} kpis
 * @param {Array<Record<string, any>> | null | undefined} rows
 */
export function insightsRequestBody(kpis, rows) {
  const agentCosts = [...(Array.isArray(rows) ? rows : [])]
    .sort((a, b) => (Number(b.total_cost) || 0) - (Number(a.total_cost) || 0))
    .slice(0, MAX_AGENT_COSTS)
    .map((r) => ({
      agent_name: r.agent_name,
      total_cost: r.total_cost,
      operations: r.operations,
      total_tokens: r.total_tokens,
    }));
  return { kpi: kpis ?? {}, agent_costs: agentCosts };
}
