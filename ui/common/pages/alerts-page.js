/**
 * Alerts page (admin only): triage alerts, manage monitors, manage notification
 * channels.
 *
 * Three tabs. Alerts lists open/acknowledged/resolved alerts with filters,
 * cursor paging, an "Open" link into the linked view and Acknowledge. Monitors
 * manages error-rate and p95 latency monitors. Channels manages Slack and
 * webhook channels: routes, test sends and recent deliveries.
 *
 * The role split here is cosmetic. Non-admins see an admin-only notice and make
 * no data calls; the server answers 403 `admin_required` to every endpoint this
 * page uses. Alert titles/messages and monitor/channel names are user-supplied,
 * so every interpolated string goes through escHtml/escAttr. Channel secrets
 * (URL, HMAC secret) are write-only: only `url_hint` and `has_hmac_secret` are
 * ever displayed, and dialogs never pre-fill a secret.
 */
import { loadCss } from '/common/utils/css.js';
const styles = await loadCss(new URL('./alerts-page.css', import.meta.url));
import { escAttr, escHtml } from '/common/utils/escape.js';
import { toast } from '/common/utils/toast.js';
import { setFieldError, clearFieldErrors } from '/common/utils/field-error.js';
import '/common/design-system/app-badge/app-badge.js';
import '/common/design-system/app-button/app-button.js';
import '/common/design-system/app-empty-state/app-empty-state.js';
import '/common/design-system/app-alert/app-alert.js';
import '/common/design-system/app-input/app-input.js';
import '/common/design-system/app-select/app-select.js';
import '/common/design-system/app-switch/app-switch.js';
import '/common/design-system/app-tabs/app-tabs.js';
import { confirmDialog } from '/common/design-system/app-modal/app-modal.js';
import { errorStateHtml, bindRetry } from '/common/design-system/app-empty-state/error-state.js';
import { call } from '../core/data-sources.js';
import { navigate } from '../core/router.js';
import '/common/services/alerts-service.js';
import { authService } from '/common/services/auth-service.js';
import {
  ALERT_KINDS, CHANNEL_KINDS, METRICS, MONITOR_SCOPES, SEVERITIES,
  budgetUrl, channelPayload, deliveryStatusTone, isInternalLink, kindLabel, monitorPayload, routesPayload,
  scopeRefLabel, severityTone, validateChannelForm, validateMonitorForm,
} from '/common/utils/alerts.js';

document.adoptedStyleSheets = [...document.adoptedStyleSheets, styles];

const PAGE_SIZE = 25;
const DELIVERIES_LIMIT = 20;
const AGENT_PICKER_LIMIT = 200;
const DEFAULT_WINDOW_MINUTES = 15;
const DEFAULT_MIN_SAMPLES = 20;
const HTTP_TOO_MANY_REQUESTS = 429;
const HTTP_CONFLICT = 409;

const STATUS_FILTER = [
  { value: '', label: 'All statuses' },
  { value: 'open', label: 'Open' },
  { value: 'acknowledged', label: 'Acknowledged' },
  { value: 'resolved', label: 'Resolved' },
];
const KIND_FILTER = [{ value: '', label: 'All types' }, ...ALERT_KINDS];
const SEVERITY_FILTER = [{ value: '', label: 'All severities' }, ...SEVERITIES];
const SCOPE_FILTER = [
  { value: '', label: 'All scopes' },
  { value: 'platform', label: 'Platform' },
  { value: 'agent', label: 'Agent' },
  { value: 'model', label: 'Model' },
  { value: 'user', label: 'User' },
];
const ROUTE_KINDS = [{ value: 'any', label: 'Any type' }, ...ALERT_KINDS];
const HMAC_MODES = [
  { value: 'keep', label: 'Keep current secret' },
  { value: 'clear', label: 'Remove secret' },
  { value: 'set', label: 'Set a new secret' },
];

const MONITOR_CODE_TO_FIELD = {
  invalid_name: 'name',
  invalid_scope_ref: 'scope_ref',
  invalid_window: 'window_minutes',
  invalid_threshold: 'threshold',
  invalid_min_samples: 'min_samples',
};
const MONITOR_FIELDS = {
  name: '#f-name',
  scope_ref: '#f-ref',
  window_minutes: '#f-window',
  threshold: '#f-threshold',
  min_samples: '#f-samples',
};
const CHANNEL_CODE_TO_FIELD = {
  invalid_name: 'name',
  invalid_kind: 'kind',
  invalid_channel_url: 'url',
  hmac_not_supported: 'hmac_secret',
  invalid_hmac_secret: 'hmac_secret',
};
const CHANNEL_FIELDS = {
  name: '#f-name',
  kind: '#f-kind',
  url: '#f-url',
  hmac_secret: '#f-secret',
};

const asRows = (resp) => {
  if (Array.isArray(resp)) return resp;
  return Array.isArray(resp?.data) ? resp.data : [];
};

const fmtDate = (iso) => {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? '—' : d.toLocaleString();
};

const optionsAttr = (list) => `options='${escAttr(JSON.stringify(list))}'`;

const badge = (variant, text) =>
  `<app-badge variant="${escAttr(variant)}">${escHtml(text)}</app-badge>`;

class AlertsPage extends HTMLElement {
  #initialized = false;
  #loadId = 0;
  #alerts = [];
  #hasMore = false;
  #nextCursor = '';
  #monitors = [];
  #channels = [];
  #deliveries = [];
  #agents = [];
  #filters = { status: 'open', kind: '', severity: '', scope: '', since: '', until: '' };
  /** @type {Map<string, string>} channel id -> last test result text (with tone) */
  #testResults = new Map();

  connectedCallback() {
    if (this.#initialized) return;
    this.#initialized = true;
    this.innerHTML = `<div class="page-head"><h1 class="title-page">Alerts</h1></div>
      <div id="body"></div>`;
    authService.fetchCurrentUser().catch(() => null).then(() => {
      if (!this.isConnected) return;
      if (!authService.isAdmin()) {
        this.querySelector('#body').innerHTML = `<app-empty-state heading="Admin access required"
          description="Alerts, monitors and notification channels are managed by administrators."></app-empty-state>`;
        return;
      }
      this.#renderShell();
      this.#loadAll();
    });
  }

  disconnectedCallback() {
    // Listeners live on this element's own subtree and go with it; bumping the
    // id drops in-flight loads, and dialogs are removed so none outlive the page.
    this.#loadId += 1;
    for (const m of this.querySelectorAll('app-modal')) m.remove();
  }

  // ── shell ────────────────────────────────────────────────────────────────

  #renderShell() {
    const f = this.#filters;
    this.querySelector('#body').innerHTML = `
      <app-tabs label="Alert sections">
        <div data-tab="alerts" data-label="Alerts">
          <div class="filter-bar">
            <app-select id="flt-status" size="sm" aria-label="Status" ${optionsAttr(STATUS_FILTER)} value="${escAttr(f.status)}"></app-select>
            <app-select id="flt-kind" size="sm" aria-label="Type" ${optionsAttr(KIND_FILTER)} value="${escAttr(f.kind)}"></app-select>
            <app-select id="flt-severity" size="sm" aria-label="Severity" ${optionsAttr(SEVERITY_FILTER)} value="${escAttr(f.severity)}"></app-select>
            <app-select id="flt-scope" size="sm" aria-label="Scope" ${optionsAttr(SCOPE_FILTER)} value="${escAttr(f.scope)}"></app-select>
            <app-input id="flt-since" size="sm" type="date" aria-label="Since" label="Since"></app-input>
            <app-input id="flt-until" size="sm" type="date" aria-label="Until" label="Until"></app-input>
          </div>
          <div id="alerts-body"></div>
          <div class="more-row"><app-button variant="tertiary" size="md" id="more-btn" hidden>Load more</app-button></div>
        </div>
        <div data-tab="monitors" data-label="Monitors">
          <div class="panel-head">
            <p class="help">Error rate counts provider 4xx/5xx responses, timeouts and stream errors. Latency is end-to-end, including streaming.</p>
            <app-button variant="primary" size="md" id="new-monitor-btn">New monitor</app-button>
          </div>
          <div id="monitors-body"></div>
        </div>
        <div data-tab="channels" data-label="Channels">
          <div class="panel-head">
            <p class="help">Alerts are delivered to Slack and signed webhooks. URLs and secrets are never shown again after saving.</p>
            <app-button variant="primary" size="md" id="new-channel-btn">New channel</app-button>
          </div>
          <div id="channels-body"></div>
          <h2 class="section-title">Recent deliveries</h2>
          <div id="deliveries-body"></div>
        </div>
      </app-tabs>`;
    bindRetry(this, 'alerts-retry', () => this.#loadAll());

    const onFilter = (key, id) => {
      this.querySelector(id).addEventListener('change', (e) => {
        this.#filters[key] = e.target.value ?? '';
        this.#loadAlerts();
      });
    };
    onFilter('status', '#flt-status');
    onFilter('kind', '#flt-kind');
    onFilter('severity', '#flt-severity');
    onFilter('scope', '#flt-scope');
    this.querySelector('#flt-since').addEventListener('change', (e) => {
      this.#filters.since = e.target.value ? `${e.target.value}T00:00:00Z` : '';
      this.#loadAlerts();
    });
    this.querySelector('#flt-until').addEventListener('change', (e) => {
      this.#filters.until = e.target.value ? `${e.target.value}T23:59:59Z` : '';
      this.#loadAlerts();
    });
    this.querySelector('#more-btn').addEventListener('click', () => this.#loadAlerts({ more: true }));
    this.querySelector('#new-monitor-btn').addEventListener('click', () => this.#openMonitorDialog(null));
    this.querySelector('#new-channel-btn').addEventListener('click', () => this.#openChannelDialog(null));
  }

  #loadAll() {
    this.#loadAlerts();
    this.#loadMonitors();
    this.#loadChannels();
    this.#loadAgents();
  }

  async #loadAgents() {
    const resp = await call('fetchAgentList', { limit: AGENT_PICKER_LIMIT }).catch(() => null);
    this.#agents = asRows(resp).map((a) => ({ id: String(a.id), name: a.name || String(a.id) }));
    if (this.#monitors.length) this.#renderMonitors();
    if (this.#alerts.length) this.#renderAlerts();
  }

  // ── alerts tab ───────────────────────────────────────────────────────────

  async #loadAlerts({ more = false } = {}) {
    const id = ++this.#loadId;
    const body = this.querySelector('#alerts-body');
    try {
      const resp = await call('fetchAlerts', {
        ...this.#filters,
        limit: PAGE_SIZE,
        cursor: more ? this.#nextCursor : '',
      });
      if (id !== this.#loadId) return;
      const rows = asRows(resp);
      this.#alerts = more ? [...this.#alerts, ...rows] : rows;
      this.#hasMore = Boolean(resp?.has_more);
      this.#nextCursor = resp?.next_cursor ?? '';
      this.#renderAlerts();
    } catch (err) {
      if (id !== this.#loadId) return;
      body.innerHTML = errorStateHtml(err?.message || 'Could not load alerts');
    }
  }

  #renderAlerts() {
    const body = this.querySelector('#alerts-body');
    this.querySelector('#more-btn').hidden = !this.#hasMore;
    if (!this.#alerts.length) {
      body.innerHTML = `<app-empty-state heading="No alerts"
        description="No alerts match these filters."></app-empty-state>`;
      return;
    }
    const rows = this.#alerts.map((a) => `<tr>
        <td>${badge(severityTone(a.severity), a.severity)}</td>
        <td>${escHtml(kindLabel(a.kind))}</td>
        <td><div class="name">${escHtml(a.title)}</div>
          <div class="sub">${escHtml(a.message)}</div></td>
        <td>${escHtml(a.scope)}${a.scope_ref ? ` <span class="sub">${escHtml(scopeRefLabel(a.scope, a.scope_ref, this.#agentNames()))}</span>` : ''}</td>
        <td class="nowrap">${escHtml(fmtDate(a.first_seen_at))}<div class="sub">last ${escHtml(fmtDate(a.last_seen_at))}</div></td>
        <td class="num">${escHtml(String(a.occurrences ?? 1))}</td>
        <td>${badge(a.status === 'open' ? 'warning' : a.status === 'resolved' ? 'success' : 'neutral', a.status)}</td>
        <td><div class="row-actions">
          ${isInternalLink(a.link) ? `<app-button variant="tertiary" size="sm" data-open="${escAttr(a.id)}">Open</app-button>` : ''}
          ${budgetUrl(a) ? `<app-button variant="tertiary" size="sm" data-budget="${escAttr(a.id)}">Budget</app-button>` : ''}
          ${a.status === 'open' ? `<app-button variant="tertiary" size="sm" data-ack="${escAttr(a.id)}">Acknowledge</app-button>` : ''}
        </div></td>
      </tr>`).join('');
    body.innerHTML = `<div class="table-wrap"><table class="alerts-table">
      <thead><tr><th>Severity</th><th>Type</th><th>Alert</th><th>Scope</th><th>Seen</th>
        <th class="num">Count</th><th>Status</th><th></th></tr></thead>
      <tbody>${rows}</tbody></table></div>`;
    for (const btn of body.querySelectorAll('[data-open]')) {
      btn.addEventListener('click', () => {
        const alert = this.#alerts.find((a) => a.id === btn.dataset.open);
        if (alert && isInternalLink(alert.link)) navigate(alert.link);
      });
    }
    for (const btn of body.querySelectorAll('[data-budget]')) {
      btn.addEventListener('click', () => {
        const alert = this.#alerts.find((a) => a.id === btn.dataset.budget);
        const url = budgetUrl(alert);
        if (url) navigate(url);
      });
    }
    for (const btn of body.querySelectorAll('[data-ack]')) {
      btn.addEventListener('click', () => this.#acknowledge(btn.dataset.ack));
    }
  }

  async #acknowledge(id) {
    try {
      const resp = await call('acknowledgeAlert', { id });
      const updated = resp?.data ?? resp;
      this.#alerts = this.#alerts.map((a) => (a.id === id ? { ...a, ...updated } : a));
      this.#renderAlerts();
      toast.success('Alert acknowledged');
    } catch (err) {
      if (err?.status === HTTP_CONFLICT) {
        toast.error('This alert was already resolved');
        this.#loadAlerts();
      } else {
        toast.error(err?.message || 'Could not acknowledge alert');
      }
    }
  }

  // ── monitors tab ─────────────────────────────────────────────────────────

  async #loadMonitors() {
    const body = this.querySelector('#monitors-body');
    try {
      this.#monitors = asRows(await call('fetchMonitors'));
      this.#renderMonitors();
    } catch (err) {
      body.innerHTML = errorStateHtml(err?.message || 'Could not load monitors');
    }
  }

  #monitorTarget(m) {
    if (m.scope === 'platform') return 'Platform';
    const label = scopeRefLabel(m.scope, m.scope_ref, this.#agentNames());
    return m.scope === 'agent' ? `Agent: ${label}` : `Model: ${label}`;
  }

  #agentNames() {
    return new Map(this.#agents.map((a) => [a.id, a.name]));
  }

  #renderMonitors() {
    const body = this.querySelector('#monitors-body');
    if (!this.#monitors.length) {
      body.innerHTML = `<app-empty-state heading="No monitors yet"
        description="Create a monitor to be alerted when error rate or p95 latency crosses a threshold."></app-empty-state>`;
      return;
    }
    const rows = this.#monitors.map((m) => `<tr>
        <td class="name">${escHtml(m.name)}</td>
        <td>${escHtml(METRICS.find((x) => x.value === m.metric)?.label ?? m.metric)}</td>
        <td>${escHtml(this.#monitorTarget(m))}</td>
        <td class="num">${escHtml(String(m.window_minutes))} min</td>
        <td class="num">${escHtml(String(m.threshold))}${m.metric === 'error_rate' ? '%' : ' ms'}</td>
        <td class="num">${escHtml(String(m.min_samples))}</td>
        <td>${badge(severityTone(m.severity), m.severity)}</td>
        <td><app-switch size="sm" data-mon-toggle="${escAttr(m.id)}" aria-label="${escAttr(`Enabled: ${m.name}`)}"
          ${m.enabled ? 'checked' : ''}></app-switch></td>
        <td><div class="row-actions">
          <app-button variant="tertiary" size="sm" data-mon-edit="${escAttr(m.id)}">Edit</app-button>
          <app-button variant="tertiary" size="sm" data-mon-delete="${escAttr(m.id)}">Delete</app-button>
        </div></td>
      </tr>`).join('');
    body.innerHTML = `<div class="table-wrap"><table class="alerts-table">
      <thead><tr><th>Name</th><th>Metric</th><th>Target</th><th class="num">Window</th>
        <th class="num">Threshold</th><th class="num">Min samples</th><th>Severity</th>
        <th>Enabled</th><th></th></tr></thead>
      <tbody>${rows}</tbody></table></div>`;
    for (const sw of body.querySelectorAll('[data-mon-toggle]')) {
      sw.addEventListener('change', () => this.#toggleMonitor(sw));
    }
    for (const btn of body.querySelectorAll('[data-mon-edit]')) {
      btn.addEventListener('click', () => {
        this.#openMonitorDialog(this.#monitors.find((m) => m.id === btn.dataset.monEdit) ?? null);
      });
    }
    for (const btn of body.querySelectorAll('[data-mon-delete]')) {
      btn.addEventListener('click', () => {
        this.#removeMonitor(this.#monitors.find((m) => m.id === btn.dataset.monDelete));
      });
    }
  }

  async #toggleMonitor(sw) {
    const enabled = sw.checked;
    try {
      await call('updateMonitor', { id: sw.dataset.monToggle, enabled });
      toast.success(enabled ? 'Monitor enabled' : 'Monitor disabled');
      await this.#loadMonitors();
    } catch (err) {
      sw.checked = !enabled;
      toast.error(err?.message || 'Could not update monitor');
    }
  }

  async #removeMonitor(row) {
    if (!row) return;
    const ok = await confirmDialog({
      title: 'Delete monitor',
      message: `Delete "${row.name}"? It will stop evaluating and its open alert resolves.`,
      confirmLabel: 'Delete',
      danger: true,
    });
    if (!ok) return;
    try {
      await call('deleteMonitor', { id: row.id });
      toast.success('Monitor deleted');
      await this.#loadMonitors();
    } catch (err) {
      toast.error(err?.message || 'Could not delete monitor');
    }
  }

  #openMonitorDialog(row) {
    this.querySelector('#monitor-modal')?.remove();
    const scope = row?.scope ?? 'platform';
    const wrap = document.createElement('div');
    wrap.innerHTML = `
      <app-modal heading="${row ? 'Edit monitor' : 'New monitor'}" id="monitor-modal">
        <div class="modal-form">
          <app-alert id="f-alert" variant="destructive" hidden></app-alert>
          <app-input id="f-name" label="Name" required autocomplete="off"
            value="${escAttr(row?.name ?? '')}"></app-input>
          <div class="pair">
            <app-select id="f-metric" label="Metric" ${optionsAttr(METRICS)}
              value="${escAttr(row?.metric ?? 'error_rate')}"></app-select>
            <app-select id="f-severity" label="Severity" ${optionsAttr(SEVERITIES)}
              value="${escAttr(row?.severity ?? 'warning')}"></app-select>
          </div>
          <div class="pair">
            <app-select id="f-scope" label="Scope" ${optionsAttr(MONITOR_SCOPES)}
              value="${escAttr(scope)}"></app-select>
            <app-select id="f-ref-agent" label="Agent" placeholder="Choose…"
              ${optionsAttr(this.#agents.map((a) => ({ value: a.id, label: a.name })))}
              value="${escAttr(scope === 'agent' ? row?.scope_ref ?? '' : '')}"></app-select>
            <app-input id="f-ref-model" label="Model" autocomplete="off"
              value="${escAttr(scope === 'model' ? row?.scope_ref ?? '' : '')}"></app-input>
          </div>
          <div class="pair">
            <app-input id="f-window" label="Window (minutes)" type="number" min="5" max="1440" step="1" required
              value="${escAttr(row?.window_minutes ?? DEFAULT_WINDOW_MINUTES)}"></app-input>
            <app-input id="f-threshold" label="Threshold" type="number" min="0" step="any" required
              value="${escAttr(row?.threshold ?? '')}"></app-input>
          </div>
          <app-input id="f-samples" label="Minimum samples" type="number" min="1" step="1"
            hint="Windows with fewer calls than this are not evaluated."
            value="${escAttr(row?.min_samples ?? DEFAULT_MIN_SAMPLES)}"></app-input>
          <app-switch id="f-enabled" label="Enabled" ${row && !row.enabled ? '' : 'checked'}></app-switch>
        </div>
        <div data-slot="footer">
          <app-button variant="tertiary" size="md" id="f-cancel">Cancel</app-button>
          <app-button variant="primary" size="md" id="f-save">${row ? 'Save' : 'Create'}</app-button>
        </div>
      </app-modal>`;
    const modal = wrap.firstElementChild;
    this.appendChild(modal);
    const q = (sel) => modal.querySelector(sel);

    const syncScope = () => {
      const s = q('#f-scope').value;
      q('#f-ref-agent').hidden = s !== 'agent';
      q('#f-ref-model').hidden = s !== 'model';
    };
    q('#f-scope').addEventListener('change', syncScope);
    syncScope();

    q('#f-cancel').addEventListener('click', () => modal.close());
    q('#f-save').addEventListener('click', () => this.#saveMonitor(modal, row));
    modal.addEventListener('modal-close', () => modal.remove());
    modal.show();
  }

  async #saveMonitor(modal, row) {
    const q = (sel) => modal.querySelector(sel);
    const refField = () => (q('#f-scope').value === 'agent' ? q('#f-ref-agent') : q('#f-ref-model'));
    const fieldFor = (key) => (key === 'scope_ref' ? refField() : q(MONITOR_FIELDS[key]));
    clearFieldErrors(...Object.keys(MONITOR_FIELDS).map(fieldFor));
    const alert = q('#f-alert');
    alert.hidden = true;

    const scope = q('#f-scope').value;
    const form = {
      name: q('#f-name').value,
      metric: q('#f-metric').value,
      scope,
      scope_ref: scope === 'agent' ? q('#f-ref-agent').value : q('#f-ref-model').value,
      window_minutes: q('#f-window').value,
      threshold: q('#f-threshold').value,
      min_samples: q('#f-samples').value,
      severity: q('#f-severity').value,
      enabled: q('#f-enabled').checked,
    };
    const errors = validateMonitorForm(form);
    const failed = Object.keys(errors);
    if (failed.length) {
      for (const key of failed) setFieldError(fieldFor(key), errors[key]);
      return;
    }

    const save = q('#f-save');
    save.setAttribute('disabled', '');
    try {
      const payload = monitorPayload(form);
      if (row) await call('updateMonitor', { id: row.id, ...payload });
      else await call('createMonitor', payload);
      toast.success(row ? 'Monitor updated' : 'Monitor created');
      modal.close();
      await this.#loadMonitors();
    } catch (err) {
      const key = MONITOR_CODE_TO_FIELD[err?.code];
      if (key) {
        setFieldError(fieldFor(key), err.message || 'Invalid value');
      } else {
        alert.setAttribute('description', err?.message || 'Could not save monitor');
        alert.hidden = false;
      }
    } finally {
      save.removeAttribute('disabled');
    }
  }

  // ── channels tab ─────────────────────────────────────────────────────────

  async #loadChannels() {
    const body = this.querySelector('#channels-body');
    try {
      this.#channels = asRows(await call('fetchChannels'));
      this.#renderChannels();
    } catch (err) {
      body.innerHTML = errorStateHtml(err?.message || 'Could not load channels');
    }
    this.#loadDeliveries();
  }

  async #loadDeliveries() {
    const body = this.querySelector('#deliveries-body');
    try {
      this.#deliveries = asRows(await call('fetchDeliveries', { limit: DELIVERIES_LIMIT }));
      this.#renderDeliveries();
    } catch (err) {
      body.innerHTML = errorStateHtml(err?.message || 'Could not load deliveries', { retry: false });
    }
  }

  #renderChannels() {
    const body = this.querySelector('#channels-body');
    if (!this.#channels.length) {
      body.innerHTML = `<app-empty-state heading="No channels yet"
        description="Add a Slack or webhook channel to receive alert notifications."></app-empty-state>`;
      return;
    }
    const rows = this.#channels.map((c) => {
      const result = this.#testResults.get(c.id);
      return `<tr>
        <td class="name">${escHtml(c.name)}</td>
        <td>${escHtml(c.kind)}</td>
        <td class="mono">${escHtml(c.url_hint)}</td>
        <td>${c.kind === 'webhook' ? (c.has_hmac_secret ? 'Yes' : 'No') : '—'}</td>
        <td><app-switch size="sm" data-ch-toggle="${escAttr(c.id)}" aria-label="${escAttr(`Enabled: ${c.name}`)}"
          ${c.enabled ? 'checked' : ''}></app-switch></td>
        <td><div class="row-actions">
          ${result ? badge(result.tone, result.text) : ''}
          <app-button variant="tertiary" size="sm" data-ch-test="${escAttr(c.id)}">Send test</app-button>
          <app-button variant="tertiary" size="sm" data-ch-routes="${escAttr(c.id)}">Routes</app-button>
          <app-button variant="tertiary" size="sm" data-ch-edit="${escAttr(c.id)}">Edit</app-button>
          <app-button variant="tertiary" size="sm" data-ch-delete="${escAttr(c.id)}">Delete</app-button>
        </div></td>
      </tr>`;
    }).join('');
    body.innerHTML = `<div class="table-wrap"><table class="alerts-table">
      <thead><tr><th>Name</th><th>Kind</th><th>URL</th><th>HMAC secret</th><th>Enabled</th><th></th></tr></thead>
      <tbody>${rows}</tbody></table></div>`;
    const find = (id) => this.#channels.find((c) => c.id === id);
    for (const sw of body.querySelectorAll('[data-ch-toggle]')) {
      sw.addEventListener('change', () => this.#toggleChannel(sw));
    }
    for (const btn of body.querySelectorAll('[data-ch-test]')) {
      btn.addEventListener('click', () => this.#testChannel(find(btn.dataset.chTest)));
    }
    for (const btn of body.querySelectorAll('[data-ch-routes]')) {
      btn.addEventListener('click', () => this.#openRoutesDialog(find(btn.dataset.chRoutes)));
    }
    for (const btn of body.querySelectorAll('[data-ch-edit]')) {
      btn.addEventListener('click', () => this.#openChannelDialog(find(btn.dataset.chEdit) ?? null));
    }
    for (const btn of body.querySelectorAll('[data-ch-delete]')) {
      btn.addEventListener('click', () => this.#removeChannel(find(btn.dataset.chDelete)));
    }
  }

  #renderDeliveries() {
    const body = this.querySelector('#deliveries-body');
    if (!this.#deliveries.length) {
      body.innerHTML = `<app-empty-state inline heading="No deliveries yet"
        description="Notifications sent to your channels appear here."></app-empty-state>`;
      return;
    }
    const names = new Map(this.#channels.map((c) => [c.id, c.name]));
    const rows = this.#deliveries.map((d) => `<tr>
        <td class="nowrap">${escHtml(fmtDate(d.created_at))}</td>
        <td>${escHtml(names.get(d.channel_id) ?? d.channel_id)}</td>
        <td>${escHtml(d.event)}</td>
        <td>${badge(deliveryStatusTone(d.status), d.status)}</td>
        <td class="num">${escHtml(String(d.attempts))}</td>
        <td>${d.last_error ? `<span class="sub">${escHtml(d.last_error)}</span>` : ''}</td>
      </tr>`).join('');
    body.innerHTML = `<div class="table-wrap"><table class="alerts-table">
      <thead><tr><th>When</th><th>Channel</th><th>Event</th><th>Status</th>
        <th class="num">Attempts</th><th>Last error</th></tr></thead>
      <tbody>${rows}</tbody></table></div>`;
  }

  async #toggleChannel(sw) {
    const enabled = sw.checked;
    try {
      await call('updateChannel', { id: sw.dataset.chToggle, enabled });
      toast.success(enabled ? 'Channel enabled' : 'Channel disabled');
      await this.#loadChannels();
    } catch (err) {
      sw.checked = !enabled;
      toast.error(err?.message || 'Could not update channel');
    }
  }

  async #removeChannel(row) {
    if (!row) return;
    const ok = await confirmDialog({
      title: 'Delete channel',
      message: `Delete "${row.name}"? Its routes are removed and pending notifications are dropped.`,
      confirmLabel: 'Delete',
      danger: true,
    });
    if (!ok) return;
    try {
      await call('deleteChannel', { id: row.id });
      this.#testResults.delete(row.id);
      toast.success('Channel deleted');
      await this.#loadChannels();
    } catch (err) {
      toast.error(err?.message || 'Could not delete channel');
    }
  }

  async #testChannel(row) {
    if (!row) return;
    let result;
    try {
      const resp = await call('testChannel', { id: row.id });
      const r = resp?.data ?? resp ?? {};
      result = r.delivered
        ? { tone: 'success', text: `Delivered${r.status_code ? ` (${r.status_code})` : ''}` }
        : { tone: 'error', text: `Failed${r.error ? `: ${r.error}` : ''}` };
    } catch (err) {
      result = err?.status === HTTP_TOO_MANY_REQUESTS
        ? { tone: 'warning', text: 'Too many test sends, try again in a minute' }
        : { tone: 'error', text: `Failed${err?.code ? `: ${err.code}` : ''}` };
    }
    this.#testResults.set(row.id, result);
    this.#renderChannels();
    this.#loadDeliveries();
  }

  #openChannelDialog(row) {
    this.querySelector('#channel-modal')?.remove();
    const kind = row?.kind ?? 'webhook';
    const wrap = document.createElement('div');
    // The URL and secret fields are never pre-filled: they are write-only.
    wrap.innerHTML = `
      <app-modal heading="${row ? 'Edit channel' : 'New channel'}" id="channel-modal">
        <div class="modal-form">
          <app-alert id="f-alert" variant="destructive" hidden></app-alert>
          <app-input id="f-name" label="Name" required autocomplete="off"
            value="${escAttr(row?.name ?? '')}"></app-input>
          <app-select id="f-kind" label="Kind" ${row ? 'disabled' : ''} ${optionsAttr(CHANNEL_KINDS)}
            value="${escAttr(kind)}"></app-select>
          <app-input id="f-url" label="${row ? 'New URL' : 'URL'}" autocomplete="off" ${row ? '' : 'required'}
            hint="${escAttr(row ? `Current: ${row.url_hint}. Leave empty to keep it.` : 'Slack incoming webhook or webhook endpoint.')}"
            value=""></app-input>
          <div id="hmac-group" class="modal-form">
            ${row ? `<app-select id="f-hmac-mode" label="HMAC secret" ${optionsAttr(HMAC_MODES)}
              value="keep"></app-select>` : ''}
            <app-input id="f-secret" label="${row ? 'New HMAC secret' : 'HMAC secret (optional)'}" type="password"
              autocomplete="new-password" value=""
              hint="Used to sign webhook bodies (X-Nasiko-Signature)."></app-input>
          </div>
        </div>
        <div data-slot="footer">
          <app-button variant="tertiary" size="md" id="f-cancel">Cancel</app-button>
          <app-button variant="primary" size="md" id="f-save">${row ? 'Save' : 'Create'}</app-button>
        </div>
      </app-modal>`;
    const modal = wrap.firstElementChild;
    this.appendChild(modal);
    const q = (sel) => modal.querySelector(sel);

    const syncVisibility = () => {
      const webhook = q('#f-kind').value === 'webhook';
      q('#hmac-group').hidden = !webhook;
      const mode = q('#f-hmac-mode')?.value;
      q('#f-secret').hidden = Boolean(mode) && mode !== 'set';
    };
    q('#f-kind').addEventListener('change', syncVisibility);
    q('#f-hmac-mode')?.addEventListener('change', syncVisibility);
    syncVisibility();

    q('#f-cancel').addEventListener('click', () => modal.close());
    q('#f-save').addEventListener('click', () => this.#saveChannel(modal, row));
    modal.addEventListener('modal-close', () => modal.remove());
    modal.show();
  }

  async #saveChannel(modal, row) {
    const q = (sel) => modal.querySelector(sel);
    clearFieldErrors(...Object.values(CHANNEL_FIELDS).map((sel) => q(sel)));
    const alert = q('#f-alert');
    alert.hidden = true;

    const isEdit = Boolean(row);
    const form = {
      name: q('#f-name').value,
      kind: q('#f-kind').value,
      url: q('#f-url').value,
      hmac_secret: q('#f-kind').value === 'webhook' ? q('#f-secret').value : '',
      hmac_mode: q('#f-hmac-mode')?.value ?? 'set',
    };
    // The server owns the https/SSRF policy and may allow plain http for
    // private receivers in dev; the client only rejects non-URLs and non-http(s).
    const errors = validateChannelForm(form, { isEdit, allowInsecure: true });
    const failed = Object.keys(errors);
    if (failed.length) {
      for (const key of failed) setFieldError(q(CHANNEL_FIELDS[key]), errors[key]);
      return;
    }

    const save = q('#f-save');
    save.setAttribute('disabled', '');
    try {
      const payload = channelPayload(form, { isEdit });
      if (isEdit) await call('updateChannel', { id: row.id, ...payload });
      else await call('createChannel', payload);
      toast.success(isEdit ? 'Channel updated' : 'Channel created');
      modal.close();
      await this.#loadChannels();
    } catch (err) {
      const key = CHANNEL_CODE_TO_FIELD[err?.code];
      if (key) {
        setFieldError(q(CHANNEL_FIELDS[key]), err.message || 'Invalid value');
      } else {
        alert.setAttribute('description', err?.message || 'Could not save channel');
        alert.hidden = false;
      }
    } finally {
      save.removeAttribute('disabled');
    }
  }

  // ── routes dialog ────────────────────────────────────────────────────────

  async #openRoutesDialog(channel) {
    if (!channel) return;
    let rows;
    try {
      rows = asRows(await call('fetchChannelRoutes', { id: channel.id })).map((r) => ({
        alert_kind: r.alert_kind ?? 'any',
        min_severity: r.min_severity,
      }));
    } catch (err) {
      toast.error(err?.message || 'Could not load routes');
      return;
    }
    this.querySelector('#routes-modal')?.remove();
    const wrap = document.createElement('div');
    wrap.innerHTML = `
      <app-modal heading="${escAttr(`Routes: ${channel.name}`)}" id="routes-modal">
        <div class="modal-form">
          <app-alert id="f-alert" variant="destructive" hidden></app-alert>
          <p class="help">An alert is sent to this channel when it matches any route. With no routes, nothing is sent.</p>
          <div id="route-rows" class="route-rows"></div>
          <div><app-button variant="tertiary" size="sm" id="add-route">Add route</app-button></div>
        </div>
        <div data-slot="footer">
          <app-button variant="tertiary" size="md" id="f-cancel">Cancel</app-button>
          <app-button variant="primary" size="md" id="f-save">Save</app-button>
        </div>
      </app-modal>`;
    const modal = wrap.firstElementChild;
    this.appendChild(modal);
    const q = (sel) => modal.querySelector(sel);
    const container = q('#route-rows');

    const draw = () => {
      container.innerHTML = rows.map((r, i) => `<div class="route-row">
          <app-select size="sm" aria-label="Alert type" data-route-kind="${i}" ${optionsAttr(ROUTE_KINDS)}
            value="${escAttr(r.alert_kind)}"></app-select>
          <app-select size="sm" aria-label="Minimum severity" data-route-sev="${i}" ${optionsAttr(SEVERITIES)}
            value="${escAttr(r.min_severity)}"></app-select>
          <app-button variant="tertiary" size="sm" data-route-remove="${i}">Remove</app-button>
        </div>`).join('');
    };
    draw();
    container.addEventListener('change', (e) => {
      const kindIdx = e.target.dataset?.routeKind;
      const sevIdx = e.target.dataset?.routeSev;
      if (kindIdx !== undefined) rows[Number(kindIdx)].alert_kind = e.target.value;
      if (sevIdx !== undefined) rows[Number(sevIdx)].min_severity = e.target.value;
    });
    container.addEventListener('click', (e) => {
      const btn = e.target.closest('[data-route-remove]');
      if (!btn) return;
      rows.splice(Number(btn.dataset.routeRemove), 1);
      draw();
    });
    q('#add-route').addEventListener('click', () => {
      rows.push({ alert_kind: 'any', min_severity: 'warning' });
      draw();
    });
    q('#f-cancel').addEventListener('click', () => modal.close());
    q('#f-save').addEventListener('click', async () => {
      const alert = q('#f-alert');
      alert.hidden = true;
      const save = q('#f-save');
      save.setAttribute('disabled', '');
      try {
        await call('saveChannelRoutes', { id: channel.id, ...routesPayload(rows) });
        toast.success('Routes saved');
        modal.close();
      } catch (err) {
        alert.setAttribute('description', err?.message || 'Could not save routes');
        alert.hidden = false;
      } finally {
        save.removeAttribute('disabled');
      }
    });
    modal.addEventListener('modal-close', () => modal.remove());
    modal.show();
  }
}

if (!customElements.get('alerts-page')) customElements.define('alerts-page', AlertsPage);
