/**
 * Alert data functions. `fetchSpikeMarkers` reads
 * `GET /api/alerts/spike-markers` (any authenticated user; the server applies
 * the same agent scoping as the FinOps endpoints). Plain transport.
 */

import { fetchApi } from '/common/services/api.js';
import { registerAll } from '/common/core/data-sources.js';

const fetchSpikeMarkers = async ({ range, startTime, endTime, agentId } = {}) => {
  const q = new URLSearchParams();
  const add = (k, v) => { if (v !== undefined && v !== null && v !== '') q.set(k, String(v)); };
  add('range', range);
  add('start_time', startTime);
  add('end_time', endTime);
  add('agent_id', agentId);
  const s = q.toString();
  return fetchApi(`/alerts/spike-markers${s ? `?${s}` : ''}`);
};

registerAll({ fetchSpikeMarkers }, { replace: true });
