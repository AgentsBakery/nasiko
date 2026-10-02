/**
 * Navigation for the OSS control plane, plus the seam every edition extends.
 *
 * Loaded first on every page (see the module-order note in any page's <head>):
 * it registers `fetchNavigation` / `fetchModuleNav` with the data-sources
 * registry for `<app-header>` and `<app-module-nav>`, and pulls in the shared
 * data functions.
 *
 * ─── Why the extension seam exists ─────────────────────────────────────────
 * EE used to override this entire file. `EeAssets` resolves before `OssAssets`
 * and EE has no HTML override for index/agents/settings/etc., so those pages load
 * THIS file — which meant the only way for EE to add its org nav items was to
 * ship a complete copy. 450 of that copy's 662 lines were identical, and the part
 * that wasn't had silently drifted into a user-visible bug.
 *
 * Now both editions share this file, and edition-specific navigation lives in
 * the `/nav-ext*.js` chain, resolved through the same overlay: this tree holds
 * a documented no-op for every link and the enterprise overlay supplies the EE
 * tree. Nothing 404s, and there is exactly one copy of every data function.
 */

import '/common/services/data-functions.js';
import { call, registerAll } from '/common/core/data-sources.js';
import { extensionChain } from '/common/core/extension-chain.js';

// rail: true → shown as a rail module icon; everything else is reachable
// through the module tree navs and the ⌘F nav search.
const BASE_ITEMS = () => [
  // rail: true → shown as a rail module icon; everything else is reachable
  // through the module tree navs and the ⌘F nav search.
  //
  // module → which MODULE_NAVS tree a page belongs to. The rail item carrying
  // the same key stays selected while any of its children is open, so a child
  // page never leaves the rail with nothing highlighted.
  { title: "Overview", url: "/", icon: "layoutDashboard", rail: true },
  { title: "Orchestrator", url: "/orchestrator", icon: "brain", rail: true, module: "orchestrator" },
  // Not on the rail: workflows are the Orchestrator module's second group, and
  // a second rail icon into the same tree read as a separate module.
  { title: "Workflows", url: "/workflows", icon: "workflow", module: "orchestrator" },
  { title: "Executions", url: "/executions", icon: "play", module: "orchestrator" },
  { title: "Agents", url: "/agents", icon: "bot", rail: true, module: "agents" },
  // Two rail items over chat sessions, deliberately: Sessions is the transcript
  // reader (module nav lists every agent's chats, /chats opens one), Observability
  // is the analytics view over the same rows (traces, tokens, latency).
  { title: "Sessions", url: "/chats", icon: "history", rail: true, module: "sessions" },
  { title: "Observability", url: "/sessions", icon: "activity", rail: true, module: "observability" },
  { title: "MCP gateway", url: "/mcp", icon: "server", rail: true, module: "mcp" },
  { title: "LLM router", url: "/llm-router", icon: "route", rail: true },
  { title: "TokenOps", url: "/tokenops", icon: "banknote", rail: true },
  { title: "Budgets", url: "/budgets", icon: "banknote" },
  { title: "Alerts", url: "/alerts", icon: "bell" },
  { title: "Your Agents", url: "/your-agents", icon: "user", module: "agents" },
  { title: "Add Agent", url: "/add-agent", icon: "plus", module: "agents" },
  { title: "Set up CLI", url: "/setup-cli", icon: "terminal" },
  { title: "Flows", url: "/flows", icon: "cornerUpRight", module: "observability" },
  { title: "Coding sessions", url: "/coding-sessions", icon: "terminal", module: "observability" },
  // In the Observability module tree but missing here, so ⌘F couldn't find it
  // and the rail lost its selection on the page.
  { title: "Resources", url: "/resources", icon: "activity", module: "observability" },
  { title: "Builds", url: "/builds", icon: "cube", module: "agents" },
  { title: "Secrets", url: "/secrets", icon: "lock", module: "settings" },
  { title: "Settings", url: "/settings", icon: "settings", rail: true, module: "settings" },
];

// In-card module tree navs (app-module-nav). Items are either page links
// ({label, url}) or in-page sections ({label, section} → the page handles
// the `module-nav-select` event). Only real pages/features appear here.
const MODULE_NAVS = {
  orchestrator: {
    title: 'Orchestrator', icon: 'brain',
    groups: [
      // A group with a url and no items is a heading-level link (see
      // app-module-nav's #groupHtml) — the entry point sits above the session
      // list, not inside it.
      { label: 'Orchestrate a task', url: '/orchestrator' },
      { label: 'Workflows', items: [
        { label: 'All workflows', url: '/workflows' },
        { label: 'Executions', url: '/executions' },
      ]},
    ],
  },
  mcp: {
    title: 'MCP gateway', icon: 'server',
    groups: [
      // Scope rows filter the unified catalog grid; ownership scopes apply
      // to custom MCP servers only (toolkits are platform-registered).
      // Every `section` here must be a key of CATALOG_SCOPES in
      // common/pages/mcp-page.js — a row naming anything else highlights and
      // then does nothing, which is what `created-by-you` and `uploads` did.
      // No separate uploads row: an upload IS a custom server, so it is already
      // under "My servers", carrying its own "Setting up" / "Build failed" chip.
      { label: 'MCP servers', items: [
        { label: 'All', section: 'all' },
        { label: 'My servers', section: 'my-servers' },
        { label: 'Shared with me', section: 'shared-with-me' },
      ]},
      { label: 'Toolkits', items: [
        { label: 'All toolkits', section: 'toolkits' },
      ]},
      // No "Agent access" row: access is granted per connector on
      // /mcp-detail (Access & security) and per agent on the agent card's
      // Configure tab. There is no page-level view of it for a row to open.
    ],
  },
  agents: {
    title: 'Agent registry', icon: 'bot',
    groups: [
      { label: 'Agent sources', items: [
        { label: 'Agent hub', url: '/agents' },
        { label: 'Your agents', url: '/your-agents' },
        { label: 'Import agent', url: '/add-agent' },
      ]},
      { label: 'Builds', items: [
        { label: 'All builds', url: '/builds' },
      ]},
    ],
  },
  observability: {
    title: 'Observability', icon: 'activity',
    groups: [
      // Heading-level links (a group with a url and no items), so the two
      // entry points sit above the dynamic "Recent sessions" group rather
      // than under a "Home" label that names nothing.
      { label: 'All sessions', url: '/sessions' },
      { label: 'Coding sessions', url: '/coding-sessions' },
      { label: 'Resources', url: '/resources' },
    ],
  },
  // Every chat, newest first, under one tree — the group is dynamic, so the
  // static tree is empty and #fetchModuleNav fills it (see sessionItems).
  sessions: { title: 'Sessions', icon: 'history', groups: [] },
  settings: {
    title: 'Settings', icon: 'settings',
    groups: [
      // `url` on a section item names the page that owns the panels. Secrets is
      // a sibling route, not a panel of this page, so from /secrets there is no
      // settings-page listening for `module-nav-select` — without the url these
      // four rows highlighted and did nothing, pinning the content to Secrets.
      { label: 'Workspace', items: [
        { label: 'General', section: 'general', url: '/settings' },
        { label: 'Flow limits', section: 'limits', url: '/settings' },
        { label: 'Registry', section: 'registry', url: '/settings' },
      ]},
      { label: 'Security', items: [
        { label: 'Single sign-on', section: 'sso', url: '/settings' },
        { label: 'Secrets', url: '/secrets' },
      ]},
    ],
  },
};

// Chat rows for a module nav's session group.
//
// `orchestratorOnly` keeps the Orchestrator tree to sessions the orchestrator
// routed — `agent_name: null` is that marker, a direct agent chat carries the
// agent's name and belongs to that agent. The Sessions module lists every
// agent's chats instead. The API has no filter for either, so over-fetch one
// page and filter client-side.
const SESSION_ROWS = 15;
const sessionItems = async ({
  orchestratorOnly = false,
  path = '/chat',
  // The observability tree links to a page that reads `session_id` alone.
  // app-module-nav's active-row match compares *every* param in the row's url
  // against the location, so carrying chat's agent params there would mean no
  // row ever highlights.
  sessionIdOnly = false,
  // Chat rows delete the session; an observability row is a read-only jump and
  // must not put a destructive control in a nav list.
  deletable = true,
} = {}) => {
  try {
    const res = await call('fetchSessions', '', 50);
    return (res?.data || [])
      .filter((s) => !orchestratorOnly || !s.agent_name)
      .slice(0, SESSION_ROWS)
      .map((s) => {
        const params = new URLSearchParams({ session_id: s.session_id });
        if (!sessionIdOnly) {
          // Absent, not empty: app-module-nav's active-row match compares every
          // param in the row's url against the location, and chat-page reads a
          // missing agent_id as "the orchestrator routed this one".
          if (s.agent_id) params.set('agent_id', s.agent_id);
          params.set('agent_name', s.agent_name || 'Orchestrator');
          if (s.is_coding_agent) params.set('read_only', '1');
        }
        return {
          // Present ⇒ app-module-nav renders the row's delete affordance.
          ...(deletable ? { sessionId: s.session_id } : {}),
          // Titles are auto-generated and often the literal "New chat", which
          // makes every row look the same — fall back to the last message.
          // Sliced: a last_message is a whole markdown answer, and the row
          // ellipsises anyway — no reason to carry KBs of it through the cache.
          label: ((s.title && s.title !== 'New chat' ? s.title : s.last_message) || 'New chat')
            .replace(/\s+/g, ' ').trim().slice(0, 60),
          // Same target as an Execution history row: chat-page loads the
          // transcript and posts to /orchestrator/a2a when there's no agent_id.
          url: `${path}?${params}`,
        };
      });
  } catch {
    return []; // a flaky request must not blank the sidebar
  }
};

/**
 * The edition extension chain, loaded once.
 *
 * One link per overlay, base first, each with a no-op in ui/oss/ so every
 * specifier resolves on every surface. An overlay replaces only the file
 * carrying its own suffix, so a higher overlay can add nav entries without
 * shadowing a lower one's away — which a shared `nav-ext.js` name did (NAS-637).
 * The hooks are still resolved through data-sources under a per-layer name, so
 * the seam keeps the DI contract and stays reachable from `__dataSources`; the
 * names are literals here rather than derived from the suffix so they can be
 * grepped from both ends. See common/core/extension-chain.js.
 *
 * @type {Array<[string, string]>}
 */
const NAV_LAYERS = [
  ['/nav-ext.js',    'navExtension'],   // base, this tree's own no-op
  ['/nav-ext-ee.js', 'navExtensionEe'], // the enterprise overlay
  ['/nav-ext-mt.js', 'navExtensionMt'], // the multi-tenant overlay
];

const extensions = extensionChain(NAV_LAYERS, 'navigation');

/**
 * Per-layer extension context (org role, feature flags), fetched at most once
 * per page per layer. A layer's hooks get its OWN context, never a neighbour's
 * — they are different objects from different endpoints.
 *
 * Deliberately lazy: the login page has no session, and eagerly fetching this at
 * module load would 401 on every unauthenticated page load.
 *
 * @type {WeakMap<object, Promise<any>>}
 */
const contexts = new WeakMap();
const extensionContext = (ext) => {
  if (!ext.context) return Promise.resolve(null);
  if (!contexts.has(ext)) {
    contexts.set(ext, Promise.resolve().then(() => ext.context()).catch(() => null));
  }
  return contexts.get(ext);
};

const fetchNavigation = async () => {
  const base = BASE_ITEMS();
  // Folded, base first: each layer receives what the layers below it produced,
  // so a hook that returns its own ordered list (the enterprise one does) is
  // still extensible by the layer above. A layer that throws is skipped and the
  // chain continues with the last good list rather than collapsing to BASE_ITEMS.
  let items = base;
  for (const ext of await extensions()) {
    if (!ext.items) continue;
    try {
      items = (await ext.items(items, await extensionContext(ext))) || items;
    } catch (err) {
      console.error('[navigation] a nav extension items() failed — keeping the layers below it', err);
    }
  }
  return items;
};

const fetchModuleNav = async (module) => {
  const nav = MODULE_NAVS[module];
  let base = nav ? { ...nav, groups: [...nav.groups] } : null;
  // Three trees list sessions: Orchestrator shows the chats it routed under its
  // own entry point, Sessions shows every agent's, Observability links the same
  // rows to their traces.
  //
  // Observability's group was removed once before, on the grounds that on
  // /sessions it restated the first rows of the table beside it. It is back
  // because the module now has a second page — /observability-session, which
  // has no table — and stepping between sessions there is the whole point of
  // the list. It costs the one `fetchSessions` call the other two trees
  // already make.
  if (base && ['orchestrator', 'sessions', 'observability'].includes(module)) {
    const orchestratorOnly = module === 'orchestrator';
    const observability = module === 'observability';
    const sessions = await sessionItems({
      orchestratorOnly,
      path: observability ? '/observability-session' : orchestratorOnly ? '/chat' : '/chats',
      sessionIdOnly: observability,
      deletable: !observability,
    });
    // Last, below the static groups; omitted entirely when empty, since a group
    // with no items and no url renders as a stray heading.
    if (sessions.length) {
      const label = observability ? 'Recent sessions'
        : orchestratorOnly ? 'Session' : 'All sessions';
      base = { ...base, groups: [...base.groups, { label, items: sessions }] };
    }
  }
  // Folded like items(): `base` for a layer is the tree the layers below it
  // returned. Note a hook returns the WHOLE tree (or null for "this module has
  // none"), so returning `base` unchanged is how a layer says "not mine" — see
  // the enterprise hook's final `return base`.
  let tree = base;
  for (const ext of await extensions()) {
    if (!ext.moduleNav) continue;
    try {
      tree = await ext.moduleNav(module, tree, await extensionContext(ext));
    } catch (err) {
      console.error('[navigation] a nav extension moduleNav() failed — keeping the layers below it', err);
    }
  }
  return tree;
};

registerAll({ fetchNavigation, fetchModuleNav }, { replace: true });

export { BASE_ITEMS, MODULE_NAVS };
