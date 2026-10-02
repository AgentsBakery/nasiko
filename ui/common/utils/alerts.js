/**
 * Pure alert view helpers: mapping spike-alert markers onto the TokenOps
 * spend chart, alert list queries, severity/kind/delivery chips, and monitor and
 * channel form validation and payload building. No DOM and no fetch so they run
 * under node:test. Strings from the API (titles, names) are untrusted: callers
 * must escape them before any HTML interpolation. Validation mirrors the server
 * rules for fast inline feedback only; the server remains the real gate.
 */

const HOUR_KEY_LEN = 13; // 'YYYY-MM-DDTHH'
const DAY_KEY_LEN = 10; // 'YYYY-MM-DD'
const NOTE_SEPARATOR = '; ';

/**
 * Map spike markers to `app-chart` anomaly entries on the Spend dataset.
 *
 * The server serialises timestamps as RFC 3339 UTC, so an ISO prefix is the
 * UTC bucket key; local time never enters. Markers with no matching point are
 * dropped and markers landing in one bucket collapse into a single anomaly.
 *
 * @param {Array<{bucket_start: string}>} points
 * @param {'hour'|'day'|string} bucket
 * @param {Array<{hour_start: string, title?: string}>} markers
 * @returns {Array<{index: number, note: string}>}
 */
export function markerAnomalies(points, bucket, markers) {
  if (!Array.isArray(points) || !Array.isArray(markers)) return [];
  const len = bucket === 'hour' ? HOUR_KEY_LEN : DAY_KEY_LEN;
  const key = (iso) => (typeof iso === 'string' ? iso.slice(0, len) : '');
  const indexByKey = new Map();
  points.forEach((p, i) => {
    const k = key(p?.bucket_start);
    if (k && !indexByKey.has(k)) indexByKey.set(k, i);
  });
  /** @type {Map<number, string[]>} */
  const notesByIndex = new Map();
  for (const m of markers) {
    const k = key(m?.hour_start);
    const i = k ? indexByKey.get(k) : undefined;
    if (i === undefined) continue;
    const notes = notesByIndex.get(i) ?? [];
    notes.push(String(m.title ?? 'Spend spike'));
    notesByIndex.set(i, notes);
  }
  return [...notesByIndex]
    .sort((a, b) => a[0] - b[0])
    .map(([index, notes]) => ({ index, note: notes.join(NOTE_SEPARATOR) }));
}

// ─── alert list ─────────────────────────────────────────────────────────────

const QUERY_KEYS = ['status', 'kind', 'severity', 'scope', 'since', 'until', 'limit', 'cursor'];

/**
 * Build the `GET /api/alerts` query string from filters, keeping non-empty keys.
 * @param {Record<string, any>} filters
 * @returns {string}
 */
export function alertQuery(filters) {
  const q = new URLSearchParams();
  for (const k of QUERY_KEYS) {
    const v = filters?.[k];
    if (v !== undefined && v !== null && v !== '') q.set(k, String(v));
  }
  return q.toString();
}

/**
 * Server-provided links are followed only when they are internal paths.
 * @param {unknown} link
 * @returns {boolean}
 */
export function isInternalLink(link) {
  return typeof link === 'string' && link.startsWith('/') && !link.startsWith('//');
}

/**
 * The internal budgets page an alert points at, or null when absent/external.
 * @param {{details?: {budget_url?: unknown}} | null | undefined} alert
 * @returns {string | null}
 */
export function budgetUrl(alert) {
  const url = alert?.details?.budget_url;
  return isInternalLink(url) ? /** @type {string} */ (url) : null;
}

const SEVERITY_TONES = { critical: 'error', warning: 'warning', info: 'info' };

/** @param {string} severity @returns {'error'|'warning'|'info'|'neutral'} */
export function severityTone(severity) {
  return Object.hasOwn(SEVERITY_TONES, severity) ? SEVERITY_TONES[severity] : 'neutral';
}

export const ALERT_KINDS = [
  { value: 'budget_soft', label: 'Budget soft threshold' },
  { value: 'budget_hard', label: 'Budget exhausted' },
  { value: 'spend_spike', label: 'Spend spike' },
  { value: 'monitor_breach', label: 'Monitor breach' },
];

export const SEVERITIES = [
  { value: 'info', label: 'Info' },
  { value: 'warning', label: 'Warning' },
  { value: 'critical', label: 'Critical' },
];

/** @param {string} kind @returns {string} */
export function kindLabel(kind) {
  return ALERT_KINDS.find((k) => k.value === kind)?.label ?? kind;
}

const SHORT_ID_LEN = 8;
const NAMED_SCOPES = new Set(['agent', 'user']);

/**
 * Display text for an alert/monitor scope reference. Agent and user refs are
 * ids, so they resolve to a name via `targetNames` (id -> name), falling back
 * to a shortened id. Callers must still escape the result.
 * @param {string} scope
 * @param {string|null|undefined} ref
 * @param {Map<string, string>} [targetNames]
 * @returns {string}
 */
export function scopeRefLabel(scope, ref, targetNames) {
  if (ref === null || ref === undefined || ref === '') return '';
  const id = String(ref);
  if (!NAMED_SCOPES.has(scope)) return id;
  return targetNames?.get(id) ?? id.slice(0, SHORT_ID_LEN);
}

const DELIVERY_TONES = { delivered: 'success', failed: 'error', pending: 'neutral', sending: 'info' };

/** @param {string} status @returns {'success'|'error'|'neutral'|'info'} */
export function deliveryStatusTone(status) {
  return Object.hasOwn(DELIVERY_TONES, status) ? DELIVERY_TONES[status] : 'neutral';
}

// ─── monitors ───────────────────────────────────────────────────────────────

const MAX_NAME_LEN = 120;
const MIN_WINDOW_MINUTES = 5;
const MAX_WINDOW_MINUTES = 1440;
const MAX_ERROR_RATE_PCT = 100;
const MIN_SAMPLES = 1;

export const METRICS = [
  { value: 'error_rate', label: 'Error rate (%)' },
  { value: 'p95_latency_ms', label: 'p95 latency (ms)' },
];

export const MONITOR_SCOPES = [
  { value: 'platform', label: 'Platform' },
  { value: 'agent', label: 'Agent' },
  { value: 'model', label: 'Model' },
];

const isBlank = (v) => String(v ?? '').trim() === '';

/**
 * @param {Record<string, any>} form
 * @returns {Record<string, string>} field-keyed messages; `{}` when valid
 */
export function validateMonitorForm(form) {
  /** @type {Record<string, string>} */
  const errors = {};
  const name = String(form.name ?? '').trim();
  if (!name) errors.name = 'Name is required';
  else if (name.length > MAX_NAME_LEN) errors.name = `Name must be at most ${MAX_NAME_LEN} characters`;
  if (form.scope === 'agent' && isBlank(form.scope_ref)) errors.scope_ref = 'Choose an agent';
  if (form.scope === 'model' && isBlank(form.scope_ref)) errors.scope_ref = 'Enter a model name';
  const win = Number(form.window_minutes);
  if (isBlank(form.window_minutes) || !Number.isInteger(win) || win < MIN_WINDOW_MINUTES || win > MAX_WINDOW_MINUTES) {
    errors.window_minutes = `Window must be a whole number between ${MIN_WINDOW_MINUTES} and ${MAX_WINDOW_MINUTES} minutes`;
  }
  const threshold = Number(form.threshold);
  if (isBlank(form.threshold) || !Number.isFinite(threshold) || threshold <= 0) {
    errors.threshold = 'Threshold must be greater than 0';
  } else if (form.metric === 'error_rate' && threshold > MAX_ERROR_RATE_PCT) {
    errors.threshold = `Error rate threshold cannot exceed ${MAX_ERROR_RATE_PCT}%`;
  }
  const samples = Number(form.min_samples);
  if (isBlank(form.min_samples) || !Number.isInteger(samples) || samples < MIN_SAMPLES) {
    errors.min_samples = `Minimum samples must be a whole number of at least ${MIN_SAMPLES}`;
  }
  return errors;
}

/** @param {Record<string, any>} form */
export function monitorPayload(form) {
  const payload = {
    name: String(form.name ?? '').trim(),
    metric: form.metric,
    scope: form.scope,
    scope_ref: form.scope === 'platform' ? null : String(form.scope_ref ?? '').trim() || null,
    window_minutes: Number(form.window_minutes),
    threshold: Number(form.threshold),
    min_samples: Number(form.min_samples),
    severity: form.severity,
  };
  return form.enabled === undefined ? payload : { ...payload, enabled: Boolean(form.enabled) };
}

// ─── channels ───────────────────────────────────────────────────────────────

export const CHANNEL_KINDS = [
  { value: 'webhook', label: 'Webhook' },
  { value: 'slack', label: 'Slack' },
];

/** Whether `url` is an acceptable channel URL (https; http only when allowed). */
function urlProblem(url, allowInsecure) {
  let u;
  try { u = new URL(url); } catch { return 'Enter a valid URL'; }
  if (u.protocol === 'https:') return '';
  if (u.protocol === 'http:' && allowInsecure) return '';
  return allowInsecure ? 'URL must start with http:// or https://' : 'URL must start with https://';
}

/**
 * @param {Record<string, any>} form
 * @param {{ isEdit: boolean, allowInsecure?: boolean }} opts
 * @returns {Record<string, string>}
 */
export function validateChannelForm(form, { isEdit, allowInsecure = false }) {
  /** @type {Record<string, string>} */
  const errors = {};
  const name = String(form.name ?? '').trim();
  if (!name) errors.name = 'Name is required';
  else if (name.length > MAX_NAME_LEN) errors.name = `Name must be at most ${MAX_NAME_LEN} characters`;
  if (!isEdit && !form.kind) errors.kind = 'Choose a kind';
  const url = String(form.url ?? '').trim();
  if (url || !isEdit) {
    const problem = url ? urlProblem(url, allowInsecure) : 'URL is required';
    if (problem) errors.url = problem;
  }
  if (form.kind === 'slack' && !isBlank(form.hmac_secret)) {
    errors.hmac_secret = 'Slack channels do not support an HMAC secret';
  }
  return errors;
}

/**
 * Create sends url and secret only when non-empty. Edit omits `kind`, omits an
 * empty url (unchanged), and maps `hmac_mode`: keep omits the secret, clear
 * sends an empty string, set sends the new value.
 * @param {Record<string, any>} form
 * @param {{ isEdit: boolean }} opts
 */
export function channelPayload(form, { isEdit }) {
  const url = String(form.url ?? '').trim();
  const secret = String(form.hmac_secret ?? '');
  /** @type {Record<string, any>} */
  const payload = { name: String(form.name ?? '').trim() };
  if (!isEdit) payload.kind = form.kind;
  if (url) payload.url = url;
  if (form.enabled !== undefined) payload.enabled = Boolean(form.enabled);
  if (isEdit) {
    if (form.hmac_mode === 'clear') payload.hmac_secret = '';
    else if (form.hmac_mode === 'set' && secret) payload.hmac_secret = secret;
  } else if (secret) {
    payload.hmac_secret = secret;
  }
  return payload;
}

/**
 * @param {Array<{alert_kind?: string|null, min_severity: string}>} rows
 * @returns {{ routes: Array<{alert_kind: string|null, min_severity: string}> }}
 */
export function routesPayload(rows) {
  const seen = new Set();
  const routes = [];
  for (const r of rows ?? []) {
    const kind = !r.alert_kind || r.alert_kind === 'any' ? null : r.alert_kind;
    const key = `${kind}|${r.min_severity}`;
    if (seen.has(key)) continue;
    seen.add(key);
    routes.push({ alert_kind: kind, min_severity: r.min_severity });
  }
  return { routes };
}
