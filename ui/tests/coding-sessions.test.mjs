/**
 * Tests for the pure Coding sessions page helpers (row mapping, detail hrefs,
 * offset paging and unavailable detection). No DOM involved.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';

const M = await import('../common/utils/coding-sessions.js');
const U = await import('../common/utils/units.js');

const DASH = '—';

/** The DB-fallback shape: a session with no trace-store data. */
const emptySummary = () => ({
  id: 's-empty',
  session_id: 's-empty',
  agent_id: '',
  num_traces: null,
  start_time: null,
  end_time: null,
  duration_ms: null,
  first_input: null,
  last_output: null,
  token_usage: { total: null },
  trace_latency_ms_p50: null,
  trace_latency_ms_p99: null,
  cost_summary: { total: { cost: null } },
  session_annotations: [],
  session_annotation_summaries: [],
});

const fullSummary = () => ({
  id: 'sess-1',
  session_id: 'sess-1',
  agent_id: 'claude-code',
  num_traces: 7,
  start_time: '2026-10-01T10:00:00Z',
  end_time: '2026-10-01T10:05:00Z',
  duration_ms: 2300,
  token_usage: { total: 123456 },
  trace_latency_ms_p50: 980,
  trace_latency_ms_p99: 4200,
  cost_summary: { total: { cost: 0.0004 } },
});

test('toSessionRow maps a fully populated summary', () => {
  const row = M.toSessionRow(fullSummary());
  assert.equal(row.sessionId, 'sess-1');
  assert.equal(row.agent, 'claude-code');
  assert.equal(row.started, '2026-10-01T10:00:00Z');
  assert.equal(row.duration, U.fmtDuration(2300));
  assert.equal(row.traces, 7);
  assert.equal(row.tokens, U.fmtNumber(123456));
  assert.equal(row.p50, U.fmtDuration(980));
  assert.equal(row.p99, U.fmtDuration(4200));
  assert.equal(row.cost, U.fmtCurrency(0.0004));
  assert.equal(row.href, '/observability-session?session_id=sess-1');
});

test('toSessionRow renders the DB-fallback shape as dashes', () => {
  const row = M.toSessionRow(emptySummary());
  assert.equal(row.sessionId, 's-empty');
  assert.equal(row.agent, DASH);
  assert.equal(row.started, null);
  assert.equal(row.duration, DASH);
  assert.equal(row.traces, DASH);
  assert.equal(row.tokens, DASH);
  assert.equal(row.p50, DASH);
  assert.equal(row.p99, DASH);
  assert.equal(row.cost, DASH);
});

test('toSessionRow tolerates missing nested objects and null agent', () => {
  const row = M.toSessionRow({ session_id: 'bare', agent_id: null });
  assert.equal(row.agent, DASH);
  assert.equal(row.tokens, DASH);
  assert.equal(row.cost, DASH);
  assert.equal(row.started, null);
  assert.equal(row.href, '/observability-session?session_id=bare');
});

test('toSessionRow falls back to id when session_id is absent', () => {
  assert.equal(M.toSessionRow({ id: 'only-id' }).sessionId, 'only-id');
});

test('sessionDetailHref encodes the session id', () => {
  assert.equal(M.sessionDetailHref('a b/c'), '/observability-session?session_id=a%20b%2Fc');
  assert.equal(M.sessionDetailHref('x&y=<z>'), '/observability-session?session_id=x%26y%3D%3Cz%3E');
});

test('nextOffset reads the end cursor only when there is a next page', () => {
  assert.equal(M.nextOffset({ pagination: { has_next_page: true, end_cursor: '25' } }), 25);
  assert.equal(M.nextOffset({ pagination: { has_next_page: false, end_cursor: '25' } }), null);
  assert.equal(M.nextOffset({ pagination: { has_next_page: true, end_cursor: null } }), null);
  assert.equal(M.nextOffset({ pagination: { has_next_page: true } }), null);
  assert.equal(M.nextOffset({ pagination: { has_next_page: true, end_cursor: 'abc' } }), null);
  assert.equal(M.nextOffset({}), null);
  assert.equal(M.nextOffset(null), null);
});

test('isUnavailable detects the under-privileged body', () => {
  assert.equal(M.isUnavailable({ available: false }), true);
  assert.equal(M.isUnavailable({ data: { sessions: [] } }), false);
  assert.equal(M.isUnavailable(null), false);
});

test('sessionsFromResponse returns the sessions array or []', () => {
  const sessions = [fullSummary()];
  assert.deepEqual(M.sessionsFromResponse({ data: { sessions } }), sessions);
  assert.deepEqual(M.sessionsFromResponse({ data: { sessions: null } }), []);
  assert.deepEqual(M.sessionsFromResponse({ data: {} }), []);
  assert.deepEqual(M.sessionsFromResponse({ available: false }), []);
  assert.deepEqual(M.sessionsFromResponse(undefined), []);
});

test('toSessionRow carries the corrected flag only when the server set it', () => {
  assert.equal(M.toSessionRow({ ...fullSummary(), usage_corrected: true }).usageCorrected, true);
  assert.equal(M.toSessionRow(fullSummary()).usageCorrected, false);
  assert.equal(M.toSessionRow({ ...fullSummary(), usage_corrected: 'true' }).usageCorrected, false);
});
