import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import {
  FEATURE_REPLAY, REPLAY_MARKERS, REPLAY_MAX_BYTES, REPLAY_METHODS, REPLAY_READ_BYTES_DEFAULT, REPLAY_READ_BYTES_MAX,
  REPLAY_RETENTION_DAYS, REPLAY_SEGMENT_BYTES, REPLAY_SEGMENT_MS, REVIEW_MAX_SESSION_ID, isReplayMethod, parseReplayParams, replayMarkerOf,
} from '../src/index.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/replay.json', import.meta.url), 'utf8'));

describe('replay.* contract (fixtures/replay.json, shared with the runner)', () => {
  it('feature, methods, limits and markers', () => {
    expect(FEATURE_REPLAY).toBe(fx.feature);
    expect([...REPLAY_METHODS]).toEqual(fx.methods);
    for (const m of REPLAY_METHODS) expect(isReplayMethod(m)).toBe(true);
    expect(isReplayMethod('pane.read')).toBe(false);
    expect({
      sessionId: REVIEW_MAX_SESSION_ID, readBytes: REPLAY_READ_BYTES_MAX, readBytesDefault: REPLAY_READ_BYTES_DEFAULT,
      retentionDays: REPLAY_RETENTION_DAYS, maxBytes: REPLAY_MAX_BYTES, segmentBytes: REPLAY_SEGMENT_BYTES, segmentMs: REPLAY_SEGMENT_MS,
    }).toEqual(fx.limits);
    expect([...REPLAY_MARKERS]).toEqual(fx.markers);
    for (const row of fx.markerOf) expect(replayMarkerOf(row.label), row.label).toBe(row.marker);
  });

  it('parameter shapes', () => {
    for (const row of fx.params.valid) expect(parseReplayParams(row.method, row.params).ok, JSON.stringify(row)).toBe(true);
    for (const row of fx.params.invalid) {
      const r = parseReplayParams(row.method, row.params);
      expect(r.ok, JSON.stringify(row)).toBe(false);
      if (!r.ok) expect(r.code, JSON.stringify(row)).toBe(row.code);
    }
  });

  it('defaults', () => {
    const r = parseReplayParams('replay.read', { pane: 'w1:p1' });
    expect(r).toEqual({ ok: true, params: { method: 'replay.read', pane: 'w1:p1', limitBytes: REPLAY_READ_BYTES_DEFAULT, markersOnly: false } });
  });
});
