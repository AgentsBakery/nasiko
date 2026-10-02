/**
 * Tests for the pure alert view helpers (chart marker mapping). No DOM
 * involved. Bucket keys are UTC ISO-string prefixes, so results never depend
 * on the machine's local time zone.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';

const A = await import('../common/utils/alerts.js');

const hourPoints = [
  { bucket_start: '2026-10-01T10:00:00.000Z' },
  { bucket_start: '2026-10-01T11:00:00.000Z' },
];

test('markerAnomalies maps an hour marker to its hour bucket index', () => {
  const markers = [{ hour_start: '2026-10-01T11:00:00Z', title: 'Spike on agent-a' }];
  assert.deepEqual(A.markerAnomalies(hourPoints, 'hour', markers), [
    { index: 1, note: 'Spike on agent-a' },
  ]);
});

test('markerAnomalies collapses same-day markers and joins notes', () => {
  const points = [
    { bucket_start: '2026-09-30T00:00:00.000Z' },
    { bucket_start: '2026-10-01T00:00:00.000Z' },
  ];
  const markers = [
    { hour_start: '2026-10-01T03:00:00Z', title: 'one' },
    { hour_start: '2026-10-01T09:00:00Z', title: 'two' },
  ];
  assert.deepEqual(A.markerAnomalies(points, 'day', markers), [{ index: 1, note: 'one; two' }]);
});

test('markerAnomalies drops markers with no matching point', () => {
  const points = [{ bucket_start: '2026-09-28T00:00:00.000Z' }, { bucket_start: '2026-10-01T00:00:00.000Z' }];
  const markers = [{ hour_start: '2026-09-30T05:00:00Z', title: 'gap day' }];
  assert.deepEqual(A.markerAnomalies(points, 'day', markers), []);
});

test('markerAnomalies keys on the UTC day, not local time', () => {
  const points = [
    { bucket_start: '2026-10-01T00:00:00.000Z' },
    { bucket_start: '2026-10-02T00:00:00.000Z' },
  ];
  const markers = [{ hour_start: '2026-10-01T23:30:00Z', title: 'late' }];
  assert.deepEqual(A.markerAnomalies(points, 'day', markers), [{ index: 0, note: 'late' }]);
});

test('markerAnomalies tolerates empty and non-array input', () => {
  assert.deepEqual(A.markerAnomalies([], 'hour', []), []);
  assert.deepEqual(A.markerAnomalies(null, 'hour', [{ hour_start: 'x', title: 't' }]), []);
  assert.deepEqual(A.markerAnomalies(hourPoints, 'hour', undefined), []);
  assert.deepEqual(A.markerAnomalies(hourPoints, 'hour', [{ title: 'no time' }]), []);
});

// ─── view helpers (alerts page) ─────────────────────────────────────────────

test('alertQuery keeps only non-empty keys', () => {
  const q = A.alertQuery({
    status: 'open', kind: '', severity: 'critical', scope: undefined,
    since: '2026-10-01T00:00:00Z', limit: 50, cursor: 'abc',
  });
  assert.deepEqual(Object.fromEntries(new URLSearchParams(q)), {
    status: 'open', severity: 'critical', since: '2026-10-01T00:00:00Z', limit: '50', cursor: 'abc',
  });
  assert.equal(A.alertQuery({}), '');
});

test('severityTone maps to badge variants', () => {
  assert.equal(A.severityTone('critical'), 'error');
  assert.equal(A.severityTone('warning'), 'warning');
  assert.equal(A.severityTone('info'), 'info');
  assert.equal(A.severityTone('weird'), 'neutral');
});

test('kindLabel names known kinds and passes unknown through', () => {
  assert.equal(A.kindLabel('budget_soft'), 'Budget soft threshold');
  assert.equal(A.kindLabel('budget_hard'), 'Budget exhausted');
  assert.equal(A.kindLabel('spend_spike'), 'Spend spike');
  assert.equal(A.kindLabel('monitor_breach'), 'Monitor breach');
  assert.equal(A.kindLabel('other_kind'), 'other_kind');
});

const goodMonitor = {
  name: 'API errors', metric: 'error_rate', scope: 'agent', scope_ref: 'a1',
  window_minutes: '15', threshold: '10', min_samples: '20', severity: 'warning',
};

test('validateMonitorForm accepts valid forms', () => {
  assert.deepEqual(A.validateMonitorForm(goodMonitor), {});
  assert.deepEqual(A.validateMonitorForm({ ...goodMonitor, scope: 'model', scope_ref: 'gpt-4o' }), {});
  assert.deepEqual(A.validateMonitorForm({ ...goodMonitor, scope: 'platform', scope_ref: 'ignored' }), {});
});

test('validateMonitorForm reports field errors', () => {
  const bad = (patch) => A.validateMonitorForm({ ...goodMonitor, ...patch });
  assert.ok(bad({ name: '  ' }).name);
  assert.ok(bad({ name: 'x'.repeat(121) }).name);
  assert.ok(bad({ scope: 'agent', scope_ref: '' }).scope_ref);
  assert.ok(bad({ scope: 'model', scope_ref: ' ' }).scope_ref);
  assert.ok(bad({ window_minutes: '4' }).window_minutes);
  assert.ok(bad({ window_minutes: '1441' }).window_minutes);
  assert.ok(bad({ window_minutes: '10.5' }).window_minutes);
  assert.ok(bad({ threshold: '0' }).threshold);
  assert.ok(bad({ metric: 'error_rate', threshold: '100.5' }).threshold);
  assert.equal(bad({ metric: 'p95_latency_ms', threshold: '2500' }).threshold, undefined);
  assert.ok(bad({ min_samples: '0' }).min_samples);
});

test('monitorPayload parses numbers and drops platform scope_ref', () => {
  assert.deepEqual(A.monitorPayload({ ...goodMonitor, name: '  API errors  ', enabled: true }), {
    name: 'API errors', metric: 'error_rate', scope: 'agent', scope_ref: 'a1',
    window_minutes: 15, threshold: 10, min_samples: 20, severity: 'warning', enabled: true,
  });
  const p = A.monitorPayload({ ...goodMonitor, scope: 'platform', scope_ref: 'x' });
  assert.equal(p.scope_ref, null);
});

test('validateChannelForm requires https url on create and rejects http by default', () => {
  const ok = { name: 'ops', kind: 'webhook', url: 'https://example.com/hook', hmac_secret: '' };
  assert.deepEqual(A.validateChannelForm(ok, { isEdit: false }), {});
  assert.ok(A.validateChannelForm({ ...ok, name: '' }, { isEdit: false }).name);
  assert.ok(A.validateChannelForm({ ...ok, kind: '' }, { isEdit: false }).kind);
  assert.ok(A.validateChannelForm({ ...ok, url: '' }, { isEdit: false }).url);
  assert.ok(A.validateChannelForm({ ...ok, url: 'http://example.com/h' }, { isEdit: false }).url);
  assert.deepEqual(
    A.validateChannelForm({ ...ok, url: 'http://example.com/h' }, { isEdit: false, allowInsecure: true }), {});
  assert.ok(A.validateChannelForm({ ...ok, kind: 'slack', hmac_secret: 's' }, { isEdit: false }).hmac_secret);
});

test('validateChannelForm edit allows an empty url (unchanged)', () => {
  const form = { name: 'ops', kind: 'webhook', url: '', hmac_secret: '' };
  assert.deepEqual(A.validateChannelForm(form, { isEdit: true }), {});
  assert.ok(A.validateChannelForm({ ...form, url: 'ftp://x' }, { isEdit: true }).url);
});

test('channelPayload create includes url and secret only when present', () => {
  assert.deepEqual(
    A.channelPayload({ name: ' ops ', kind: 'webhook', url: 'https://e.com/h', hmac_secret: '', enabled: true }, { isEdit: false }),
    { name: 'ops', kind: 'webhook', url: 'https://e.com/h', enabled: true });
  assert.equal(
    A.channelPayload({ name: 'o', kind: 'webhook', url: 'https://e.com/h', hmac_secret: 'sec' }, { isEdit: false }).hmac_secret,
    'sec');
});

test('channelPayload edit omits unchanged url and honours keep/clear for the secret', () => {
  const base = { name: 'ops', kind: 'webhook', url: '', hmac_secret: '', hmac_mode: 'keep' };
  const keep = A.channelPayload(base, { isEdit: true });
  assert.ok(!('url' in keep) && !('hmac_secret' in keep) && !('kind' in keep));
  assert.equal(A.channelPayload({ ...base, hmac_mode: 'clear' }, { isEdit: true }).hmac_secret, '');
  const set = A.channelPayload({ ...base, url: 'https://e.com/n', hmac_mode: 'set', hmac_secret: 'new' }, { isEdit: true });
  assert.equal(set.url, 'https://e.com/n');
  assert.equal(set.hmac_secret, 'new');
});

test('routesPayload maps any to null and removes exact duplicates', () => {
  assert.deepEqual(A.routesPayload([
    { alert_kind: '', min_severity: 'warning' },
    { alert_kind: 'any', min_severity: 'warning' },
    { alert_kind: 'budget_hard', min_severity: 'critical' },
    { alert_kind: 'budget_hard', min_severity: 'critical' },
  ]), { routes: [
    { alert_kind: null, min_severity: 'warning' },
    { alert_kind: 'budget_hard', min_severity: 'critical' },
  ] });
});

test('deliveryStatusTone maps statuses', () => {
  assert.equal(A.deliveryStatusTone('delivered'), 'success');
  assert.equal(A.deliveryStatusTone('failed'), 'error');
  assert.equal(A.deliveryStatusTone('pending'), 'neutral');
  assert.equal(A.deliveryStatusTone('sending'), 'info');
  assert.equal(A.deliveryStatusTone('x'), 'neutral');
});

test('scopeRefLabel resolves agent and user names and shortens unknown ids', () => {
  const names = new Map([
    ['f7819085-e5cf-4000-8000-000000000000', 'Support bot'],
    ['0e34cb22-cace-40d4-acaa-380d81979a06', 'member1'],
  ]);
  assert.equal(A.scopeRefLabel('agent', 'f7819085-e5cf-4000-8000-000000000000', names), 'Support bot');
  assert.equal(A.scopeRefLabel('user', '0e34cb22-cace-40d4-acaa-380d81979a06', names), 'member1');
  assert.equal(A.scopeRefLabel('user', '9a9a9a9a-0000-4000-8000-000000000000', names), '9a9a9a9a');
  assert.equal(A.scopeRefLabel('agent', 'abcdef12-3456', names), 'abcdef12');
  assert.equal(A.scopeRefLabel('model', 'gpt-4o', names), 'gpt-4o');
  assert.equal(A.scopeRefLabel('platform', null, names), '');
});

test('isInternalLink accepts only single-slash internal paths', () => {
  assert.equal(A.isInternalLink('/x'), true);
  assert.equal(A.isInternalLink('//evil'), false);
  assert.equal(A.isInternalLink('https://x'), false);
  assert.equal(A.isInternalLink(null), false);
});

test('budgetUrl returns the internal budget path or null', () => {
  assert.equal(A.budgetUrl({ details: { budget_url: '/budgets' } }), '/budgets');
  assert.equal(A.budgetUrl({}), null);
  assert.equal(A.budgetUrl(null), null);
  assert.equal(A.budgetUrl({ details: { budget_url: 'https://evil' } }), null);
  assert.equal(A.budgetUrl({ details: { budget_url: '//evil' } }), null);
});
