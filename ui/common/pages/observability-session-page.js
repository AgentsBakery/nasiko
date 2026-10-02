/**
 * Observability session detail — a turn navigator over the session's traces,
 * with the selected turn's span tree and the selected span's detail below it.
 *
 * A "turn" is one trace: one user query and the agent's answer to it. The
 * question and answer text comes from the chat transcript, joined to the trace
 * on `chat_messages.trace_id`; where a message carries no trace id (BYO-key
 * agents), the trace's own root-span content stands in.
 *
 * @element observability-session-page
 * @note Data sources (see /api/docs):
 *       `call('fetchObservabilitySession', sessionId)` → GET /api/observability/session/{id}
 *       `call('fetchObservabilityTrace', traceId)`     → GET /api/observability/trace/{id}
 *       `call('fetchSpanDetail', traceId, spanId)`     → GET /api/observability/span/{trace_id}/{span_id}
 *       `call('fetchChatSession', sessionId)`          → GET /api/chat/sessions/{id} (chat transcript)
 *       `call('fetchCodingSessionAgents', sessionId)`  → GET /api/coding-sessions/{id}/agents
 *         (per-agent breakdown; fetched by <coding-agent-breakdown> itself, from
 *         Postgres, so it renders even when the trace backend is down)
 *
 * Subagent and teammate traces are labelled from their root-span attributes
 * (`scopedTurnLabel`) so they never read as the user's question, and corrected
 * legacy Codex figures carry a "Corrected" badge wherever their cost shows.
 */
import { loadCss } from '/common/utils/css.js';
const styles = await loadCss(new URL('./observability-session-page.css', import.meta.url));
import { icons } from '../utils/icons.js';
// Both were previously "imported" from inside the docblock above, i.e. never:
// <app-skeleton> and <app-empty-state> rendered as inert unknown elements.
import '/common/design-system/app-skeleton/app-skeleton.js';
import '/common/design-system/app-empty-state/app-empty-state.js';
import '/common/design-system/app-stat-row/app-stat-row.js';
import '/common/design-system/app-badge/app-badge.js';
import '/common/design-system/app-button/app-button.js';
import '/common/design-system/app-tabs/app-tabs.js';
import '/common/design-system/app-menu/app-menu.js';
import '/common/design-system/app-trace-tree/app-trace-tree.js';
import '/common/features/app-module-nav.js';
import { escAttr, escHtml } from '/common/utils/escape.js';
import { renderMarkdown } from '/common/utils/markdown.js';
import { call } from '../core/data-sources.js';
import '/common/features/agent-steps.js';
import { errorStateHtml } from '/common/design-system/app-empty-state/error-state.js';
import '/common/features/coding-agent-breakdown.js';
import {
  CORRECTED_HINT, isCorrected, scopedTurnKind, scopedTurnLabel,
} from '/common/utils/coding-agent-breakdown.js';


document.adoptedStyleSheets = [...document.adoptedStyleSheets, styles];

/// Sentinel span id for the synthetic node at the top of each turn's tree. Not
/// a real span — selecting it shows the turn, not a span fetch.
const TURN_ROOT_ID = '__turn_root__';

/// Turn labels in the picker. Long questions are the norm, and the menu is a
/// popover, not a page.
const TURN_LABEL_CHARS = 46;

const fmtInt = (v) => (v == null ? '—' : Number(v).toLocaleString());

/// The four token pills from one set of raw counts.
///
/// `input` is the *fresh* prompt — the provider bills cached prompt tokens at a different
/// rate, so they are counted apart. The pills add them back: "Input tokens" is the whole
/// prompt, which is what a reader means by the word. Reporting the fresh count alone made
/// an unchanged turn look smaller as the cache warmed — the same six prompts replayed two
/// minutes apart read 2,873 tokens and then 957, for prompts of 5,305 and 5,309.
///
/// `Total = Input + Output` holds, and "Cache tokens" is the cached slice *of Input* shown
/// again for cost context — it is not a fifth class to be added on top.
///
/// There is no output-side cache: both cached counts are prompt tokens
/// (`cache_read_input_tokens`, `cache_creation_input_tokens`), so "Output tokens" is the
/// generated tokens alone.
function tokenPills({ input, output, cacheRead, cacheCreation }) {
  const known = [input, output, cacheRead, cacheCreation].some((v) => v != null);
  if (!known) return null;
  const fresh = input ?? 0;
  const read = cacheRead ?? 0;
  // `null` means the read/write split was never carried at this level (the turn view sums
  // them upstream). Say "Cached N" there rather than inventing a "write 0" that is a guess.
  const splitKnown = cacheCreation != null;
  const written = cacheCreation ?? 0;
  const out = output ?? 0;
  const cached = read + written;
  const n = (v) => Number(v).toLocaleString();
  // One part per line: a native `title` renders \n as a line break, and these read as a
  // breakdown rather than a sentence. Keep the total on its own last line so the sum the
  // pill shows is visible next to the parts that make it.
  const lines = (...rows) => rows.filter(Boolean).join('\n');
  const cacheRows = splitKnown
    ? [`Cache read     ${n(read)}`, `Cache write    ${n(written)}`]
    : [`Cached         ${n(cached)}`];
  return {
    total: fresh + cached + out,
    input: fresh + cached,
    output: out,
    cache: cached,
    totalHint: lines(
      `Input (fresh)  ${n(fresh)}`,
      ...cacheRows,
      `Output         ${n(out)}`,
      `─────`,
      `Total          ${n(fresh + cached + out)}`,
    ),
    inputHint: lines(
      `Fresh          ${n(fresh)}`,
      `Cached         ${n(cached)}`,
      `─────`,
      `Input          ${n(fresh + cached)}`,
    ),
    outputHint: lines(
      `Generated      ${n(out)}`,
      `Prompt caching does not apply to output.`,
    ),
    cacheHint: lines(
      ...cacheRows,
      `─────`,
      `Cache          ${n(cached)}`,
      `Already counted inside Input.`,
    ),
  };
}

/// The same four pills as `<dt>/<dd>` rows, for the detail panes.
const pillRows = (p) => (p == null
  ? [['Total tokens', '—', ''], ['Input tokens', '—', ''], ['Output tokens', '—', ''], ['Cache tokens', '—', '']]
  : [
    ['Total tokens', fmtInt(p.total), p.totalHint],
    ['Input tokens', fmtInt(p.input), p.inputHint],
    ['Output tokens', fmtInt(p.output), p.outputHint],
    ['Cache tokens', fmtInt(p.cache), p.cacheHint],
  ]);
/// Sum two counts that may be absent. Null only when neither side was served:
/// a folded trace with no tokens must not erase the tokens already counted.
const addCounts = (a, b) => (a == null && b == null ? null : (a ?? 0) + (b ?? 0));

/// Merge a content-less trace's usage into `turn` — a HITL resume or a
/// proxy-only hop, folded into the turn it's really part of rather than
/// shown as its own empty entry. Mutates `turn` in place; used both for
/// folding backward into the previous turn and (see #buildTurns) forward
/// into the next one when there's no previous turn yet to fold into.
function foldTraceInto(turn, root, traceId) {
  turn.traceIds.push(traceId);
  turn.totalTokens = addCounts(turn.totalTokens, root.cumulative_token_count_total);
  turn.inputTokens = addCounts(turn.inputTokens, root.input_tokens);
  turn.outputTokens = addCounts(turn.outputTokens, root.output_tokens);
  turn.cacheReadTokens = addCounts(turn.cacheReadTokens, root.cache_read_tokens);
  turn.cacheCreationTokens = addCounts(turn.cacheCreationTokens, root.cache_creation_tokens);
  turn.cost = addCounts(turn.cost, root.trace?.cost_summary?.total?.cost);
  turn.corrected = turn.corrected || isCorrected(root);
  // Max, not sum: the folded trace usually overlaps the one it belongs to
  // (same wall clock, different exporter), so summing double-counts.
  turn.durationMs = root.latency_ms == null ? turn.durationMs
    : Math.max(turn.durationMs ?? 0, root.latency_ms);
}

/// Dollars at 2dp, sub-cent amounts at 4dp. A fixed 2dp renders a $0.0010 turn
/// as "$0.00", and a fixed 4dp renders a real session total as "$4.8200".
const fmtUsd = (v) => {
  if (v == null) return '—';
  const n = Number(v);
  return `$${n.toFixed(Math.abs(n) >= 0.01 ? 2 : 4)}`;
};
const fmtMs = (ms) => {
  if (ms == null) return '—';
  return ms >= 1000 ? `${(ms / 1000).toFixed(2)}s` : `${Math.round(ms)}ms`;
};

/// The "Corrected" badge for legacy Codex figures adjusted on read. Constant
/// markup: the hint is ours, escaped anyway for the attribute context.
const correctedBadgeHtml = () =>
  `<app-badge variant="info" class="corrected-badge" title="${escAttr(CORRECTED_HINT)}">Corrected</app-badge>`;

/// A stat-row item whose figure was corrected: `app-stat-row` draws only text,
/// so the label travels in the value and the explanation in the tooltip.
const markCorrected = (item, corrected) => (corrected
  ? {
    ...item,
    value: `${item.value} · Corrected`,
    hint: item.hint ? `${CORRECTED_HINT}\n\n${item.hint}` : CORRECTED_HINT,
  }
  : item);

class ObservabilitySessionPage extends HTMLElement {
  #initialized = false;
  #sessionId = '';
  #session = null;
  /// One entry per turn, in order: {traceId, traceIds, question, answer, metrics}.
  #turns = [];
  #turnIndex = 0;
  /// Fingerprint of what the turn strip currently shows, so the span poll does
  /// not rebuild identical markup every two seconds.
  #renderedTurnKey = null;
  /// Chat messages keyed by trace_id, for the question/answer text.
  #messages = [];
  #tree = [];           // <app-trace-tree> nodes for the current turn
  #traceOf = new Map(); // node id -> the trace it came from (the span fetch needs both)
  #span = null;         // currently-selected span's detail payload
  #selected = null;     // {traceId, spanId}
  /// Span ids whose children are folded away in the trace tree.
  #collapsed = new Set();
  #tracesState = 'loading';  // loading | ready | empty | error
  #focusTraceId = '';        // ?trace_id= — preselect this trace
  #pollTimer = null;
  #pollDeadline = 0;
  /// Whether any trace of the selected turn came back corrected from the trace
  /// view (`fetchObservabilityTrace`), on top of the turn's own flags.
  #traceCorrected = false;

  connectedCallback() {
    if (this.#initialized) return;
    this.#initialized = true;
    this.innerHTML = `
      <app-module-nav module="observability"></app-module-nav>
      <div class="page-head">
        <!-- A plain link, deliberately not a data-back popper: the module nav's
             session rows lead back here, so every switch is a history entry and
             popping one landed on the previous session. Back means the list. -->
        <app-button variant="tertiary" size="sm" icon-only href="/sessions"
          aria-label="Back">${icons.arrowLeft()}</app-button>
        <!-- Starts as the id and is replaced by the session's title once the
             payload lands (#renderTitle). Not a URL param: that would only
             work when arriving from the list, never on a deep link. -->
        <h1 class="page-title" id="page-title"></h1>
        <button type="button" class="id-chip" aria-label="Copy session ID">
          <span class="id-chip__text"></span>
          <span class="id-chip__icon">${icons.copy('', 14)}</span>
        </button>
      </div>
      <app-stat-row id="kpi-strip" variant="chips" loading="6"></app-stat-row>
      <!-- Fetches its own data from Postgres: independent of the trace backend
           and of fetchObservabilitySession, so it is placed once here and only
           its session-id changes (#enter). Hides itself for non-coding
           sessions and on 404. -->
      <coding-agent-breakdown id="agent-breakdown"></coding-agent-breakdown>
      <section class="turn-strip" id="turn-strip" aria-label="Session turns"></section>
      <div class="panes">
        <section class="pane" id="traces-pane" aria-label="Traces"></section>
        <section class="pane" id="detail-pane" aria-label="Span detail"></section>
      </div>
    `;

    // Delegated on the host: every pane rebuilds its own subtree on refresh, so
    // per-element listeners would be dropped on the next render.
    this.addEventListener('click', (e) => {
      const copy = e.target.closest('[data-copy]');
      if (copy) { this.#copy(copy); return; }

      const step = e.target.closest('[data-step]');
      if (step) { this.#step(Number(step.dataset.step)); return; }

    });

    // Folding is a view change over a tree the page already holds, so it
    // re-renders rather than re-fetching every trace the way it used to.
    this.addEventListener('trace-tree-toggle', (e) => {
      e.detail.expanded ? this.#collapsed.delete(e.detail.id) : this.#collapsed.add(e.detail.id);
      this.#renderTraces();
    });

    this.addEventListener('trace-tree-select', (e) => {
      this.#selectSpan(this.#traceOf.get(e.detail.id), e.detail.id);
    });

    this.addEventListener('menu-select', (e) => {
      if (e.target.id !== 'turn-menu') return;
      this.#goToTurn(Number(e.detail.id));
    });

    // The module nav's session rows point at this same route, so the router
    // keeps the page mounted and only fires `route-update` — without this,
    // clicking a row moved the URL and left the old session on screen.
    this.addEventListener('route-update', this.#onRouteUpdate);
    this.addEventListener('stat-row-retry', this.#onStripRetry);

    this.#enter();
  }

  #onRouteUpdate = () => {
    const params = new URLSearchParams(window.location.search);
    if ((params.get('session_id') || '') === this.#sessionId
      && (params.get('trace_id') || '') === this.#focusTraceId) return;
    this.#enter();
  };

  /** Read the session out of the URL and build the page for it. Called on
   *  mount and on every route-update that names a different session. */
  #enter() {
    const params = new URLSearchParams(window.location.search);
    this.#sessionId = params.get('session_id') || '';
    this.#focusTraceId = params.get('trace_id') || '';

    // Every field below describes the session being left; carried over, the new
    // session renders under the old turn strip, spans and KPIs.
    clearTimeout(this.#pollTimer);
    this.#session = null;
    this.#turns = [];
    this.#turnIndex = 0;
    this.#renderedTurnKey = null;
    this.#messages = [];
    this.#tree = [];
    this.#traceOf.clear();
    this.#span = null;
    this.#selected = null;
    this.#collapsed.clear();
    this.#tracesState = 'loading';
    this.#traceCorrected = false;
    this.querySelector('#agent-breakdown').setAttribute('session-id', this.#sessionId);

    this.querySelector('#page-title').textContent = this.#sessionId;
    const chip = this.querySelector('.page-head .id-chip');
    chip.dataset.copy = this.#sessionId;
    chip.querySelector('.id-chip__text').textContent = this.#sessionId;
    const kpis = this.querySelector('#kpi-strip');
    kpis.hidden = false;
    kpis.setAttribute('loading', '6');
    this.querySelector('#turn-strip').innerHTML =
      '<div class="pane-empty" aria-busy="true"><app-skeleton lines="3"></app-skeleton></div>';
    this.querySelector('#traces-pane').innerHTML =
      '<div class="pane-empty" aria-busy="true"><app-skeleton lines="4"></app-skeleton></div>';
    this.querySelector('#detail-pane').innerHTML =
      '<div class="pane-empty">Select a span to see its details</div>';

    this.#load();
  }

  /**
   * Retry on the KPI strip's failure state. Bound on the host and delegated,
   * because the strip rewrites its own contents on every render — the button
   * that fires this does not survive one.
   */
  #onStripRetry = () => {
    const kpis = this.querySelector('#kpi-strip');
    kpis.removeAttribute('error');
    kpis.setAttribute('loading', '6');
    this.#load();
  };

  disconnectedCallback() {
    this.removeEventListener('route-update', this.#onRouteUpdate);
    this.removeEventListener('stat-row-retry', this.#onStripRetry);
    clearTimeout(this.#pollTimer);
  }

  async #load() {
    // The transcript is what supplies the question and answer text, so it has
    // to be in hand before the turns are built — not raced against them.
    await this.#loadChat();
    await this.#loadSession();
    this.#startPolling();
  }

  /**
   * Agents export spans through a batching OTel exporter, so a trace opened
   * straight after a chat holds only the control plane's own `a2a.dispatch`
   * span — the agent's `a2a.execute` and `ChatCompletion` spans land a few
   * seconds later. Re-fetch for the full export window instead of treating a
   * temporarily stable span count as proof that the trace is complete.
   *
   * The transcript is re-read alongside the spans: turns are built from chat
   * messages, so an assistant row that lands late would otherwise never show.
   */
  #startPolling() {
    const INTERVAL_MS = 2000;
    const WINDOW_MS = 30_000;

    this.#pollDeadline = Date.now() + WINDOW_MS;

    const polling = this.#sessionId;
    const tick = async () => {
      if (Date.now() > this.#pollDeadline || this.#sessionId !== polling) return;
      await this.#loadChat();
      if (this.#sessionId !== polling) return;
      await this.#loadSession();
      if (this.#sessionId !== polling) return;
      this.#pollTimer = setTimeout(tick, INTERVAL_MS);
    };
    this.#pollTimer = setTimeout(tick, INTERVAL_MS);
  }

  async #loadChat() {
    try {
      const resp = await call('fetchChatSession', this.#sessionId);
      this.#messages = resp?.data ?? [];
    } catch {
      // Observability sessions don't always map to a chat session; the trace's
      // own root-span content is the fallback. Deliberately not cleared: the
      // poll re-reads this every 2s, and one failed read must not imply the
      // messages vanished. #enter() resets it when the session changes.
    }
  }

  async #loadSession() {
    let resp;
    try {
      resp = await call('fetchObservabilitySession', this.#sessionId);
    } catch (e) {
      console.error('Session fetch failed:', e);
      this.#tracesState = 'error';
      this.#renderTracesPlaceholder(
        'Traces unavailable',
        'The trace backend could not be reached for this session.',
      );
      this.#renderKpis();
      // The turn strip's own skeleton is only ever cleared by `#renderTurn`,
      // which the success path below reaches and this one does not — so it
      // kept shimmering forever, leaving ~120px of dead animation between
      // the failed KPI strip and the Traces panel. There are no turns to
      // show and nothing still coming.
      this.querySelector('#turn-strip').innerHTML = '';
      return;
    }
    this.#session = resp?.data?.session ?? null;
    this.#renderTitle();
    this.#buildTurns();
    this.#renderKpis();
    this.#renderTurn();
    await this.#loadTurnTrace();
  }

  /// The session's own `title` is the chat title, which is auto-generated and
  /// is usually the literal "New chat" — so the first user message stands in,
  /// the same fallback the session list and the module nav apply. Sessions that
  /// never went through chat have neither, and keep the id the reader navigated
  /// with rather than a blank heading.
  #renderTitle() {
    const t = this.#session?.title;
    const title = (t && t !== 'New chat' ? t : this.#firstQuestion())
      || this.#session?.agent_name || '';
    const el = this.querySelector('#page-title');
    if (!title) return;
    el.textContent = title.replace(/\s+/g, ' ').trim().slice(0, 90);
    // The heading is clipped to one line, so the untruncated text has to be
    // reachable somewhere.
    el.title = title;
  }

  /// The transcript is loaded before the session (#load), so this is available
  /// on the first title render. Falls back to the first trace's root-span input
  /// for BYO-key agents, whose messages carry no trace id.
  #firstQuestion() {
    const msg = this.#messages.find((m) => m.role === 'user')?.content;
    // A subagent's or teammate's prompt is not the user's question.
    const mainTrace = (this.#session?.traces ?? [])
      .find((t) => scopedTurnLabel(t?.root_span?.attributes) === null);
    return this.#plainText(msg || mainTrace?.root_span?.input?.value);
  }

  // ── KPI strip ────────────────────────────────────────────────────────────

  #renderKpis() {
    const s = this.#session;
    const strip = this.querySelector('#kpi-strip');
    // A failed session fetch and a session that simply has no metrics used to
    // fold the strip away identically. They are different answers: one says
    // there is nothing to count, the other that we could not count. The strip
    // fails as one block because one request filled all of it.
    if (!s && this.#tracesState === 'error') {
      strip.hidden = false;
      strip.removeAttribute('loading');
      strip.setAttribute('error', "Couldn't load these metrics");
      return;
    }
    strip.removeAttribute('error');
    // No session, no metrics. Leaving the skeleton up would claim the numbers
    // are still loading, so fold the whole thing away.
    if (!s) {
      strip.items = [];
      strip.hidden = true;
      return;
    }
    strip.hidden = false;
    const cost = s.cost_summary ?? {};
    // The server sets metrics_complete=false when the trace search was capped
    // or a trace failed to load. The totals are a lower bound then, and a
    // confident number is worse than an em dash — fmtInt/fmtUsd render null
    // as "—" already, so blanking the value is enough.
    const whole = (v) => (s.metrics_complete === false ? null : v);
    const pills = s.metrics_complete === false ? null : tokenPills({
      input: cost.prompt?.tokens,
      output: cost.completion?.tokens,
      cacheRead: s.cache_read_tokens,
      cacheCreation: s.cache_creation_tokens,
    });
    const corrected = isCorrected(s);
    strip.items = [
      { label: 'Total tokens', value: fmtInt(pills?.total ?? null), hint: pills?.totalHint },
      markCorrected(
        { label: 'Input tokens', value: fmtInt(pills?.input ?? null), hint: pills?.inputHint },
        corrected),
      { label: 'Output tokens', value: fmtInt(pills?.output ?? null), hint: pills?.outputHint },
      { label: 'Cache tokens', value: fmtInt(pills?.cache ?? null), hint: pills?.cacheHint },
      markCorrected({ label: 'Total cost', value: fmtUsd(whole(cost.total?.cost)) }, corrected),
      // P50 here and on the session list, so the same session reads the same
      // number on both screens. `latency_avg` is served alongside it.
      { label: 'Latency P50', value: fmtMs(s.latency_p50) },
    ];
  }

  // ── Turns ────────────────────────────────────────────────────────────────

  /**
   * One turn per trace, except traces with no message of their own, which fold
   * into the turn before them (HITL resumes and proxy-only hops) — or, when
   * there is no previous turn yet (the content-less trace is first, or every
   * trace so far has been content-less), into the next real turn instead.
   * Without that second direction, a leading content-less trace had nothing
   * to fold into and wrongly became its own blank turn — real usage numbers
   * attached to a card with no question or answer, and the turn count one
   * higher than the number of actual exchanges.
   * `chat_messages.trace_id` is what ties a turn's text to its spans; the
   * pairing walks the transcript in order so a user row is matched with the
   * assistant row that answered it.
   */
  #buildTurns() {
    const traces = this.#session?.traces ?? [];
    const byTrace = new Map();
    let pendingUser = null;
    for (const m of this.#messages) {
      if (m.role === 'user') { pendingUser = m; continue; }
      if (!m.trace_id) { pendingUser = null; continue; }
      byTrace.set(m.trace_id, { user: pendingUser, assistant: m });
      pendingUser = null;
    }

    this.#turns = [];
    // Content-less traces seen before any real turn exists yet — held here
    // and folded forward into the next real turn once one is pushed.
    let pendingFold = [];
    for (const entry of traces) {
      const root = entry.root_span ?? {};
      const pair = byTrace.get(entry.trace_id);
      // `||` not `??`: the server serializes "no content" as an empty
      // string, which must fall through to the next source.
      // Subagent/teammate traces are labelled from their root-span attributes:
      // their input is the delegated task, not something the user asked.
      const scopedLabel = scopedTurnLabel(root.attributes);
      const question = this.#plainText(pair?.user?.content || root.input?.value);
      const answer = this.#plainText(pair?.assistant?.content || root.output?.value);
      const prev = this.#turns[this.#turns.length - 1];
      // A trace carrying neither a question nor an answer is not a turn of
      // its own — it is the rest of some other turn (a HITL resume, a
      // proxy-only hop). Fold it into the turn before it so the reader sees
      // one chat entry with both traces under its root, not an empty second
      // entry — or, with no previous turn yet, hold it for the next one.
      // A scoped trace is always its own turn: it has a label even when content
      // capture was off, and folding it would hide who did the work.
      if (!scopedLabel && !question && !answer) {
        if (prev) {
          foldTraceInto(prev, root, entry.trace_id);
        } else {
          pendingFold.push({ traceId: entry.trace_id, root });
        }
        continue;
      }
      const turn = {
        traceId: entry.trace_id,
        // Built below: any content-less traces held from before this turn
        // come first (they're chronologically earlier), then this trace —
        // traceIds order must match wall-clock order, since the span-tree
        // view later zips fetched trace details back to these ids by index.
        traceIds: [],
        question,
        answer,
        scopedLabel,
        scopedKind: scopedTurnKind(root.attributes),
        corrected: isCorrected(root) || isCorrected(pair?.assistant),
        startTime: root.start_time,
        // Coding-agent turns carry the steps they took on the message itself.
        toolCalls: pair?.assistant?.metadata?.coding_agent?.tool_calls ?? null,
        totalTokens: root.cumulative_token_count_total,
        // The trace's own counts first — they cover BYO-key agents too. The
        // chat message's usage is the fallback for turns whose spans carried
        // no token attributes.
        inputTokens: root.input_tokens ?? pair?.assistant?.input_tokens ?? null,
        outputTokens: root.output_tokens ?? pair?.assistant?.output_tokens ?? null,
        // Same fallback as the two above. Without it a turn whose spans carried no cache
        // attribute showed real input and output next to an em dash for cache, even though
        // the chat message row had the number all along (migration 0037).
        // Prefer the trace's own counts, then the chat message row — the same fallback
        // input/output already had. `/api/chat/sessions/{id}/messages` carries both halves
        // (migration 0037), so the turn keeps the real read/write split rather than a sum.
        cacheReadTokens: root.cache_read_tokens ?? pair?.assistant?.cache_read_tokens ?? null,
        cacheCreationTokens:
          root.cache_creation_tokens ?? pair?.assistant?.cache_creation_tokens ?? null,
        cost: root.trace?.cost_summary?.total?.cost ?? null,
        durationMs: root.latency_ms ?? pair?.assistant?.duration_ms ?? null,
      };
      for (const p of pendingFold) foldTraceInto(turn, p.root, p.traceId);
      pendingFold = [];
      turn.traceIds.push(entry.trace_id);
      this.#turns.push(turn);
    }

    // Every trace in the session was content-less — there is no real turn to
    // fold into. Surface the accumulated usage as its own turn rather than
    // silently dropping real data.
    if (pendingFold.length) {
      const turn = {
        traceId: pendingFold[0].traceId,
        traceIds: [],
        question: '',
        answer: '',
        scopedLabel: null,
        scopedKind: null,
        corrected: false,
        startTime: null,
        toolCalls: null,
        totalTokens: null,
        inputTokens: null,
        outputTokens: null,
        cacheReadTokens: null,
        cacheCreationTokens: null,
        cost: null,
        durationMs: null,
      };
      for (const p of pendingFold) foldTraceInto(turn, p.root, p.traceId);
      this.#turns.push(turn);
    }

    // ?trace_id= (from a chat's "Detailed trace") opens on that turn. Only on
    // the first build — a poll must not yank the reader back.
    if (this.#focusTraceId) {
      const i = this.#turns.findIndex((t) => t.traceIds.includes(this.#focusTraceId));
      if (i >= 0) this.#turnIndex = i;
      this.#focusTraceId = '';
    }
    this.#turnIndex = Math.min(this.#turnIndex, Math.max(0, this.#turns.length - 1));
  }

  #turn() { return this.#turns[this.#turnIndex] ?? null; }

  #step(delta) {
    this.#goToTurn(this.#turnIndex + delta);
  }

  async #goToTurn(index) {
    if (!this.#turns.length) return;
    const next = Math.max(0, Math.min(index, this.#turns.length - 1));
    if (next === this.#turnIndex) return;
    this.#turnIndex = next;
    // A different turn is a different trace, so the previous turn's selection
    // means nothing here.
    this.#selected = null;
    this.#renderTurn();
    await this.#loadTurnTrace();
  }

  #renderTurn() {
    const strip = this.querySelector('#turn-strip');
    const turn = this.#turn();
    if (!turn) {
      strip.innerHTML = `<div class="pane-empty">No turns recorded for this session</div>`;
      this.#renderedTurnKey = null;
      return;
    }
    // The span poll re-runs this every 2s for half a minute. Rebuilding
    // identical markup would close an open turn picker under the reader's
    // cursor and throw away any "Show more" they had expanded.
    const key = JSON.stringify([this.#turnIndex, this.#turns.length, turn]);
    if (key === this.#renderedTurnKey) return;
    this.#renderedTurnKey = key;
    const total = this.#turns.length;
    const items = this.#turns.map((t, i) => ({
      id: String(i),
      label: `${i + 1}. ${(t.scopedLabel || t.question || '(no question recorded)')
        .replace(/\s+/g, ' ').trim().slice(0, TURN_LABEL_CHARS)}`,
    }));

    strip.innerHTML = `
      <div class="turn-nav">
        <button type="button" class="turn-step" data-step="-1"
          ${this.#turnIndex === 0 ? 'disabled' : ''} aria-label="Previous turn"
        >${icons.chevronUp('', 14)}</button>
        <app-menu id="turn-menu" align="center" label="Jump to a turn"
          items='${escAttr(JSON.stringify(items))}'
        ><button type="button" class="turn-count" title="Jump to a turn"
          ><span class="turn-count-current">${this.#turnIndex + 1}</span>/${total}</button></app-menu>
        <button type="button" class="turn-step" data-step="1"
          ${this.#turnIndex === total - 1 ? 'disabled' : ''} aria-label="Next turn"
        >${icons.chevronDown('', 14)}</button>
      </div>
      <div class="turn-body">
        <div class="turn-line">
          <span class="turn-glyph" aria-hidden="true">${icons.helpCircle('', 16)}</span>
          <!-- .turn-text is a column: #applyClamps inserts its "Show more"
               button as the clamped block's next sibling, and directly inside
               the row-flex .turn-line that puts it beside the text. -->
          <div class="turn-text">
            <!-- Literal user input: escaped, never parsed as markdown, and
                 pre-wrap so a pasted stack trace keeps its lines. Blank lines
                 are dropped *here only*: the preview is two lines tall, and a
                 question whose second line is the paragraph break spent one of
                 them on whitespace — a one-line question with a gap under it.
                 The detail pane below keeps the text as written. -->
            ${turn.scopedLabel ? `<div class="turn-scoped">
              <span class="turn-scoped-label">${escHtml(turn.scopedLabel)}</span>
              <app-badge variant="neutral">${escHtml(turn.scopedKind ?? 'unknown')}</app-badge>
            </div>` : ''}
            <div class="turn-question msg-clamp">${escHtml(
              (turn.question || (turn.scopedLabel ? 'No task text recorded' : 'No question recorded for this turn'))
                .replace(/\n\s*\n/g, '\n'))}</div>
          </div>
        </div>
        <div class="turn-line">
          <span class="turn-glyph" aria-hidden="true">${icons.message('', 16)}</span>
          <div class="turn-text">
            <!-- Agent replies are markdown, as they are in the chat transcript
                 this text comes from. Rendering the source verbatim put "##"
                 and "**" on screen and collapsed every newline into one
                 paragraph. -->
            ${Array.isArray(turn.toolCalls) ? '<agent-steps></agent-steps>' : ''}
            <div class="turn-answer msg-clamp md-body">${turn.answer
              ? renderMarkdown(turn.answer)
              : escHtml('No answer recorded for this turn')}</div>
          </div>
        </div>
        <div class="turn-meta">
          <app-stat-row variant="chips" id="turn-metrics"></app-stat-row>
          ${turn.corrected ? correctedBadgeHtml() : ''}
          <span class="turn-time">${escHtml(this.#fmtDate(turn.startTime))}</span>
        </div>
      </div>
    `;
    // Set after the markup lands: agent-steps takes the calls through a
    // method, not an attribute.
    strip.querySelector('agent-steps')?.loadToolCalls(turn.toolCalls);
    // Same four pills as the session strip and the detail panes. `Total` comes from the
    // parts rather than the trace's own `cumulative_token_count_total`, so Total = Input +
    // Output holds here too and the three surfaces cannot drift apart.
    const turnPills = tokenPills({
      input: turn.inputTokens,
      output: turn.outputTokens,
      cacheRead: turn.cacheReadTokens,
      cacheCreation: turn.cacheCreationTokens,
    });
    this.querySelector('#turn-metrics').items = [
      { label: 'Total tokens', value: fmtInt(turnPills?.total ?? null), hint: turnPills?.totalHint },
      { label: 'Input tokens', value: fmtInt(turnPills?.input ?? null), hint: turnPills?.inputHint },
      { label: 'Output tokens', value: fmtInt(turnPills?.output ?? null), hint: turnPills?.outputHint },
      { label: 'Cache tokens', value: fmtInt(turnPills?.cache ?? null), hint: turnPills?.cacheHint },
      markCorrected({ label: 'Cost', value: fmtUsd(turn.cost) }, turn.corrected),
      { label: 'Duration', value: fmtMs(turn.durationMs) },
    ];
    // A whole answer can run to thousands of characters; without this the strip
    // grew until it pushed the trace tree off the fold.
    this.#applyClamps(strip);
  }

  // ── Traces ───────────────────────────────────────────────────────────────

  /**
   * The span tree for the selected turn only. This used to fetch every trace in
   * the session up front — one sequential request per turn — to build a single
   * flat list; the tree is per-turn, so all but one of those were thrown away.
   */
  async #loadTurnTrace() {
    const turn = this.#turn();
    this.#traceCorrected = false;
    if (!turn) {
      this.#tree = [];
      this.#renderTraces();
      return;
    }
    let details;
    try {
      details = await Promise.all(
        turn.traceIds.map((id) => call('fetchObservabilityTrace', id)));
    } catch (e) {
      console.warn(`Trace ${turn.traceIds.join(', ')} fetch failed:`, e);
      this.#tracesState = 'error';
      this.#renderTracesPlaceholder(
        'Traces unavailable',
        'The trace backend could not be reached for this turn.',
      );
      return;
    }
    // The turn is the root of its own tree. Tempo hands back whatever span
    // happened to start the trace (`a2a.dispatch`, `request`, …), which says
    // nothing about the chat message that caused it — so the query the reader
    // asked sits at the top and every span hangs beneath it, matching
    // `NAM → dept  session.run` in the design. A turn that folded in a
    // message-less trace roots that trace's spans here too.
    this.#traceCorrected = details.some((detail) => isCorrected(detail));
    const roots = details.flatMap((detail, i) =>
      (detail?.spans ?? []).map((node) => ({ node, traceId: turn.traceIds[i] })));

    // Folding and indentation belong to <app-trace-tree>; the page's job is to
    // hand it the whole tree and remember which trace each node came from,
    // because the span fetch is keyed on both.
    this.#traceOf = new Map([[TURN_ROOT_ID, turn.traceId]]);
    const seen = new Set();
    this.#tree = [{
      id: TURN_ROOT_ID,
      label: turn.scopedLabel || this.#sessionId,
      meta: 'session.run',
      icon: 'trace',
      // Wall-clock for the whole turn, not the sum of its parts: spans overlap.
      duration: fmtMs(turn.durationMs ?? null),
      // One errored span anywhere under the turn makes the turn an error.
      status: roots.some((r) => this.#subtreeHasError(r.node)) ? 'error' : 'ok',
      children: roots
        .map(({ node, traceId }) => this.#treeNode(node, traceId, seen))
        .filter(Boolean),
    }];
    this.#renderTraces();

    // Keep whatever the reader picked; only auto-select when nothing is
    // selected yet or a poll dropped the selected span from the tree.
    if (this.#selected && this.#traceOf.has(this.#selected.spanId)) return;
    this.#selectSpan(turn.traceId, TURN_ROOT_ID);
  }

  /**
   * One span mapped onto the tree component's generic node shape. `seen` is
   * keyed on trace + span so a span that legitimately appears in two traces is
   * kept, while a cyclic `children` chain terminates.
   */
  #treeNode(node, traceId, seen) {
    const key = `${traceId}:${node.span_id}`;
    if (seen.has(key)) return null;
    seen.add(key);
    this.#traceOf.set(node.span_id, traceId);
    return {
      id: node.span_id,
      label: node.name,
      meta: node.operation ?? null,
      icon: this.#spanIcon(node),
      status: this.#isError(node.status_code) ? 'error' : 'ok',
      duration: fmtMs(node.latency_ms),
      children: (node.children || [])
        .map((c) => this.#treeNode(c, traceId, seen)).filter(Boolean),
    };
  }

  /**
   * Trace pane with nothing to show. The span-detail pane is meaningless
   * without a span to select, so `.traces-empty` folds it away and this one
   * empty state takes both columns.
   */
  /**
   * The trace pane's placeholder, for all three of its non-data states.
   *
   * Which one it is comes off `#tracesState`, which every caller has already
   * set on the line above — rather than from an icon each passes in. That is
   * what keeps the failure states drawing the shared failure look instead of
   * each picking a glyph: the two that set `error` used to hand over
   * `icons.xCircle()`, which reads as a plain absence, so "the backend could
   * not be reached" was dressed the same way as "nothing was recorded here".
   *
   * @param {string} heading
   * @param {string} description
   * @param {string} [icon] Markup for the glyph, for the non-error states
   *   only; `variant="error"` brings its own.
   */
  #renderTracesPlaceholder(heading, description, icon) {
    const failed = this.#tracesState === 'error';
    this.querySelector('#traces-pane').innerHTML = `
      ${this.#tracesTitle()}
      <app-empty-state ${failed ? 'variant="error"' : ''}
        heading="${escHtml(heading)}" description="${escHtml(description)}"
        ${failed ? '' : `icon='${icon || ''}'`}></app-empty-state>
    `;
    this.#syncPanes();
  }

  /** Fold away panes that have no content to carry. */
  #syncPanes() {
    const panes = this.querySelector('.panes');
    if (!panes) return;
    panes.classList.toggle('traces-empty', this.#tracesState === 'empty' || this.#tracesState === 'error');
  }

  #tracesTitle() {
    const traceId = this.#turn()?.traceId;
    return `<h2 class="pane-title">Traces${traceId ? `
      <button type="button" class="id-chip" data-copy="${escAttr(traceId)}"
        aria-label="Copy trace ID">
        <span class="id-chip__text">${escHtml(traceId)}</span>
        <span class="id-chip__icon">${icons.copy('', 14)}</span>
      </button>` : ''}${traceId && this.#traceCorrected ? ` ${correctedBadgeHtml()}` : ''}</h2>`;
  }

  #renderTraces() {
    const pane = this.querySelector('#traces-pane');
    if (!this.#tree.length) {
      this.#tracesState = 'empty';
      this.#renderTracesPlaceholder(
        'No traces for this turn',
        'Nothing was recorded here. Spans appear once an instrumented agent handles a request in this session.',
        icons.trace(),
      );
      return;
    }
    this.#tracesState = 'ready';
    this.#syncPanes();
    pane.innerHTML = `
      ${this.#tracesTitle()}
      <app-trace-tree label="Trace spans"
        spans='${escAttr(JSON.stringify(this.#tree))}'
        collapsed='${escAttr(JSON.stringify([...this.#collapsed]))}'
        value="${escAttr(this.#selected?.spanId ?? '')}"></app-trace-tree>
    `;
  }

  #markSelected() {
    this.querySelector('app-trace-tree')?.setAttribute('value', this.#selected?.spanId ?? '');
  }

  // ── Span detail ──────────────────────────────────────────────────────────

  async #selectSpan(traceId, spanId) {
    this.#selected = { traceId, spanId };
    this.#markSelected();
    const pane = this.querySelector('#detail-pane');

    // The turn root is not a span — there is nothing to fetch. It carries the
    // chat message itself, so selecting it shows the query and the answer.
    if (spanId === TURN_ROOT_ID) {
      this.#span = null;
      this.#renderTurnDetail();
      return;
    }

    pane.innerHTML = '<div class="pane-empty" aria-busy="true"><app-skeleton lines="4"></app-skeleton></div>';
    let resp;
    try {
      resp = await call('fetchSpanDetail', traceId, spanId);
    } catch (e) {
      console.error('Span fetch failed:', e);
      // Was a bare line where the skeleton had been — true, but nothing
      // that looked like the rest of the product and no way to try again.
      pane.innerHTML = errorStateHtml("Couldn't load this span");
      pane.querySelector('[data-retry]')
        ?.addEventListener('click', () => this.#selectSpan(traceId, spanId));
      return;
    }
    this.#span = resp?.data?.span ?? null;
    this.#renderDetail();
  }

  /** Detail pane for the turn root: the chat message and the turn's totals. */
  #renderTurnDetail() {
    const pane = this.querySelector('#detail-pane');
    const turn = this.#turn();
    if (!turn) {
      pane.innerHTML = '<div class="pane-empty">Select a span to see its details</div>';
      return;
    }
    pane.innerHTML = `
      <div class="detail-head">
        <h3>${escHtml(turn.scopedLabel || this.#sessionId)}</h3>
        <app-badge variant="info">session.run</app-badge>
      </div>
      <div class="detail-section-title">${turn.scopedLabel ? 'Task' : 'User'}</div>
      <div class="msg-block"><div class="msg-content msg-clamp">${escHtml(
        turn.question || (turn.scopedLabel ? 'No task text recorded' : 'No question recorded for this turn'))}</div></div>
      <div class="detail-section-title">Assistant</div>
      <div class="msg-block"><div class="msg-content msg-clamp md-body">${turn.answer
        ? renderMarkdown(turn.answer)
        : escHtml('No answer recorded for this turn')}</div></div>
      <div class="detail-section-title">Usage</div>
      <dl class="kv">
        ${pillRows(tokenPills({
          input: turn.inputTokens,
          output: turn.outputTokens,
          cacheRead: turn.cacheReadTokens,
          cacheCreation: turn.cacheCreationTokens,
        })).map(([k, v, hint]) => `
        <dt${hint ? ` title="${escAttr(hint)}"` : ''}>${escHtml(k)}</dt>
        <dd${hint ? ` title="${escAttr(hint)}"` : ''}>${escHtml(v)}</dd>`).join('')}
        <dt>Cost</dt><dd>${escHtml(fmtUsd(turn.cost))}${
          turn.corrected || this.#traceCorrected ? ` ${correctedBadgeHtml()}` : ''}</dd>
        <dt>Duration</dt><dd>${escHtml(fmtMs(turn.durationMs))}</dd>
      </dl>
    `;
    this.#applyClamps(pane);
  }

  #renderDetail() {
    const s = this.#span;
    const pane = this.querySelector('#detail-pane');
    if (!s) {
      pane.innerHTML = '<div class="pane-empty">Select a span to see its details</div>';
      return;
    }
    const attrs = s.attributes ?? {};
    // Served as top-level fields; the nested semconv attributes stay as the
    // fallback for spans recorded before that promotion.
    const provider = s.provider ?? attrs.gen_ai?.system ?? null;
    const model = s.model ?? attrs.gen_ai?.request?.model ?? attrs.gen_ai?.response?.model ?? null;

    pane.innerHTML = `
      <div class="detail-head">
        <h3>${escHtml(s.name)}</h3>
        <app-badge variant="info">${escHtml(s.span_kind || 'internal')}</app-badge>
      </div>
      ${provider || model ? `<div class="detail-origin">
        ${provider ? `<span><b>Provider:</b> ${escHtml(provider)}</span>` : ''}
        ${model ? `<span><b>Model:</b> ${escHtml(model)}</span>` : ''}
      </div>` : ''}
      <app-tabs label="Span sections">
        <div data-tab="input" data-label="Input">${this.#inputTabHtml()}</div>
        <div data-tab="usage" data-label="Usage">${this.#usageTabHtml()}</div>
        <div data-tab="events" data-label="Metadata &amp; events">${this.#eventsTabHtml()}</div>
        <div data-tab="raw" data-label="Raw attributes"><pre class="raw-json">${
          escHtml(JSON.stringify(attrs, null, 2))}</pre></div>
      </app-tabs>
      <app-tabs class="output-tabs" label="Span output">
        <div data-tab="output" data-label="${escHtml(this.#outputTitle())}">${this.#outputHtml()}</div>
      </app-tabs>
    `;
    // Every panel is rendered up front — app-tabs owns the switch, so there is
    // no re-render to hang the clamp pass off. #applyClamps measures, and a
    // hidden panel measures as zero, so only the visible one gets a toggle.
    this.#applyClamps(pane);
  }

  /** Messages from the span, as `{role, content}` pairs. */
  #messagesFor(which) {
    const s = this.#span;
    const attrs = s?.attributes ?? {};
    // tempo.rs builds a flat dotted-key attribute map, but the span-detail API
    // re-nests it before serializing (`unflatten_attrs` in
    // oss/server/src/observability/service.rs), so the wire shape is
    // `attributes.gen_ai.input.messages` — a flat `attrs['gen_ai.input.messages']`
    // lookup can never match. The server also resolves the raw content into
    // `input.value`/`output.value`, so that field is the primary source here.
    //
    // `llm.*` is the older OpenInference convention, kept first for spans
    // recorded before OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental
    // — a fallback chain rather than a version check, matching how this repo
    // handles A2A payload drift. `||` not `??`: the server serializes "no
    // content" as an empty string, which must fall through.
    return which === 'input'
      ? this.#extractMessages(attrs.llm?.input_messages,
        s?.input?.value || attrs.gen_ai?.input?.messages || s?.input_content)
      : this.#extractMessages(attrs.llm?.output_messages,
        s?.output?.value || attrs.gen_ai?.output?.messages || s?.output_content);
  }

  #msgBlocksHtml(msgs, emptyText) {
    if (!msgs.length) return `<div class="pane-empty">${escHtml(emptyText)}</div>`;
    return msgs.map((m) => `
      <div class="msg-block">
        <div class="msg-role">${escHtml(m.role || '')}</div>
        <!-- Escaped, not markdown: span payloads are often raw JSON or tool
             output, which a markdown pass would mangle. -->
        <div class="msg-content msg-clamp">${escHtml(m.content || '')}</div>
      </div>`).join('');
  }

  #inputTabHtml() {
    const attrs = this.#span?.attributes ?? {};
    if (!this.#isToolSpan(attrs)) {
      return this.#msgBlocksHtml(this.#messagesFor('input'), 'No input message available');
    }
    const s = this.#span;
    return `
      <div class="detail-section-title">Tool execution</div>
      <div class="msg-block">
        <div class="msg-content">${escHtml([
          `Tool: ${attrs.tool?.name || 'unknown'}`,
          `Status: ${attrs.tool?.status || s.status_code || 'unknown'}`,
          `Duration: ${fmtMs(s.latency_ms)}`,
          attrs.tool?.call?.id ? `Call ID: ${attrs.tool.call.id}` : '',
        ].filter(Boolean).join('\n'))}</div>
      </div>
      <div class="detail-section-title">Arguments</div>
      ${this.#msgBlocksHtml(this.#messagesFor('input'), 'No arguments captured')}
    `;
  }

  /** Tool spans name their output by outcome; everything else is just "Output". */
  #outputTitle() {
    const attrs = this.#span?.attributes ?? {};
    if (!this.#isToolSpan(attrs)) return 'Output';
    return attrs.tool?.status === 'failed' ? 'Error' : 'Result';
  }

  #outputHtml() {
    return this.#msgBlocksHtml(this.#messagesFor('output'),
      this.#isToolSpan(this.#span?.attributes ?? {})
        ? 'No result captured'
        : 'No output message available');
  }

  /** Per-span usage: the token split, the cache counts and the cost. */
  #usageTabHtml() {
    const s = this.#span;
    const cost = s.cost_summary ?? {};
    const rows = [
      ...pillRows(tokenPills({
        input: cost.prompt?.tokens ?? s.input_tokens,
        output: cost.completion?.tokens ?? s.output_tokens,
        cacheRead: s.cache_read_tokens,
        cacheCreation: s.cache_creation_tokens,
      })),
      ['Input cost', fmtUsd(cost.prompt?.cost), ''],
      ['Output cost', fmtUsd(cost.completion?.cost), ''],
      ['Total cost', fmtUsd(cost.total?.cost), '', isCorrected(s)],
      ['Latency', fmtMs(s.latency_ms), ''],
    ];
    return `<dl class="usage-grid">${rows.map(([k, v, hint, corrected]) => `
      <dt${hint ? ` title="${escAttr(hint)}"` : ''}>${escHtml(k)}</dt>
      <dd${hint ? ` title="${escAttr(hint)}"` : ''}>${escHtml(v)}${
        corrected ? ` ${correctedBadgeHtml()}` : ''}</dd>`).join('')}</dl>`;
  }

  /**
   * Span events plus the status line. Instrumentation that emits no events at
   * all is normal, so the empty branch states that rather than reading as a
   * failure.
   */
  #eventsTabHtml() {
    const s = this.#span;
    const meta = [
      ['Span ID', s.span_id],
      ['Parent span', s.parent_id || '—'],
      ['Status', s.status_code || '—'],
      ['Status message', s.status_message || '—'],
      ['Started', this.#fmtDate(s.start_time)],
      ['Ended', this.#fmtDate(s.end_time)],
    ];
    const events = Array.isArray(s.events) ? s.events : [];
    return `
      <dl class="usage-grid">${meta.map(([k, v]) => `
        <dt>${escHtml(k)}</dt><dd>${escHtml(String(v))}</dd>`).join('')}</dl>
      <div class="detail-section-title">Events</div>
      ${events.length
        ? events.map((ev) => `<pre class="raw-json">${escHtml(JSON.stringify(ev, null, 2))}</pre>`).join('')
        : '<div class="pane-empty">No events recorded for this span</div>'}
    `;
  }

  /**
   * Trace root input/output carry the raw agent payload, which for HITL turns
   * is a serialized GenAI message array rather than prose. Flatten it to the
   * text a reader expects; anything that isn't a message envelope (ordinary
   * chat content included) falls through unchanged.
   */
  #plainText(raw) {
    if (!raw) return '';
    const text = this.#extractMessages(null, raw)
      .map((m) => m.content).filter(Boolean).join('\n\n').trim();
    return text || String(raw);
  }

  /** Messages may live in OTel genai attributes or in the raw input/output value. */
  #extractMessages(attrMsgs, rawValue) {
    if (Array.isArray(attrMsgs) && attrMsgs.length) {
      return attrMsgs.map((m) => {
        const msg = m.message || m;
        return { role: msg.role, content: typeof msg.content === 'string' ? msg.content : JSON.stringify(msg.content) };
      });
    }
    if (!rawValue) return [];
    try {
      const parsed = typeof rawValue === 'string' ? JSON.parse(rawValue) : rawValue;
      if (Array.isArray(parsed?.messages)) {
        return parsed.messages.map((m) => ({
          role: m.role,
          content: typeof m.content === 'string' ? m.content : JSON.stringify(m.content),
        }));
      }
      // GenAI semconv shape (gen_ai.input/output.messages):
      // [{role, parts: [{type:"text",content}|{type:"tool_call",name,arguments}|
      //                 {type:"tool_call_response",response}]}]
      if (Array.isArray(parsed)) {
        return parsed.map((m) => ({
          role: m.role || '',
          content: Array.isArray(m.parts) ? m.parts.map((p) => this.#partText(p)).join('\n') : JSON.stringify(m),
        }));
      }
    } catch { /* plain text below */ }
    return [{ role: '', content: String(rawValue) }];
  }

  /** Render one GenAI semconv message part as display text. */
  #partText(p) {
    if (p?.type === 'tool_call') {
      const args = typeof p.arguments === 'string' ? p.arguments : JSON.stringify(p.arguments ?? {});
      return `⚒ ${p.name || 'tool'}(${args})`;
    }
    if (p?.type === 'tool_call_response') {
      return typeof p.response === 'string' ? p.response : JSON.stringify(p.response ?? '');
    }
    if (typeof p?.content === 'string') return p.content;
    return JSON.stringify(p ?? '');
  }

  // ── Shared helpers ───────────────────────────────────────────────────────

  async #copy(btn) {
    try {
      await navigator.clipboard.writeText(btn.dataset.copy || '');
    } catch {
      return; // clipboard denied — leave the button as it was rather than lying
    }
    const icon = btn.querySelector('.id-chip__icon');
    if (!icon) return;
    icon.innerHTML = icons.check('', 14);
    setTimeout(() => { icon.innerHTML = icons.copy('', 14); }, 1500);
  }

  /**
   * Add a Show more/less toggle to every clamped block that actually
   * overflows — measured, so short messages get no stray control.
   */
  #applyClamps(root) {
    root.querySelectorAll('.msg-clamp').forEach((el) => {
      if (el.scrollHeight <= el.clientHeight + 4) return;
      el.classList.add('is-clamped');
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'msg-more';
      btn.textContent = 'Show more';
      btn.addEventListener('click', () => {
        const open = el.classList.toggle('is-expanded');
        btn.textContent = open ? 'Show less' : 'Show more';
      });
      el.after(btn);
    });
  }

  /** True for a GenAI tool-execution span, whichever convention recorded it. */
  #isToolSpan(attrs = {}) {
    return attrs.gen_ai?.operation?.name === 'execute_tool' || !!attrs.tool?.name;
  }

  #spanIcon(node) {
    // ponytail: provider picks the LLM glyph, not a per-vendor mark — the icon
    // set carries no OpenAI/Anthropic logos and the name is on the row already.
    if (node.provider || node.model || node.name?.toLowerCase().includes('chatcompletion')) {
      return 'cube';
    }
    if (this.#isToolSpan(node.attributes) || node.name?.toLowerCase().startsWith('tool')) {
      return 'terminal';
    }
    return 'trace';
  }

  /** True when this span or anything beneath it failed. */
  #subtreeHasError(node) {
    if (this.#isError(node.status_code)) return true;
    return (node.children || []).some((c) => this.#subtreeHasError(c));
  }

  #isError(status) {
    return typeof status === 'string' && status.toUpperCase().includes('ERROR');
  }

  #fmtDate(iso) {
    if (!iso) return '—';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '—';
    return `${d.toLocaleTimeString('en-US', { hour: '2-digit', minute: '2-digit' })} · ${d.toLocaleDateString('en-US', { day: 'numeric', month: 'short', year: 'numeric' })}`;
  }

}

customElements.define('observability-session-page', ObservabilitySessionPage);
