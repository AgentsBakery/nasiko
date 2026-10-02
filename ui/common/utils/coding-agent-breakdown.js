/**
 * Pure helpers for the per-agent breakdown of a coding session
 * (`GET /api/coding-sessions/{id}/agents`) and for the "Corrected" label on
 * legacy Codex figures.
 *
 * No DOM, no fetch and no window access so they run under node:test. Agent
 * types, names and task descriptions are user/agent-supplied: every string
 * returned here is plain text and callers must escape it before rendering.
 */
import { fmtNumber, fmtCurrency } from './units.js';

const EM_DASH = '—';
const LOWER_BOUND_PREFIX = '≥ ';
/** Shares are fractions (0..1); anything non-zero below this rounds to 0%. */
const MIN_VISIBLE_SHARE = 0.005;
const KIND_ATTRIBUTE = 'coding_agent.agent.kind';
const TYPE_ATTRIBUTE = 'coding_agent.agent.type';
const INTENT_HIDDEN_HINT = 'Task text hidden: content capture off';
const KNOWN_KINDS = new Set(['main', 'subagent', 'teammate']);

/** Same wording as docs/CODING_AGENT_TELEMETRY.md ("UI label"). */
export const CORRECTED_HINT =
  'Corrected: Codex cached input was counted twice by older CLIs. Figures shown exclude the duplicate.';

/**
 * @typedef {object} TokenCounts
 * @property {number} input
 * @property {number} output
 * @property {number} cache_read
 * @property {number} cache_creation
 */

/**
 * @typedef {object} AgentRow One row of `data.agents`.
 * @property {string} key
 * @property {'main'|'subagent'|'teammate'|'unknown'} kind
 * @property {string|null} agent_id
 * @property {string|null} agent_type
 * @property {string|null} name
 * @property {string|null} intent
 * @property {boolean} intent_hidden
 * @property {string} started_at
 * @property {string} ended_at
 * @property {number} runs
 * @property {number} llm_calls
 * @property {number} tool_calls
 * @property {TokenCounts} tokens
 * @property {number|null} cost_usd
 * @property {boolean} output_incomplete
 * @property {boolean} usage_corrected
 * @property {number} share Fraction of the session total (0..1).
 */

/**
 * @typedef {object} BreakdownData The `data` of the breakdown response.
 * @property {string} session_id
 * @property {string|null} source_agent_id
 * @property {'cost'|'tokens'} share_basis
 * @property {object} totals
 * @property {AgentRow[]} agents
 * @property {{subagents: object, teams: object}} capture
 */

/**
 * @typedef {object} BreakdownRow Display-ready row; strings are unescaped.
 * @property {string} key
 * @property {string} label
 * @property {'main'|'subagent'|'teammate'|'unknown'} kindBadge
 * @property {string} runsSuffix
 * @property {{text: string, hidden: boolean, hint?: string}} intent
 * @property {string|null} startedAt
 * @property {string|null} endedAt
 * @property {number} llmCalls
 * @property {number} toolCalls
 * @property {TokenCounts} tokens
 * @property {string} output Formatted output tokens, lower-bound marked.
 * @property {string} cost Formatted cost, lower-bound marked.
 * @property {string} share
 * @property {boolean} corrected
 */

/**
 * "22%", "<1%" for a non-zero share that rounds away, "—" when absent.
 * @param {number|null|undefined} share Fraction 0..1.
 * @returns {string}
 */
export function fmtShare(share) {
  if (typeof share !== 'number' || !Number.isFinite(share)) return EM_DASH;
  if (share > 0 && share < MIN_VISIBLE_SHARE) return '<1%';
  return `${Math.round(share * 100)}%`;
}

/**
 * Prefix a formatted figure with "≥ " when it is a lower bound. A dash stays
 * a dash: "≥ —" would claim a bound nobody measured.
 * @param {string} formatted
 * @param {boolean} incomplete
 * @returns {string}
 */
export function lowerBound(formatted, incomplete) {
  if (!incomplete || formatted === EM_DASH) return formatted;
  return `${LOWER_BOUND_PREFIX}${formatted}`;
}

/**
 * Intent cell under the content policy: the description only when it was
 * captured; otherwise the subagent type plus a hint, never the task text.
 * @param {Partial<AgentRow>} row
 * @returns {{text: string, hidden: boolean, hint?: string}}
 */
export function intentCell(row) {
  if (row?.kind === 'main') return { text: EM_DASH, hidden: false };
  if (row?.intent_hidden) {
    return { text: row.agent_type || EM_DASH, hidden: true, hint: INTENT_HIDDEN_HINT };
  }
  return { text: row?.intent || EM_DASH, hidden: false };
}

/** @param {Partial<AgentRow>} row */
function rowLabel(row) {
  switch (row.kind) {
    case 'main': return 'Main agent';
    case 'subagent': return row.agent_type ?? 'Subagent';
    case 'teammate': return row.name ?? row.agent_type ?? 'Teammate';
    default: return row.agent_type ?? 'Unknown agent';
  }
}

/** @param {any} t */
function tokenCounts(t) {
  return {
    input: Number(t?.input ?? 0),
    output: Number(t?.output ?? 0),
    cache_read: Number(t?.cache_read ?? 0),
    cache_creation: Number(t?.cache_creation ?? 0),
  };
}

/**
 * Map the breakdown payload to display rows, main agent first (the server
 * already orders by main, then start; the sort is stable and only re-asserts
 * the main-first rule).
 * @param {Partial<BreakdownData>|null|undefined} data
 * @returns {BreakdownRow[]}
 */
export function toBreakdownRows(data) {
  const agents = Array.isArray(data?.agents) ? data.agents : [];
  const ordered = [...agents].sort((a, b) =>
    Number(b?.kind === 'main') - Number(a?.kind === 'main'));
  return ordered.map((row) => {
    const incomplete = row.output_incomplete === true;
    const tokens = tokenCounts(row.tokens);
    const runs = Number(row.runs ?? 1);
    return {
      key: String(row.key ?? ''),
      label: rowLabel(row),
      kindBadge: KNOWN_KINDS.has(row.kind) ? row.kind : 'unknown',
      runsSuffix: runs > 1 ? `×${runs} runs` : '',
      intent: intentCell(row),
      startedAt: row.started_at ?? null,
      endedAt: row.ended_at ?? null,
      llmCalls: Number(row.llm_calls ?? 0),
      toolCalls: Number(row.tool_calls ?? 0),
      tokens,
      output: lowerBound(fmtNumber(tokens.output), incomplete),
      cost: lowerBound(row.cost_usd == null ? EM_DASH : fmtCurrency(row.cost_usd), incomplete),
      share: fmtShare(row.share),
      corrected: row.usage_corrected === true,
    };
  });
}

/** @param {any} s */
function subagentStatusText(s) {
  switch (s?.status) {
    case 'captured':
      return `Subagents: captured (${Number(s.captured ?? 0)} of ${Number(s.spawned ?? 0)})`;
    case 'partial':
      if (s.reason === 'cli_version_mixed') {
        return 'Subagents: partially captured (earlier turns came from a CLI version that cannot report them)';
      }
      return `Subagents: partially captured (${Number(s.captured ?? 0)} of ${Number(s.spawned ?? 0)} spawned)`;
    case 'not_captured':
      return 'Subagents: not captured (this CLI version cannot report them)';
    case 'not_applicable':
      return 'Subagents: not applicable for this agent';
    default:
      return 'Subagents: unknown';
  }
}

/** @param {any} t */
function teamStatusText(t) {
  switch (t?.status) {
    case 'captured': return 'Agent teams: captured';
    case 'no_activity': return 'Agent teams: no team activity';
    case 'not_captured': return 'Agent teams: not captured (not supported yet)';
    case 'not_applicable': return 'Agent teams: not applicable';
    default: return 'Agent teams: unknown';
  }
}

/**
 * Capture status in words, one sentence for subagents and one for teams.
 * @param {any} capture `data.capture`
 * @returns {{subagents: string, teams: string}}
 */
export function captureStatusText(capture) {
  return {
    subagents: subagentStatusText(capture?.subagents),
    teams: teamStatusText(capture?.teams),
  };
}

/**
 * @param {string} basis `data.share_basis`
 * @returns {string}
 */
export function shareBasisNote(basis) {
  return basis === 'cost'
    ? 'Share of total cost'
    : 'Share of total tokens (some calls unpriced)';
}

/**
 * Footnote explaining the "≥" marker; empty when no row is a lower bound.
 * @param {Partial<BreakdownData>|null|undefined} data
 * @returns {string}
 */
export function outputFootnote(data) {
  const agents = Array.isArray(data?.agents) ? data.agents : [];
  return agents.some((row) => row?.output_incomplete === true)
    ? 'Subagent output tokens are a lower bound: Claude Code records only partial output usage for subagents.'
    : '';
}

/** @param {unknown} attrs */
function parseAttributes(attrs) {
  if (attrs && typeof attrs === 'object') return /** @type {Record<string, unknown>} */ (attrs);
  if (typeof attrs !== 'string' || attrs === '') return {};
  try {
    const parsed = JSON.parse(attrs);
    return parsed && typeof parsed === 'object' ? parsed : {};
  } catch {
    return {};
  }
}

/**
 * Turn-navigator label for a subagent or teammate trace, from the flat root
 * span attributes (object or the JSON string `RootSpanEntry.attributes`
 * carries). Null for main/unscoped traces. Never uses an agent name: names
 * are content and are not exported as span attributes.
 * @param {unknown} rootAttrs
 * @returns {string|null}
 */
export function scopedTurnLabel(rootAttrs) {
  const attrs = parseAttributes(rootAttrs);
  const kind = attrs[KIND_ATTRIBUTE];
  if (typeof kind !== 'string' || kind === '' || kind === 'main') return null;
  const rawType = attrs[TYPE_ATTRIBUTE];
  const type = typeof rawType === 'string' && rawType !== '' ? rawType : null;
  if (kind === 'subagent') return type ? `${type} subagent` : 'Subagent';
  if (kind === 'teammate') return type ? `Teammate: ${type}` : 'Teammate';
  return 'Scoped agent';
}

/**
 * True only when the server flagged the figures as corrected (the flag is
 * omitted when false on session, trace, span and chat payloads).
 * @param {any} x
 * @returns {boolean}
 */
export function isCorrected(x) {
  return x?.usage_corrected === true;
}
