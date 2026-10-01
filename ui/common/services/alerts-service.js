/**
 * Alert data functions. `fetchSpikeMarkers` reads
 * `GET /api/alerts/spike-markers` (any authenticated user; the server applies
 * the same agent scoping as the FinOps endpoints). Everything else (alerts,
 * monitors, notification channels, routes, test sends, deliveries) is
 * admin-only; the server enforces the gate. Plain transports.
 */

import { fetchApi, postJson, putJson, deleteJson } from '/common/services/api.js';
import { registerAll } from '/common/core/data-sources.js';
import { alertQuery } from '/common/utils/alerts.js';

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

const enc = encodeURIComponent;

const fetchAlerts = async (filters = {}) => {
  const s = alertQuery(filters);
  return fetchApi(`/alerts${s ? `?${s}` : ''}`);
};
const acknowledgeAlert = async ({ id }) => postJson(`/alerts/${enc(id)}/acknowledge`, {});

const fetchMonitors = async () => fetchApi('/monitors');
const createMonitor = async (body) => postJson('/monitors', body);
const updateMonitor = async ({ id, ...body }) => putJson(`/monitors/${enc(id)}`, body);
const deleteMonitor = async ({ id }) => deleteJson(`/monitors/${enc(id)}`);

const fetchChannels = async () => fetchApi('/notification-channels');
const createChannel = async (body) => postJson('/notification-channels', body);
const updateChannel = async ({ id, ...body }) => putJson(`/notification-channels/${enc(id)}`, body);
const deleteChannel = async ({ id }) => deleteJson(`/notification-channels/${enc(id)}`);
const fetchChannelRoutes = async ({ id }) => fetchApi(`/notification-channels/${enc(id)}/routes`);
const saveChannelRoutes = async ({ id, routes }) =>
  putJson(`/notification-channels/${enc(id)}/routes`, { routes });
const testChannel = async ({ id }) => postJson(`/notification-channels/${enc(id)}/test`, {});

const fetchDeliveries = async ({ alertId, channelId, limit } = {}) => {
  const q = new URLSearchParams();
  if (alertId) q.set('alert_id', String(alertId));
  if (channelId) q.set('channel_id', String(channelId));
  if (limit) q.set('limit', String(limit));
  const s = q.toString();
  return fetchApi(`/notification-deliveries${s ? `?${s}` : ''}`);
};

registerAll(
  {
    fetchSpikeMarkers, fetchAlerts, acknowledgeAlert,
    fetchMonitors, createMonitor, updateMonitor, deleteMonitor,
    fetchChannels, createChannel, updateChannel, deleteChannel,
    fetchChannelRoutes, saveChannelRoutes, testChannel, fetchDeliveries,
  },
  { replace: true },
);
