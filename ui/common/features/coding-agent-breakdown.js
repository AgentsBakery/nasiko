/**
 * `<coding-agent-breakdown session-id="…">` — who did the work in a coding
 * session: one row for the main agent and one per subagent or teammate, with
 * calls, tokens by class, cost and share, plus a line saying in words whether
 * subagents and agent teams could be captured at all.
 *
 * Data source: `call('fetchCodingSessionAgents', sessionId)` →
 * GET /api/coding-sessions/{id}/agents. It reads the Postgres rollup only, so
 * the element renders on its own whether or not Tempo is configured or the
 * session's trace fetch failed.
 *
 * A 404 (not found, or not this user's session: the server hides both the
 * same way) and a session with no agent rows (not a coding session) hide the
 * element entirely.
 *
 * Agent types, names and task descriptions are user/agent-supplied: every
 * interpolated string goes through escHtml/escAttr, and numbers are formatted
 * before they are interpolated.
 */
import { loadCss } from '/common/utils/css.js';
import { escAttr, escHtml } from '/common/utils/escape.js';
import { fmtDateTime, fmtNumber, fmtCurrency } from '/common/utils/units.js';
import { call } from '/common/core/data-sources.js';
import {
  CORRECTED_HINT, captureStatusText, outputFootnote, shareBasisNote, toBreakdownRows, lowerBound,
} from '/common/utils/coding-agent-breakdown.js';
import { errorStateHtml, bindRetry } from '/common/design-system/app-empty-state/error-state.js';
import '/common/design-system/app-badge/app-badge.js';
import '/common/design-system/app-skeleton/app-skeleton.js';

const styles = await loadCss(new URL('./coding-agent-breakdown.css', import.meta.url));
document.adoptedStyleSheets = [...document.adoptedStyleSheets, styles];

const EM_DASH = '—';
const NOT_FOUND = 404;
const KIND_BADGE_VARIANT = { main: 'info', subagent: 'neutral', teammate: 'neutral', unknown: 'warning' };
const KIND_BADGE_LABEL = { main: 'main', subagent: 'subagent', teammate: 'teammate', unknown: 'unknown' };

/** @param {string|null} iso */
const fmtWhen = (iso) => (iso ? fmtDateTime(iso) : EM_DASH);

const correctedBadge = () =>
  `<app-badge variant="info" class="cab-corrected" title="${escAttr(CORRECTED_HINT)}">Corrected</app-badge>`;

class CodingAgentBreakdown extends HTMLElement {
  static get observedAttributes() { return ['session-id']; }

  #bound = false;
  /** Bumped per request; a response for an older id is dropped. */
  #requestId = 0;

  connectedCallback() {
    if (!this.#bound) {
      this.#bound = true;
      bindRetry(this, 'coding-agent-breakdown-retry', () => this.#load());
    }
    this.#load();
  }

  disconnectedCallback() {
    // Drops any in-flight response so it cannot write into a detached element.
    this.#requestId += 1;
  }

  attributeChangedCallback(_name, oldValue, newValue) {
    if (oldValue === newValue || !this.isConnected) return;
    this.#load();
  }

  async #load() {
    const id = ++this.#requestId;
    const sessionId = this.getAttribute('session-id') || '';
    if (!sessionId) {
      this.hidden = true;
      this.innerHTML = '';
      return;
    }
    this.hidden = false;
    this.setAttribute('aria-busy', 'true');
    this.innerHTML = `${this.#headingHtml()}<app-skeleton lines="3"></app-skeleton>`;
    let resp;
    try {
      resp = await call('fetchCodingSessionAgents', sessionId);
    } catch (e) {
      if (id !== this.#requestId) return;
      this.removeAttribute('aria-busy');
      if (e?.status === NOT_FOUND) {
        this.hidden = true;
        this.innerHTML = '';
        return;
      }
      console.warn('coding-agent-breakdown: fetch failed', e);
      this.innerHTML = `${this.#headingHtml()}${errorStateHtml("Couldn't load the agent breakdown")}`;
      return;
    }
    if (id !== this.#requestId) return;
    this.removeAttribute('aria-busy');
    const data = resp?.data ?? null;
    const rows = toBreakdownRows(data);
    if (!rows.length) {
      this.hidden = true;
      this.innerHTML = '';
      return;
    }
    this.#render(data, rows);
  }

  #headingHtml() {
    return '<h2 class="cab-title">Agents in this session</h2>';
  }

  /**
   * @param {any} data
   * @param {import('/common/utils/coding-agent-breakdown.js').BreakdownRow[]} rows
   */
  #render(data, rows) {
    const status = captureStatusText(data?.capture);
    const footnotes = [shareBasisNote(data?.share_basis), outputFootnote(data)].filter(Boolean);
    const totals = data?.totals ?? {};
    const totalsIncomplete = totals.output_incomplete === true;
    const t = totals.tokens ?? {};

    this.innerHTML = `
      ${this.#headingHtml()}
      <p class="cab-status">
        <span>${escHtml(status.subagents)}</span>
        <span class="cab-sep" aria-hidden="true">·</span>
        <span>${escHtml(status.teams)}</span>
      </p>
      <div class="cab-scroll">
        <table class="cab-table">
          <thead>
            <tr>
              <th scope="col">Agent</th>
              <th scope="col">Intent</th>
              <th scope="col">Started</th>
              <th scope="col">Ended</th>
              <th scope="col" class="num">LLM calls</th>
              <th scope="col" class="num">Tool calls</th>
              <th scope="col" class="num">Input</th>
              <th scope="col" class="num">Cache read</th>
              <th scope="col" class="num">Cache write</th>
              <th scope="col" class="num">Output</th>
              <th scope="col" class="num">Cost</th>
              <th scope="col" class="num">Share</th>
            </tr>
          </thead>
          <tbody>
            ${rows.map((r) => this.#rowHtml(r)).join('')}
          </tbody>
          <tfoot>
            <tr>
              <th scope="row" colspan="4">Total${totals.usage_corrected === true ? ` ${correctedBadge()}` : ''}</th>
              <td class="num">${escHtml(fmtNumber(totals.llm_calls))}</td>
              <td class="num">${escHtml(fmtNumber(totals.tool_calls))}</td>
              <td class="num">${escHtml(fmtNumber(t.input))}</td>
              <td class="num">${escHtml(fmtNumber(t.cache_read))}</td>
              <td class="num">${escHtml(fmtNumber(t.cache_creation))}</td>
              <td class="num">${escHtml(lowerBound(fmtNumber(t.output), totalsIncomplete))}</td>
              <td class="num">${escHtml(lowerBound(
                totals.cost_usd == null ? EM_DASH : fmtCurrency(totals.cost_usd), totalsIncomplete))}</td>
              <td class="num">${escHtml(this.#totalShare(data))}</td>
            </tr>
          </tfoot>
        </table>
      </div>
      ${footnotes.map((note) => `<p class="cab-note">${escHtml(note)}</p>`).join('')}
    `;
  }

  /** "100%" when any row carries a share; a dash when nothing was spent. */
  #totalShare(data) {
    const agents = Array.isArray(data?.agents) ? data.agents : [];
    return agents.some((a) => Number(a?.share) > 0) ? '100%' : EM_DASH;
  }

  /** @param {import('/common/utils/coding-agent-breakdown.js').BreakdownRow} r */
  #rowHtml(r) {
    const intent = r.intent.hidden
      ? `<span class="cab-hidden" title="${escAttr(r.intent.hint)}">${escHtml(r.intent.text)}
          <span class="cab-hidden-tag">(hidden)</span></span>`
      : `<span class="cab-intent" title="${escAttr(r.intent.text)}">${escHtml(r.intent.text)}</span>`;
    return `
      <tr>
        <th scope="row" class="cab-agent">
          <span class="cab-label">${escHtml(r.label)}</span>
          <app-badge variant="${escAttr(KIND_BADGE_VARIANT[r.kindBadge])}">${escHtml(KIND_BADGE_LABEL[r.kindBadge])}</app-badge>
          ${r.runsSuffix ? `<span class="cab-runs">${escHtml(r.runsSuffix)}</span>` : ''}
          ${r.corrected ? correctedBadge() : ''}
        </th>
        <td>${intent}</td>
        <td class="cab-when">${escHtml(fmtWhen(r.startedAt))}</td>
        <td class="cab-when">${escHtml(fmtWhen(r.endedAt))}</td>
        <td class="num">${escHtml(fmtNumber(r.llmCalls))}</td>
        <td class="num">${escHtml(fmtNumber(r.toolCalls))}</td>
        <td class="num">${escHtml(fmtNumber(r.tokens.input))}</td>
        <td class="num">${escHtml(fmtNumber(r.tokens.cache_read))}</td>
        <td class="num">${escHtml(fmtNumber(r.tokens.cache_creation))}</td>
        <td class="num">${escHtml(r.output)}</td>
        <td class="num">${escHtml(r.cost)}</td>
        <td class="num">${escHtml(r.share)}</td>
      </tr>`;
  }
}

if (!customElements.get('coding-agent-breakdown')) {
  customElements.define('coding-agent-breakdown', CodingAgentBreakdown);
}
