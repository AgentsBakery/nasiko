/**
 * Tests for the pure Budgets page helpers (state chips, money/percent
 * formatting, form validation and payload building). No DOM involved.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';

const M = await import('../common/utils/budgets.js');

const validForm = () => ({
  name: 'Team cap',
  scope: 'user',
  target_id: 'u1',
  period: 'monthly',
  limit_usd: '10',
  soft_threshold_pct: '80',
  action: 'block',
  downgrade_ceiling_pct: '125',
});

test('stateBadgeVariant and stateLabel map every state', () => {
  assert.equal(M.stateBadgeVariant('ok'), 'success');
  assert.equal(M.stateBadgeVariant('soft'), 'warning');
  assert.equal(M.stateBadgeVariant('downgrading'), 'info');
  assert.equal(M.stateBadgeVariant('blocked'), 'error');
  assert.equal(M.stateBadgeVariant('disabled'), 'neutral');
  assert.equal(M.stateBadgeVariant('unknown'), 'neutral');
  assert.equal(M.stateBadgeVariant('weird'), 'neutral');
  assert.equal(M.stateLabel('ok'), 'OK');
  assert.equal(M.stateLabel('soft'), 'Soft limit');
  assert.equal(M.stateLabel('downgrading'), 'Downgrading');
  assert.equal(M.stateLabel('blocked'), 'Blocked');
  assert.equal(M.stateLabel('disabled'), 'Disabled');
  assert.equal(M.stateLabel('nope'), 'Unknown');
});

test('fmtUsd uses 4 decimals only for tiny positive values', () => {
  assert.equal(M.fmtUsd(12.3456), '$12.35');
  assert.equal(M.fmtUsd(0.0042), '$0.0042');
  assert.equal(M.fmtUsd(0), '$0.00');
  assert.equal(M.fmtUsd(null), '—');
  assert.equal(M.fmtUsd(undefined), '—');
});

test('fmtPct and progressValue', () => {
  assert.equal(M.fmtPct(50), '50%');
  assert.equal(M.fmtPct(123.456), '123.5%');
  assert.equal(M.fmtPct(null), '—');
  assert.equal(M.progressValue(150), 100);
  assert.equal(M.progressValue(-5), 0);
  assert.equal(M.progressValue(null), 0);
  assert.equal(M.progressValue(42), 42);
});

test('validateBudgetForm accepts a valid form', () => {
  assert.deepEqual(M.validateBudgetForm(validForm()), {});
});

test('validateBudgetForm reports field errors', () => {
  assert.ok(M.validateBudgetForm({ ...validForm(), name: '  ' }).name);
  assert.ok(M.validateBudgetForm({ ...validForm(), limit_usd: '0' }).limit_usd);
  assert.ok(M.validateBudgetForm({ ...validForm(), limit_usd: 'abc' }).limit_usd);
  assert.ok(M.validateBudgetForm({ ...validForm(), soft_threshold_pct: '0' }).soft_threshold_pct);
  assert.ok(M.validateBudgetForm({ ...validForm(), soft_threshold_pct: '101' }).soft_threshold_pct);
  assert.ok(M.validateBudgetForm({ ...validForm(), downgrade_ceiling_pct: '99' }).downgrade_ceiling_pct);
  assert.ok(M.validateBudgetForm({ ...validForm(), target_id: '' }).target_id);
  assert.ok(M.validateBudgetForm({ ...validForm(), scope: 'platform' }).target_id);
  assert.deepEqual(M.validateBudgetForm({ ...validForm(), scope: 'platform', target_id: '' }), {});
});

test('budgetFormToPayload trims, converts and nulls target for platform', () => {
  const p = M.budgetFormToPayload({ ...validForm(), name: '  Cap  ' });
  assert.equal(p.name, 'Cap');
  assert.equal(p.limit_usd, 10);
  assert.equal(p.soft_threshold_pct, 80);
  assert.equal(p.downgrade_ceiling_pct, 125);
  assert.equal(p.scope, 'user');
  assert.equal(p.target_id, 'u1');
  const plat = M.budgetFormToPayload({ ...validForm(), scope: 'platform' });
  assert.equal(plat.target_id, null);
});

test('budgetFormToPayload omits scope and target_id for updates', () => {
  const p = M.budgetFormToPayload(validForm(), { update: true });
  assert.ok(!('scope' in p));
  assert.ok(!('target_id' in p));
  assert.equal(p.period, 'monthly');
});

test('isRedacted detects platform rows without limit_usd', () => {
  assert.equal(M.isRedacted({ scope: 'platform' }), true);
  assert.equal(M.isRedacted({ scope: 'platform', limit_usd: 5 }), false);
  assert.equal(M.isRedacted({ scope: 'user' }), false);
});
