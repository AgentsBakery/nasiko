/**
 * FinOps insights view-model (common/utils/finops-insights.js).
 *
 * The insights text comes from an LLM and the panel is admin-only, so the
 * helper must normalise every response shape (including a missing one) into a
 * small closed set the page can render without branching on raw JSON.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';

const M = await import('../common/utils/finops-insights.js');

test('available insights become bullets with the leading marker stripped', () => {
  const vm = M.insightsViewModel({
    data: { available: true, insights: ['• Spend rose 20%', '  Second'] },
  });
  assert.deepEqual(vm, { kind: 'bullets', items: ['Spend rose 20%', 'Second'] });
});

test('available:false maps to the not-configured message', () => {
  const vm = M.insightsViewModel({
    data: { available: false, reason: 'llm_not_configured', insights: [] },
  });
  assert.deepEqual(vm, {
    kind: 'not_configured',
    message: 'Insights need an LLM configured (OPENAI_API_KEY)',
  });
});

test('no insights and no response are empty', () => {
  assert.deepEqual(M.insightsViewModel({ data: { available: true, insights: [] } }), { kind: 'empty' });
  assert.deepEqual(M.insightsViewModel(null), { kind: 'empty' });
});

test('request body sorts by cost, trims to 20 and keeps only the four fields', () => {
  const rows = Array.from({ length: 25 }, (_, i) => ({
    agent_name: `a${i}`, total_cost: i, operations: 1, total_tokens: 2, secret: 'x',
  }));
  const body = M.insightsRequestBody({ total_cost: 5 }, rows);
  assert.deepEqual(body.kpi, { total_cost: 5 });
  assert.equal(body.agent_costs.length, 20);
  assert.equal(body.agent_costs[0].agent_name, 'a24');
  assert.deepEqual(Object.keys(body.agent_costs[0]).sort(),
    ['agent_name', 'operations', 'total_cost', 'total_tokens']);
  assert.deepEqual(M.insightsRequestBody(null, null), { kpi: {}, agent_costs: [] });
});
