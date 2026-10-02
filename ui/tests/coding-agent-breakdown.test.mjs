/**
 * Tests for the pure per-agent breakdown helpers used by the session detail
 * page (row mapping, intent policy, capture status wording, share basis,
 * scoped turn labels and the corrected flag). No DOM involved.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';

const M = await import('../common/utils/coding-agent-breakdown.js');
const U = await import('../common/utils/units.js');

const DASH = '—';

const tokens = (input, output, cacheRead = 0, cacheCreation = 0) => ({
  input, output, cache_read: cacheRead, cache_creation: cacheCreation,
});

const mainRow = () => ({
  key: 'main', kind: 'main', agent_id: null, agent_type: null, name: null,
  intent: null, intent_hidden: false,
  started_at: '2026-10-01T10:00:00Z', ended_at: '2026-10-01T10:05:00Z',
  runs: 1, llm_calls: 4, tool_calls: 6, tokens: tokens(1000, 200, 5000, 300),
  cost_usd: 0.78, cost_estimated: false, output_incomplete: false,
  usage_corrected: false, share: 0.78,
});

const subRow = () => ({
  key: 'subagent:a1', kind: 'subagent', agent_id: 'a1', agent_type: 'Explore', name: null,
  intent: 'find where budgets are enforced', intent_hidden: false,
  started_at: '2026-10-01T10:01:00Z', ended_at: '2026-10-01T10:02:00Z',
  runs: 3, llm_calls: 2, tool_calls: 3, tokens: tokens(400, 50, 100, 0),
  cost_usd: 0.22, cost_estimated: false, output_incomplete: true,
  usage_corrected: false, share: 0.22,
});

const teammateRow = () => ({
  key: 'teammate:t1', kind: 'teammate', agent_id: 't1', agent_type: 'reviewer', name: 'Rita',
  intent: null, intent_hidden: true,
  started_at: '2026-10-01T10:03:00Z', ended_at: '2026-10-01T10:04:00Z',
  runs: 1, llm_calls: 1, tool_calls: 0, tokens: tokens(10, 1),
  cost_usd: null, cost_estimated: false, output_incomplete: false,
  usage_corrected: true, share: 0.004,
});

const payload = (agents) => ({
  session_id: 's1', source_agent_id: 'claude', share_basis: 'cost',
  totals: { llm_calls: 0, tool_calls: 0, tokens: tokens(0, 0), cost_usd: null },
  agents,
  capture: {
    subagents: { status: 'captured', reason: null, spawned: 1, captured: 1 },
    teams: { status: 'not_captured', reason: 'agent_teams_unsupported' },
  },
});

// ─── toBreakdownRows ────────────────────────────────────────────────────────

test('toBreakdownRows puts the main row first and labels each kind', () => {
  const rows = M.toBreakdownRows(payload([subRow(), teammateRow(), mainRow()]));
  assert.deepEqual(rows.map((r) => r.label), ['Main agent', 'Explore', 'Rita']);
  assert.deepEqual(rows.map((r) => r.kindBadge), ['main', 'subagent', 'teammate']);
});

test('toBreakdownRows falls back to generic labels', () => {
  const sub = { ...subRow(), agent_type: null };
  const mate = { ...teammateRow(), name: null, agent_type: null };
  const typedMate = { ...teammateRow(), name: null };
  const rows = M.toBreakdownRows(payload([sub, mate, typedMate]));
  assert.deepEqual(rows.map((r) => r.label), ['Subagent', 'Teammate', 'reviewer']);
});

test('toBreakdownRows adds a runs suffix only for more than one run', () => {
  const rows = M.toBreakdownRows(payload([mainRow(), subRow()]));
  assert.equal(rows[0].runsSuffix, '');
  assert.equal(rows[1].runsSuffix, '×3 runs');
});

test('toBreakdownRows keeps tokens as numbers and formats share', () => {
  const rows = M.toBreakdownRows(payload([mainRow(), subRow(), teammateRow()]));
  assert.deepEqual(rows[0].tokens, { input: 1000, output: 200, cache_read: 5000, cache_creation: 300 });
  assert.equal(rows[0].share, '78%');
  assert.equal(rows[1].share, '22%');
  assert.equal(rows[2].share, '<1%');
});

test('toBreakdownRows marks lower bounds and missing cost', () => {
  const rows = M.toBreakdownRows(payload([mainRow(), subRow(), teammateRow()]));
  assert.equal(rows[0].output, U.fmtNumber(200));
  assert.equal(rows[0].cost, U.fmtCurrency(0.78));
  assert.equal(rows[1].output, `≥ ${U.fmtNumber(50)}`);
  assert.equal(rows[1].cost, `≥ ${U.fmtCurrency(0.22)}`);
  assert.equal(rows[2].cost, DASH);
  assert.equal(rows[2].corrected, true);
  assert.equal(rows[0].corrected, false);
});

test('toBreakdownRows tolerates a missing or malformed payload', () => {
  assert.deepEqual(M.toBreakdownRows(null), []);
  assert.deepEqual(M.toBreakdownRows({ agents: 'nope' }), []);
});

// ─── fmtShare / lowerBound ──────────────────────────────────────────────────

test('fmtShare rounds to whole percent with a <1% floor', () => {
  assert.equal(M.fmtShare(0), '0%');
  assert.equal(M.fmtShare(0.001), '<1%');
  assert.equal(M.fmtShare(0.005), '1%');
  assert.equal(M.fmtShare(1), '100%');
  assert.equal(M.fmtShare(null), DASH);
});

test('lowerBound prefixes only when incomplete and the value is known', () => {
  assert.equal(M.lowerBound('12', true), '≥ 12');
  assert.equal(M.lowerBound('12', false), '12');
  assert.equal(M.lowerBound(DASH, true), DASH);
});

// ─── intentCell ─────────────────────────────────────────────────────────────

test('intentCell shows the description when content capture was on', () => {
  assert.deepEqual(M.intentCell(subRow()), { text: 'find where budgets are enforced', hidden: false });
});

test('intentCell hides task text when content capture was off', () => {
  const hidden = { ...subRow(), intent: null, intent_hidden: true };
  assert.deepEqual(M.intentCell(hidden),
    { text: 'Explore', hidden: true, hint: 'Task text hidden: content capture off' });
  const untyped = { ...hidden, agent_type: null };
  assert.equal(M.intentCell(untyped).text, DASH);
});

test('intentCell is a dash for the main agent', () => {
  assert.deepEqual(M.intentCell(mainRow()), { text: DASH, hidden: false });
});

test('intentCell is a dash for a missing intent that is not hidden', () => {
  assert.deepEqual(M.intentCell({ ...subRow(), intent: null }), { text: DASH, hidden: false });
});

// ─── captureStatusText ──────────────────────────────────────────────────────

const subagents = (status, reason = null, spawned = null, captured = null) =>
  ({ subagents: { status, reason, spawned, captured }, teams: { status: 'not_applicable' } });

test('captureStatusText words every subagent status', () => {
  assert.equal(M.captureStatusText(subagents('captured', null, 4, 4)).subagents,
    'Subagents: captured (4 of 4)');
  assert.equal(M.captureStatusText(subagents('partial', null, 5, 3)).subagents,
    'Subagents: partially captured (3 of 5 spawned)');
  assert.equal(M.captureStatusText(subagents('partial', 'cli_version_mixed', 2, 1)).subagents,
    'Subagents: partially captured (earlier turns came from a CLI version that cannot report them)');
  assert.equal(M.captureStatusText(subagents('not_captured', 'cli_version')).subagents,
    'Subagents: not captured (this CLI version cannot report them)');
  assert.equal(M.captureStatusText(subagents('not_applicable', 'agent_unsupported')).subagents,
    'Subagents: not applicable for this agent');
  assert.equal(M.captureStatusText(subagents('weird')).subagents, 'Subagents: unknown');
});

test('captureStatusText words every team status', () => {
  const teams = (status) => M.captureStatusText({ subagents: {}, teams: { status } }).teams;
  assert.equal(teams('captured'), 'Agent teams: captured');
  assert.equal(teams('no_activity'), 'Agent teams: no team activity');
  assert.equal(teams('not_captured'), 'Agent teams: not captured (not supported yet)');
  assert.equal(teams('not_applicable'), 'Agent teams: not applicable');
  assert.equal(teams('weird'), 'Agent teams: unknown');
});

test('captureStatusText tolerates a missing capture block', () => {
  assert.deepEqual(M.captureStatusText(null),
    { subagents: 'Subagents: unknown', teams: 'Agent teams: unknown' });
});

// ─── notes ──────────────────────────────────────────────────────────────────

test('shareBasisNote names the share basis', () => {
  assert.equal(M.shareBasisNote('tokens'), 'Share of total tokens (some calls unpriced)');
  assert.equal(M.shareBasisNote('cost'), 'Share of total cost');
});

test('outputFootnote is set only when some row is a lower bound', () => {
  assert.equal(M.outputFootnote(payload([mainRow()])), '');
  assert.equal(M.outputFootnote(payload([mainRow(), subRow()])),
    'Subagent output tokens are a lower bound: Claude Code records only partial output usage for subagents.');
  assert.equal(M.outputFootnote(null), '');
});

// ─── scopedTurnLabel ────────────────────────────────────────────────────────

test('scopedTurnLabel is null for main or unscoped traces', () => {
  assert.equal(M.scopedTurnLabel(null), null);
  assert.equal(M.scopedTurnLabel({}), null);
  assert.equal(M.scopedTurnLabel({ 'coding_agent.agent.kind': 'main' }), null);
  assert.equal(M.scopedTurnLabel('not json'), null);
});

test('scopedTurnLabel labels subagents by type', () => {
  assert.equal(M.scopedTurnLabel({
    'coding_agent.agent.kind': 'subagent', 'coding_agent.agent.type': 'Explore',
  }), 'Explore subagent');
  assert.equal(M.scopedTurnLabel({ 'coding_agent.agent.kind': 'subagent' }), 'Subagent');
});

test('scopedTurnLabel labels teammates without names', () => {
  assert.equal(M.scopedTurnLabel({ 'coding_agent.agent.kind': 'teammate' }), 'Teammate');
  assert.equal(M.scopedTurnLabel(JSON.stringify({
    'coding_agent.agent.kind': 'teammate', 'coding_agent.agent.type': 'reviewer',
    'coding_agent.agent.name': 'Rita',
  })), 'Teammate: reviewer');
});

test('scopedTurnLabel labels an unknown scoped kind generically', () => {
  assert.equal(M.scopedTurnLabel({ 'coding_agent.agent.kind': 'unknown' }), 'Scoped agent');
});

// ─── isCorrected / CORRECTED_HINT ───────────────────────────────────────────

test('isCorrected is true only for a literal true flag', () => {
  assert.equal(M.isCorrected({ usage_corrected: true }), true);
  assert.equal(M.isCorrected({ usage_corrected: 'true' }), false);
  assert.equal(M.isCorrected({}), false);
  assert.equal(M.isCorrected(null), false);
});

test('CORRECTED_HINT explains the correction', () => {
  assert.match(M.CORRECTED_HINT, /^Corrected: Codex cached input was counted twice/);
});
