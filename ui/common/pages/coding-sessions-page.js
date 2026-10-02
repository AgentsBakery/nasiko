/**
 * Coding sessions — Claude Code, Codex and other coding-agent sessions traced
 * through Nasiko (GET /api/observability/session/list?coding_only=true). The
 * same rows and columns `nasiko observe sessions` prints, in the portal.
 *
 * The "Coding agents only" switch is a server-side filter (`coding_only`), not
 * a client-side one: filtering a fetched page would break offset paging, since
 * a page of 25 could hold zero coding rows. Turning it off lists every agent's
 * sessions from the same endpoint.
 *
 * Non-superusers see only their own sessions; the server scopes the list.
 * Session ids and agent names are untrusted, so every interpolated string goes
 * through escHtml/escAttr.
 *
 * @element coding-sessions-page
 */
import { loadCss } from '/common/utils/css.js';
const styles = await loadCss(new URL('./coding-sessions-page.css', import.meta.url));
import { escAttr, escHtml } from '/common/utils/escape.js';
import { showToast } from '../utils/toast.js';
import '../design-system/app-badge/app-badge.js';
import '../design-system/app-button/app-button.js';
import '../design-system/app-empty-state/app-empty-state.js';
import '../design-system/app-switch/app-switch.js';
import '../design-system/app-table/app-table.js';
import '../features/app-module-nav.js';
import { errorStateHtml, bindRetry } from '/common/design-system/app-empty-state/error-state.js';
import { call } from '../core/data-sources.js';
import { navigate as routerNavigate } from '../core/router.js';
import {
  isUnavailable, nextOffset, sessionsFromResponse, toSessionRow,
} from '/common/utils/coding-sessions.js';
import { CORRECTED_HINT } from '/common/utils/coding-agent-breakdown.js';

document.adoptedStyleSheets = [...document.adoptedStyleSheets, styles];

/** Rows per request. Each row costs the server one trace-store lookup. */
const PAGE_SIZE = 25;
const EM_DASH = '—';
const LOAD_ERROR = 'Could not load coding sessions.';
const UNAVAILABLE_ERROR = 'Session observability is not available for your account.';

/** Localized start time, or a dash when the trace store had none. */
const fmtStarted = (iso) => {
  if (!iso) return EM_DASH;
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? EM_DASH : d.toLocaleString();
};

const textCell = (v) => escHtml(String(v ?? EM_DASH));

class CodingSessionsPage extends HTMLElement {
  #initialized = false;
  /** @type {import('/common/utils/coding-sessions.js').SessionRow[]} */
  #rows = [];
  /** Offset of the next page; null once the list is exhausted. */
  #nextOffset = null;
  #codingOnly = true;
  /** Bumped on every fresh load so a stale in-flight response is dropped. */
  #loadId = 0;
  #loadingMore = false;

  connectedCallback() {
    if (this.#initialized) return;
    this.#initialized = true;
    this.#render();
    bindRetry(this, 'coding-sessions-retry', () => this.#load());
    this.#load();
  }

  disconnectedCallback() {
    // Listeners live on this element's own subtree and go with it; bumping the
    // id drops any in-flight load so it cannot write into a detached tree.
    this.#loadId += 1;
  }

  #render() {
    this.innerHTML = `
      <app-module-nav module="observability"></app-module-nav>
      <div class="coding-sessions-header">
        <h1 class="title-page">Coding sessions</h1>
        <p class="coding-sessions-subtitle">Claude Code, Codex and other coding-agent sessions traced through Nasiko</p>
      </div>
      <div class="coding-sessions-toolbar">
        <app-switch id="coding-only" size="sm" label="Coding agents only" checked></app-switch>
      </div>
      <div class="coding-sessions-list" id="coding-sessions-list"></div>
      <div class="coding-sessions-more" id="coding-sessions-more" hidden>
        <app-button variant="secondary" size="sm" id="btn-more">Load more</app-button>
        <span class="coding-sessions-count" id="coding-sessions-count"></span>
      </div>
    `;

    this.querySelector('#coding-only')?.addEventListener('change', (e) => {
      const sw = /** @type {any} */ (e.currentTarget);
      this.#codingOnly = Boolean(sw?.checked);
      this.#load();
    });

    this.querySelector('#btn-more')?.addEventListener('click', () => this.#load({ more: true }));

    // Delegated on the container: app-table rebuilds its tbody on every
    // refresh and sort, so per-row listeners would be dropped. The link keeps
    // its href so it stays a real, middle-clickable anchor.
    this.querySelector('#coding-sessions-list')?.addEventListener('click', (e) => {
      const target = /** @type {HTMLElement} */ (e.target);
      if (/** @type {MouseEvent} */ (e).metaKey || /** @type {MouseEvent} */ (e).ctrlKey) return;
      const link = target.closest('tr')?.querySelector('.coding-session-link');
      if (link) {
        e.preventDefault();
        routerNavigate(link.getAttribute('href'));
      }
    });
  }

  #mountTable() {
    const list = this.querySelector('#coding-sessions-list');
    list.innerHTML = '<app-table id="coding-sessions-table" pagination="none"></app-table>';
    const table = /** @type {any} */ (list.querySelector('#coding-sessions-table'));
    table.columns = [
      {
        key: 'sessionId',
        label: 'Session',
        render: (_v, r) => `<a class="coding-session-link" href="${escAttr(r.href)}"
          title="${escAttr(r.sessionId)}"><span class="coding-session-id">${escHtml(r.sessionId)}</span></a>`,
      },
      { key: 'agent', label: 'Agent', render: textCell },
      { key: 'started', label: 'Started', render: (v) => escHtml(fmtStarted(v)) },
      { key: 'duration', label: 'Duration', render: textCell },
      { key: 'traces', label: 'Traces', render: textCell },
      { key: 'tokens', label: 'Tokens', render: textCell },
      {
        key: 'cost',
        label: 'Cost',
        // Legacy Codex figures were adjusted on read; say so next to the number.
        render: (v, r) => `${textCell(v)}${r.usageCorrected
          ? ` <app-badge variant="info" title="${escAttr(CORRECTED_HINT)}">Corrected</app-badge>`
          : ''}`,
      },
    ];
    table.dataFn = async () => ({ data: this.#rows });
  }

  /** Loads one page. `more: true` appends the next page instead of replacing. */
  async #load({ more = false } = {}) {
    if (more && (this.#loadingMore || this.#nextOffset === null)) return;
    const id = more ? this.#loadId : ++this.#loadId;
    const offset = more ? this.#nextOffset : 0;
    const moreBtn = this.querySelector('#btn-more');
    if (more) {
      this.#loadingMore = true;
      moreBtn?.setAttribute('loading', '');
    } else {
      this.#rows = [];
      this.#nextOffset = null;
      this.#renderState('<app-table pagination="none" loading></app-table>');
      this.#renderPager();
    }

    try {
      const resp = await call('fetchObservabilitySessions', PAGE_SIZE, offset, {
        codingOnly: this.#codingOnly,
      });
      if (id !== this.#loadId) return;
      if (isUnavailable(resp)) {
        this.#renderState(errorStateHtml(UNAVAILABLE_ERROR));
        return;
      }
      const page = sessionsFromResponse(resp).map(toSessionRow);
      this.#rows = more ? [...this.#rows, ...page] : page;
      this.#nextOffset = nextOffset(resp?.data);
      this.#syncList();
    } catch {
      if (id !== this.#loadId) return;
      // A failed "load more" must not discard the pages already on screen.
      if (more) {
        showToast('Could not load more sessions.');
        return;
      }
      this.#renderState(errorStateHtml(LOAD_ERROR));
    } finally {
      if (more) {
        this.#loadingMore = false;
        moreBtn?.removeAttribute('loading');
      }
      if (id === this.#loadId) this.#renderPager();
    }
  }

  #syncList() {
    if (!this.#rows.length) {
      const heading = this.#codingOnly ? 'No coding sessions yet' : 'No sessions yet';
      this.#renderState(`<app-empty-state heading="${escAttr(heading)}"
        description="Connect a coding agent with &quot;nasiko agents install claude&quot;. Traces need TEMPO_URL, LOKI_URL and CODING_AGENT_OTLP_ENDPOINT configured on the server."></app-empty-state>`);
      return;
    }
    const table = /** @type {any} */ (this.querySelector('#coding-sessions-table'));
    if (table) table.refresh();
    else this.#mountTable();
  }

  /** Replaces the table slot with a standalone block (loading, empty or failed). */
  #renderState(html) {
    const list = this.querySelector('#coding-sessions-list');
    if (list) list.innerHTML = html;
  }

  #renderPager() {
    const wrap = /** @type {HTMLElement|null} */ (this.querySelector('#coding-sessions-more'));
    const count = this.querySelector('#coding-sessions-count');
    const moreBtn = /** @type {HTMLElement|null} */ (this.querySelector('#btn-more'));
    if (!wrap || !count || !moreBtn) return;
    const shown = this.#rows.length;
    wrap.hidden = shown === 0;
    const exhausted = this.#nextOffset === null;
    moreBtn.hidden = exhausted;
    count.textContent = exhausted
      ? `Showing all ${shown} session${shown === 1 ? '' : 's'}`
      : `Showing ${shown} sessions`;
  }
}

if (!customElements.get('coding-sessions-page')) {
  customElements.define('coding-sessions-page', CodingSessionsPage);
}
