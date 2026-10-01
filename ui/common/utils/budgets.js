/**
 * Pure helpers for the Budgets page: state chips, money/percent formatting,
 * form validation and request-payload building.
 *
 * No DOM and no window access so they run under node:test. Strings returned
 * from the API (names, targets) are untrusted: callers must still escape them
 * before rendering. Validation here mirrors the server rules for fast inline
 * feedback only; the server remains the real gate.
 */

const EM_DASH = '—';
const TINY_USD_THRESHOLD = 0.01;
const MIN_LIMIT_USD = 0.0001;
const MIN_SOFT_PCT = 1;
const MAX_SOFT_PCT = 100;
const MIN_CEILING_PCT = 100;

export const SCOPES = [
  { value: 'user', label: 'User' },
  { value: 'agent', label: 'Agent' },
  { value: 'platform', label: 'Platform' },
];

export const PERIODS = [
  { value: 'daily', label: 'Daily' },
  { value: 'weekly', label: 'Weekly' },
  { value: 'monthly', label: 'Monthly' },
];

export const ACTIONS = [
  { value: 'block', label: 'Block' },
  { value: 'downgrade', label: 'Downgrade' },
];

const STATE_VARIANTS = {
  ok: 'success',
  soft: 'warning',
  downgrading: 'info',
  blocked: 'error',
  disabled: 'neutral',
  unknown: 'neutral',
};

const STATE_LABELS = {
  ok: 'OK',
  soft: 'Soft limit',
  downgrading: 'Downgrading',
  blocked: 'Blocked',
  disabled: 'Disabled',
  unknown: 'Unknown',
};

/** @param {string} state @returns {'success'|'warning'|'info'|'error'|'neutral'} */
export function stateBadgeVariant(state) {
  return Object.hasOwn(STATE_VARIANTS, state) ? STATE_VARIANTS[state] : 'neutral';
}

/** @param {string} state @returns {string} */
export function stateLabel(state) {
  return Object.hasOwn(STATE_LABELS, state) ? STATE_LABELS[state] : STATE_LABELS.unknown;
}

/** @param {number|null|undefined} v @returns {string} */
export function fmtUsd(v) {
  if (v === null || v === undefined || Number.isNaN(Number(v))) return EM_DASH;
  const n = Number(v);
  const digits = n > 0 && n < TINY_USD_THRESHOLD ? 4 : 2;
  return `$${n.toFixed(digits)}`;
}

/** @param {number|null|undefined} v @returns {string} */
export function fmtPct(v) {
  if (v === null || v === undefined || Number.isNaN(Number(v))) return EM_DASH;
  return `${Number(Number(v).toFixed(1))}%`;
}

/** @param {number|null|undefined} pct @returns {number} */
export function progressValue(pct) {
  if (pct === null || pct === undefined || Number.isNaN(Number(pct))) return 0;
  return Math.min(100, Math.max(0, Number(pct)));
}

/** Platform rows from /api/budgets/me carry no dollar fields for members. */
export function isRedacted(row) {
  return row?.scope === 'platform' && !('limit_usd' in row);
}

/**
 * @param {Record<string, any>} form
 * @returns {Record<string, string>} field-keyed messages; `{}` when valid
 */
export function validateBudgetForm(form) {
  /** @type {Record<string, string>} */
  const errors = {};
  if (!String(form.name ?? '').trim()) errors.name = 'Name is required';
  const limit = Number(form.limit_usd);
  if (String(form.limit_usd ?? '').trim() === '' || !Number.isFinite(limit) || limit < MIN_LIMIT_USD) {
    errors.limit_usd = `Limit must be a number of at least ${MIN_LIMIT_USD}`;
  }
  const soft = Number(form.soft_threshold_pct);
  if (!Number.isFinite(soft) || soft < MIN_SOFT_PCT || soft > MAX_SOFT_PCT) {
    errors.soft_threshold_pct = `Threshold must be between ${MIN_SOFT_PCT} and ${MAX_SOFT_PCT}`;
  }
  const ceil = Number(form.downgrade_ceiling_pct);
  if (!Number.isFinite(ceil) || ceil < MIN_CEILING_PCT) {
    errors.downgrade_ceiling_pct = `Ceiling must be at least ${MIN_CEILING_PCT}`;
  }
  const hasTarget = String(form.target_id ?? '').trim() !== '';
  if (form.scope === 'platform') {
    if (hasTarget) errors.target_id = 'Platform budgets have no target';
  } else if (!hasTarget) {
    errors.target_id = 'Choose a target';
  }
  return errors;
}

/**
 * @param {Record<string, any>} form
 * @param {{ update?: boolean }} [opts] update payloads omit the immutable scope and target
 */
export function budgetFormToPayload(form, opts = {}) {
  const payload = {
    name: String(form.name ?? '').trim(),
    period: form.period,
    limit_usd: Number(form.limit_usd),
    soft_threshold_pct: Number(form.soft_threshold_pct),
    action: form.action,
    downgrade_ceiling_pct: Number(form.downgrade_ceiling_pct),
  };
  if (opts.update) return payload;
  return {
    ...payload,
    scope: form.scope,
    target_id: form.scope === 'platform' ? null : String(form.target_id ?? '').trim() || null,
  };
}
