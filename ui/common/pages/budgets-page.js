/**
 * Budgets page.
 *
 * Admins (`authService.isAdmin()`) get the management table: every budget with
 * live spend, % bar, projection and state, plus a create/edit dialog, an
 * enabled toggle and a delete confirmation. Everyone else gets a read-only
 * "My budgets" list from `/api/budgets/me`; platform rows there arrive
 * redacted (no dollar fields) and render as % used, state and reset time only.
 *
 * The role split here is cosmetic. The server answers 403 `admin_required` to
 * every budget mutation from a non-admin and redacts platform rows itself.
 * Budget, user and agent names are user-supplied, so every interpolated string
 * goes through escHtml/escAttr.
 */
import { loadCss } from '/common/utils/css.js';
const styles = await loadCss(new URL('./budgets-page.css', import.meta.url));
import { escAttr, escHtml } from '/common/utils/escape.js';
import { toast } from '/common/utils/toast.js';
import { setFieldError, clearFieldErrors } from '/common/utils/field-error.js';
import '/common/design-system/app-badge/app-badge.js';
import '/common/design-system/app-button/app-button.js';
import '/common/design-system/app-empty-state/app-empty-state.js';
import '/common/design-system/app-alert/app-alert.js';
import '/common/design-system/app-input/app-input.js';
import '/common/design-system/app-progress/app-progress.js';
import '/common/design-system/app-select/app-select.js';
import '/common/design-system/app-switch/app-switch.js';
import { confirmDialog } from '/common/design-system/app-modal/app-modal.js';
import { errorStateHtml, bindRetry } from '/common/design-system/app-empty-state/error-state.js';
import { call } from '../core/data-sources.js';
import { authService } from '/common/services/auth-service.js';
import {
  ACTIONS, PERIODS, SCOPES,
  budgetFormToPayload, fmtPct, fmtUsd, isRedacted, progressValue,
  stateBadgeVariant, stateLabel, validateBudgetForm,
} from '/common/utils/budgets.js';

document.adoptedStyleSheets = [...document.adoptedStyleSheets, styles];

const DEFAULT_SOFT_PCT = 80;
const DEFAULT_CEILING_PCT = 125;

/** Server 400 `code` -> the form field that caused it. */
const CODE_TO_FIELD = {
  invalid_name: 'name',
  invalid_limit: 'limit_usd',
  invalid_threshold: 'soft_threshold_pct',
  invalid_ceiling: 'downgrade_ceiling_pct',
  target_required: 'target_id',
  target_not_allowed: 'target_id',
  target_not_found: 'target_id',
};

const FIELD_SELECTORS = {
  name: '#f-name',
  limit_usd: '#f-limit',
  soft_threshold_pct: '#f-soft',
  downgrade_ceiling_pct: '#f-ceiling',
  target_id: '#f-target',
};

const asRows = (resp) => {
  if (Array.isArray(resp)) return resp;
  return Array.isArray(resp?.data) ? resp.data : [];
};

const fmtDate = (iso) => {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? '—' : d.toLocaleString();
};

class BudgetsPage extends HTMLElement {
  #initialized = false;
  #admin = false;
  #rows = [];
  /** @type {Map<string, string>} target id -> display name */
  #targetNames = new Map();
  #users = [];
  #agents = [];
  #editing = null;
  #loadId = 0;

  connectedCallback() {
    if (this.#initialized) return;
    this.#initialized = true;
    this.innerHTML = `<div class="page-head"><h1 class="title-page">Budgets</h1></div>
      <div id="body"></div>`;
    bindRetry(this, 'budgets-retry', () => this.#load());
    authService.fetchCurrentUser().catch(() => null).then(() => {
      this.#admin = authService.isAdmin();
      this.#renderHead();
      this.#load();
      if (this.#admin) this.#loadTargets();
    });
  }

  disconnectedCallback() {
    // All listeners are attached to this element's own subtree, which is torn
    // down with it; bumping the id drops any in-flight load.
    this.#loadId += 1;
  }

  #renderHead() {
    const head = this.querySelector('.page-head');
    head.innerHTML = `<h1 class="title-page">${this.#admin ? 'Budgets' : 'My budgets'}</h1>
      ${this.#admin ? '<app-button variant="primary" size="md" id="new-btn">New budget</app-button>' : ''}`;
    head.querySelector('#new-btn')?.addEventListener('click', () => this.#openDialog(null));
  }

  async #load() {
    const id = ++this.#loadId;
    const body = this.querySelector('#body');
    try {
      const resp = await call(this.#admin ? 'fetchBudgets' : 'fetchMyBudgets');
      if (id !== this.#loadId) return;
      this.#rows = asRows(resp);
      this.#renderBody();
    } catch (err) {
      if (id !== this.#loadId) return;
      body.innerHTML = errorStateHtml(err?.message || 'Could not load budgets');
    }
  }

  async #loadTargets() {
    const [users, agents] = await Promise.all([
      call('fetchBudgetUsers').catch(() => null),
      call('fetchAgentList', { limit: 200 }).catch(() => null),
    ]);
    this.#users = asRows(users).map((u) => ({
      id: u.id, name: u.display_name || u.username || u.email || u.id,
    }));
    this.#agents = asRows(agents).map((a) => ({ id: a.id, name: a.name || a.id }));
    this.#targetNames = new Map([...this.#users, ...this.#agents].map((t) => [String(t.id), t.name]));
    if (this.#rows.length) this.#renderBody();
  }

  #targetLabel(row) {
    if (row.scope === 'platform') return 'Platform';
    const name = this.#targetNames.get(String(row.target_id)) ?? row.target_id ?? '';
    return `${row.scope === 'user' ? 'User' : 'Agent'}: ${name}`;
  }

  #stateChip(row) {
    return `<app-badge variant="${escAttr(stateBadgeVariant(row.state))}">${escHtml(stateLabel(row.state))}</app-badge>`;
  }

  #usedCell(row) {
    return `<div class="used">
        <app-progress size="sm" value="${progressValue(row.pct_used)}"
          label="${escAttr(`${row.name} used`)}"></app-progress>
        <span class="used-label">${escHtml(fmtPct(row.pct_used))}</span>
      </div>`;
  }

  #renderBody() {
    const body = this.querySelector('#body');
    if (!this.#rows.length) {
      body.innerHTML = `<app-empty-state heading="${this.#admin ? 'No budgets yet' : 'No budgets apply to you'}"
        description="${this.#admin ? 'Create a budget to cap LLM spend per user, agent or platform-wide.' : 'Budgets that cover your usage will appear here.'}"></app-empty-state>`;
      return;
    }
    body.innerHTML = this.#admin ? this.#adminTable() : this.#memberTable();
    if (!this.#admin) return;
    for (const sw of body.querySelectorAll('app-switch[data-id]')) {
      sw.addEventListener('change', () => this.#toggle(sw));
    }
    for (const btn of body.querySelectorAll('[data-edit]')) {
      btn.addEventListener('click', () => {
        this.#openDialog(this.#rows.find((r) => r.id === btn.dataset.edit) ?? null);
      });
    }
    for (const btn of body.querySelectorAll('[data-delete]')) {
      btn.addEventListener('click', () => {
        this.#remove(this.#rows.find((r) => r.id === btn.dataset.delete));
      });
    }
  }

  #adminTable() {
    const rows = this.#rows.map((r) => `<tr>
        <td class="name">${escHtml(r.name)}</td>
        <td>${escHtml(this.#targetLabel(r))}</td>
        <td>${escHtml(r.period)}</td>
        <td class="num">${escHtml(fmtUsd(r.limit_usd))}</td>
        <td class="num">${escHtml(fmtUsd(r.spend_usd))}</td>
        <td>${this.#usedCell(r)}</td>
        <td class="num">${escHtml(fmtUsd(r.projected_usd))}</td>
        <td>${this.#stateChip(r)}</td>
        <td>${r.action === 'downgrade'
    ? `Downgrade <span class="sub">(${escHtml(String(r.downgrade_ceiling_pct))}% ceiling)</span>`
    : 'Block'}</td>
        <td><app-switch size="sm" data-id="${escAttr(r.id)}" aria-label="${escAttr(`Enabled: ${r.name}`)}"
          ${r.enabled ? 'checked' : ''}></app-switch></td>
        <td><div class="row-actions">
          <app-button variant="tertiary" size="sm" data-edit="${escAttr(r.id)}">Edit</app-button>
          <app-button variant="tertiary" size="sm" data-delete="${escAttr(r.id)}">Delete</app-button>
        </div></td>
      </tr>`).join('');
    return `<div class="table-wrap"><table class="budgets-table">
      <thead><tr><th>Name</th><th>Scope / target</th><th>Period</th><th class="num">Limit</th>
        <th class="num">Spend</th><th>Used</th><th class="num">Projected</th><th>State</th>
        <th>Action</th><th>Enabled</th><th></th></tr></thead>
      <tbody>${rows}</tbody></table></div>`;
  }

  #memberTable() {
    const rows = this.#rows.map((r) => {
      const redacted = isRedacted(r);
      return `<tr>
        <td class="name">${escHtml(r.name)}</td>
        <td>${escHtml(r.scope === 'platform' ? 'Platform' : r.scope)}</td>
        <td>${escHtml(r.period)}</td>
        ${redacted
    ? '<td class="num">—</td><td class="num">—</td>'
    : `<td class="num">${escHtml(fmtUsd(r.limit_usd))}</td><td class="num">${escHtml(fmtUsd(r.spend_usd))}</td>`}
        <td>${this.#usedCell(r)}</td>
        ${redacted ? '<td class="num">—</td>' : `<td class="num">${escHtml(fmtUsd(r.projected_usd))}</td>`}
        <td>${this.#stateChip(r)}</td>
        <td>Resets ${escHtml(fmtDate(r.resets_at))}</td>
      </tr>`;
    }).join('');
    return `<div class="table-wrap"><table class="budgets-table">
      <thead><tr><th>Name</th><th>Scope</th><th>Period</th><th class="num">Limit</th>
        <th class="num">Spend</th><th>Used</th><th class="num">Projected</th><th>State</th>
        <th>Resets</th></tr></thead>
      <tbody>${rows}</tbody></table></div>`;
  }

  async #toggle(sw) {
    const enabled = sw.checked;
    try {
      await call('updateBudget', { id: sw.dataset.id, enabled });
      toast.success(enabled ? 'Budget enabled' : 'Budget disabled');
      await this.#load();
    } catch (err) {
      sw.checked = !enabled;
      toast.error(err?.message || 'Could not update budget');
    }
  }

  async #remove(row) {
    if (!row) return;
    const ok = await confirmDialog({
      title: 'Delete budget',
      message: `Delete "${row.name}"? Its spend history is kept but it will stop being enforced.`,
      confirmLabel: 'Delete',
      danger: true,
    });
    if (!ok) return;
    try {
      await call('deleteBudget', { id: row.id });
      toast.success('Budget deleted');
      await this.#load();
    } catch (err) {
      toast.error(err?.message || 'Could not delete budget');
    }
  }

  // ── dialog ───────────────────────────────────────────────────────────────

  #targetOptions(scope) {
    const list = scope === 'agent' ? this.#agents : this.#users;
    return JSON.stringify(list.map((t) => ({ value: String(t.id), label: t.name })));
  }

  #openDialog(row) {
    this.#editing = row;
    this.querySelector('#budget-modal')?.remove();
    const scope = row?.scope ?? 'user';
    const wrap = document.createElement('div');
    wrap.innerHTML = `
      <app-modal heading="${row ? 'Edit budget' : 'New budget'}" id="budget-modal">
        <div class="modal-form">
          <app-alert id="f-alert" variant="error" hidden></app-alert>
          <app-input id="f-name" label="Name" required autocomplete="off"
            value="${escAttr(row?.name ?? '')}"></app-input>
          <div class="pair">
            <app-select id="f-scope" label="Scope" ${row ? 'disabled' : ''}
              options='${escAttr(JSON.stringify(SCOPES))}' value="${escAttr(scope)}"></app-select>
            <app-select id="f-target" label="Target" placeholder="Choose…" ${row ? 'disabled' : ''}
              options='${escAttr(this.#targetOptions(scope))}'
              value="${escAttr(row?.target_id ?? '')}" ${scope === 'platform' ? 'hidden' : ''}></app-select>
          </div>
          <div class="pair">
            <app-select id="f-period" label="Period"
              options='${escAttr(JSON.stringify(PERIODS))}' value="${escAttr(row?.period ?? 'monthly')}"></app-select>
            <app-input id="f-limit" label="Limit (USD)" type="number" min="0" step="any" required
              value="${escAttr(row?.limit_usd ?? '')}"></app-input>
          </div>
          <div class="pair">
            <app-input id="f-soft" label="Soft threshold (%)" type="number" min="1" max="100"
              value="${escAttr(row?.soft_threshold_pct ?? DEFAULT_SOFT_PCT)}"></app-input>
            <app-select id="f-action" label="Action"
              options='${escAttr(JSON.stringify(ACTIONS))}' value="${escAttr(row?.action ?? 'block')}"></app-select>
          </div>
          <app-input id="f-ceiling" label="Downgrade ceiling (%)" type="number" min="100"
            hint="Above this share of the limit, calls are blocked even when downgrading."
            value="${escAttr(row?.downgrade_ceiling_pct ?? DEFAULT_CEILING_PCT)}"></app-input>
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
      const platform = q('#f-scope').value === 'platform';
      q('#f-target').hidden = platform;
      q('#f-ceiling').hidden = q('#f-action').value !== 'downgrade';
    };
    q('#f-scope').addEventListener('change', () => {
      q('#f-target').setAttribute('options', this.#targetOptions(q('#f-scope').value));
      q('#f-target').value = '';
      syncVisibility();
    });
    q('#f-action').addEventListener('change', syncVisibility);
    syncVisibility();

    q('#f-cancel').addEventListener('click', () => modal.close());
    q('#f-save').addEventListener('click', () => this.#save(modal));
    modal.addEventListener('modal-close', () => modal.remove());
    modal.show();
  }

  #readForm(modal) {
    const q = (sel) => modal.querySelector(sel);
    return {
      name: q('#f-name').value,
      scope: q('#f-scope').value,
      target_id: q('#f-target').value,
      period: q('#f-period').value,
      limit_usd: q('#f-limit').value,
      soft_threshold_pct: q('#f-soft').value,
      action: q('#f-action').value,
      downgrade_ceiling_pct: q('#f-ceiling').value || String(DEFAULT_CEILING_PCT),
    };
  }

  async #save(modal) {
    const fields = Object.values(FIELD_SELECTORS).map((sel) => modal.querySelector(sel));
    clearFieldErrors(...fields);
    const alert = modal.querySelector('#f-alert');
    alert.hidden = true;

    const form = this.#readForm(modal);
    const errors = validateBudgetForm(form);
    // An update never sends scope or target, so a target error cannot apply.
    if (this.#editing) delete errors.target_id;
    const failed = Object.keys(errors);
    if (failed.length) {
      for (const key of failed) setFieldError(modal.querySelector(FIELD_SELECTORS[key]), errors[key]);
      return;
    }

    const save = modal.querySelector('#f-save');
    save.setAttribute('disabled', '');
    try {
      if (this.#editing) {
        await call('updateBudget', { id: this.#editing.id, ...budgetFormToPayload(form, { update: true }) });
      } else {
        await call('createBudget', budgetFormToPayload(form));
      }
      toast.success(this.#editing ? 'Budget updated' : 'Budget created');
      modal.close();
      await this.#load();
    } catch (err) {
      const field = CODE_TO_FIELD[err?.code];
      if (field) {
        setFieldError(modal.querySelector(FIELD_SELECTORS[field]), err.message || 'Invalid value');
      } else {
        alert.textContent = err?.message || 'Could not save budget';
        alert.hidden = false;
      }
    } finally {
      save.removeAttribute('disabled');
    }
  }
}

if (!customElements.get('budgets-page')) customElements.define('budgets-page', BudgetsPage);
