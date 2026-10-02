/**
 * Observability data functions.
 *
 * Split from `data-functions.js` (Phase 3B) — traces, spans, session
 * history, and resource usage.
 */

import { fetchApi } from '/common/services/api.js';
import { registerAll } from '/common/core/data-sources.js';

const fetchTraceDetail = async (traceId) => {
  // Server route: GET /api/observability/trace/{id} (same as `nasiko observe trace`).
  // Envelope {data:{trace}}; trace.spans is a nested tree (children embedded).
  const resp = await fetchApi(`/observability/trace/${traceId}`);
  return resp.data?.trace ?? resp.trace ?? resp;
};

// Paged: every row costs the server one trace-store lookup, so asking for the
// whole history is what made Execution history slow to appear.
const fetchObservabilitySessions = async (limit = 25, offset = 0, { codingOnly = false } = {}) => {
  const params = new URLSearchParams({ limit, offset });
  // Filtered on the server: client-side filtering would break offset paging.
  if (codingOnly) params.set('coding_only', 'true');
  return fetchApi(`/observability/session/list?${params}`);
};

const fetchObservabilitySession = async (sessionId) => {
  return fetchApi(`/observability/session/${encodeURIComponent(sessionId)}`);
};

// Resource usage — host + per-container CPU/memory/IO (admin-only endpoint).
const fetchResourceStats = async () => {
  return fetchApi('/observability/resources');
};

// Owner-scoped: usage for a single agent. Accepts a UUID or an agent name.
const fetchAgentResourceStats = async (agentRef) => {
  return fetchApi(`/observability/agent/${encodeURIComponent(agentRef)}/resources`);
};

const fetchObservabilityTrace = async (traceId) => {
  const resp = await fetchApi(`/observability/trace/${encodeURIComponent(traceId)}`);
  return resp.data?.trace ?? resp.trace ?? resp;
};

const fetchSpanDetail = async (traceId, spanId) => {
  return fetchApi(`/observability/span/${encodeURIComponent(traceId)}/${encodeURIComponent(spanId)}`);
};

registerAll({
  fetchTraceDetail, fetchObservabilitySessions, fetchObservabilitySession,
  fetchResourceStats, fetchAgentResourceStats, fetchObservabilityTrace,
  fetchSpanDetail,
}, { replace: true });
