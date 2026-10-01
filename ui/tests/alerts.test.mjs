/**
 * Tests for the pure alert view helpers (chart marker mapping). No DOM
 * involved. Bucket keys are UTC ISO-string prefixes, so results never depend
 * on the machine's local time zone.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';

const A = await import('../common/utils/alerts.js');

const hourPoints = [
  { bucket_start: '2026-10-01T10:00:00.000Z' },
  { bucket_start: '2026-10-01T11:00:00.000Z' },
];

test('markerAnomalies maps an hour marker to its hour bucket index', () => {
  const markers = [{ hour_start: '2026-10-01T11:00:00Z', title: 'Spike on agent-a' }];
  assert.deepEqual(A.markerAnomalies(hourPoints, 'hour', markers), [
    { index: 1, note: 'Spike on agent-a' },
  ]);
});

test('markerAnomalies collapses same-day markers and joins notes', () => {
  const points = [
    { bucket_start: '2026-09-30T00:00:00.000Z' },
    { bucket_start: '2026-10-01T00:00:00.000Z' },
  ];
  const markers = [
    { hour_start: '2026-10-01T03:00:00Z', title: 'one' },
    { hour_start: '2026-10-01T09:00:00Z', title: 'two' },
  ];
  assert.deepEqual(A.markerAnomalies(points, 'day', markers), [{ index: 1, note: 'one; two' }]);
});

test('markerAnomalies drops markers with no matching point', () => {
  const points = [{ bucket_start: '2026-09-28T00:00:00.000Z' }, { bucket_start: '2026-10-01T00:00:00.000Z' }];
  const markers = [{ hour_start: '2026-09-30T05:00:00Z', title: 'gap day' }];
  assert.deepEqual(A.markerAnomalies(points, 'day', markers), []);
});

test('markerAnomalies keys on the UTC day, not local time', () => {
  const points = [
    { bucket_start: '2026-10-01T00:00:00.000Z' },
    { bucket_start: '2026-10-02T00:00:00.000Z' },
  ];
  const markers = [{ hour_start: '2026-10-01T23:30:00Z', title: 'late' }];
  assert.deepEqual(A.markerAnomalies(points, 'day', markers), [{ index: 0, note: 'late' }]);
});

test('markerAnomalies tolerates empty and non-array input', () => {
  assert.deepEqual(A.markerAnomalies([], 'hour', []), []);
  assert.deepEqual(A.markerAnomalies(null, 'hour', [{ hour_start: 'x', title: 't' }]), []);
  assert.deepEqual(A.markerAnomalies(hourPoints, 'hour', undefined), []);
  assert.deepEqual(A.markerAnomalies(hourPoints, 'hour', [{ title: 'no time' }]), []);
});
