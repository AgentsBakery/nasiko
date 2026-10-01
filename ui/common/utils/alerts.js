/**
 * Pure alert view helpers: mapping spike-alert markers onto the TokenOps
 * spend chart. No DOM and no fetch so they run under node:test. Marker titles
 * come from the API and are untrusted: the chart renders them as text, and any
 * HTML interpolation by a caller must go through escHtml.
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
