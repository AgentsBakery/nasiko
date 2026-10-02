/**
 * Pure helpers for the Coding sessions page: map a session-list row to display
 * cells, build the session detail href, and read the offset-paging cursor.
 *
 * No DOM and no window access so they run under node:test. Strings returned
 * from the API (session ids, agent names) are untrusted: callers must still
 * escape them before rendering.
 */
import { fmtDuration, fmtNumber, fmtCurrency } from './units.js';

const EM_DASH = '—';
const SESSION_DETAIL_PATH = '/observability-session';

/**
 * @typedef {object} SessionRow
 * @property {string} sessionId
 * @property {string} agent
 * @property {string|null} started Raw ISO-8601 start, or null when unknown.
 * @property {string} duration
 * @property {number|string} traces
 * @property {string} tokens
 * @property {string} p50
 * @property {string} p99
 * @property {string} cost
 * @property {string} href
 */

/**
 * Detail page link for one session. The id is percent-encoded here; callers
 * still escape the result for the attribute context.
 * @param {string} sessionId
 * @returns {string}
 */
export function sessionDetailHref(sessionId) {
  return `${SESSION_DETAIL_PATH}?session_id=${encodeURIComponent(sessionId)}`;
}

/**
 * Map a SessionSummary from `/observability/session/list` to display cells.
 * `agent_id` on the wire is the agent NAME (empty when the agent is gone).
 * @param {any} summary
 * @returns {SessionRow}
 */
export function toSessionRow(summary) {
  const s = summary ?? {};
  const sessionId = String(s.session_id ?? s.id ?? '');
  return {
    sessionId,
    agent: s.agent_id ? String(s.agent_id) : EM_DASH,
    started: s.start_time ?? null,
    duration: fmtDuration(s.duration_ms),
    traces: s.num_traces ?? EM_DASH,
    tokens: fmtNumber(s.token_usage?.total),
    p50: fmtDuration(s.trace_latency_ms_p50),
    p99: fmtDuration(s.trace_latency_ms_p99),
    cost: fmtCurrency(s.cost_summary?.total?.cost),
    href: sessionDetailHref(sessionId),
  };
}

/**
 * Next offset from the list payload (`resp.data`), or null when there is no
 * further page or the cursor is not a number.
 * @param {any} data
 * @returns {number|null}
 */
export function nextOffset(data) {
  const pagination = data?.pagination;
  if (!pagination?.has_next_page) return null;
  const cursor = pagination.end_cursor;
  if (cursor == null || cursor === '') return null;
  const offset = Number(cursor);
  return Number.isInteger(offset) && offset >= 0 ? offset : null;
}

/**
 * True for the `{ available: false }` body under-privileged reads return.
 * @param {any} resp
 * @returns {boolean}
 */
export function isUnavailable(resp) {
  return resp?.available === false;
}

/**
 * Session rows from a list response, or [] for any other shape.
 * @param {any} resp
 * @returns {any[]}
 */
export function sessionsFromResponse(resp) {
  const sessions = resp?.data?.sessions;
  return Array.isArray(sessions) ? sessions : [];
}
