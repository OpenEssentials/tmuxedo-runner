import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { clampStatsHz, parseClientFrame, parseRunnerFrame, STATS_FEATURE, STATS_HZ_MAX, STATS_HZ_MIN, STATS_MAX_BYTES } from '../src/frames.js';
import {
  diskUsedPct, formatBytes, memPct, parseStatsFrame, parseStatsSample, pushStatsPoint, statsCpuLevel, statsDiskLevel, statsMemLevel, statsPoint,
  STATS_HISTORY_MAX, STATS_HISTORY_MS, STATS_MEM_FULL, type StatsPoint, type StatsSample,
} from '../src/stats.js';

// The same file the runner checks (runner/crates/protocol/tests/fixtures.rs `stats_*`).
const fx = JSON.parse(readFileSync(new URL('../fixtures/stats.json', import.meta.url), 'utf8'));
const withoutT = (f: Record<string, unknown>) => { const { t: _t, ...rest } = f; return rest; };

describe('stats fixtures', () => {
  it('agrees on the feature and the rate bounds', () => {
    expect(STATS_FEATURE).toBe(fx.feature);
    expect([STATS_HZ_MIN, STATS_HZ_MAX]).toEqual([fx.hz.min, fx.hz.max]);
  });

  it.each(fx.samples.map((c: any) => [c.name, c]))('sample %s parses to itself, with the listed levels', (_n, c: any) => {
    const text = JSON.stringify(c.frame);
    const p = parseRunnerFrame(text);
    expect(p.ok).toBe(true);
    const s = parseStatsSample(c.frame)!;
    expect(s).toEqual(withoutT(c.frame));
    expect(memPct(s)).toBe(c.memPct);
    expect(diskUsedPct(s)).toBe(c.diskUsedPct);
    expect(statsMemLevel(s)).toBe(c.mem);
    expect(statsDiskLevel(s)).toBe(c.disk);
    expect(text.length).toBeLessThan(400);
  });

  it.each(fx.invalidSamples.map((c: any) => [c.name, c]))('sample %s is refused', (_n, c: any) => {
    expect(parseStatsSample(c.frame)).toBeNull();
    expect(parseStatsFrame(c.frame)).toBeNull();
  });

  it('passes on numbers only: names, paths and lists beside them are dropped', () => {
    const out = parseStatsSample(fx.dropped.in);
    expect(out).toEqual(fx.dropped.out);
    expect(JSON.stringify(out)).not.toMatch(/name|path|command/);
  });

  it.each(fx.client.map((c: any) => [c.name, c]))('client frame %s', (_n, c: any) => {
    const p = parseClientFrame(c.text);
    if (c.frame === null) expect(p).toEqual({ ok: false, reason: 'bad_stats' });
    else expect(p).toEqual({ ok: true, frame: c.frame });
  });

  it('the rates the gateway asks a runner for are in the bounds the runner keeps', () => {
    for (const c of fx.gateway) if (c.hz !== 0) expect(clampStatsHz(c.hz)).toBe(c.hz);
  });
});

describe('parseRunnerFrame stats', () => {
  it('refuses a frame over the size cap and one without a time', () => {
    const big = JSON.stringify({ t: 'stats', at: 1, disks: [], pad: 'x'.repeat(STATS_MAX_BYTES) });
    expect(parseRunnerFrame(big)).toEqual({ ok: false, reason: 'stats_too_large' });
    expect(parseRunnerFrame('{"t":"stats","disks":[]}')).toEqual({ ok: false, reason: 'bad_stats' });
  });
  it('never throws on junk', () => {
    for (const v of [null, 1, 'x', [], {}, { at: 'now' }, { at: 1, cpu: null }, { at: 1, mem: [] }, { at: 1, disks: [null] }, { at: 1, disks: [[]] }]) {
      expect(parseStatsSample(v)).toBeNull();
    }
    expect(parseStatsSample({ at: 1 })).toEqual({ at: 1, disks: [] });
  });
});

describe('clampStatsHz', () => {
  it('keeps the rate between the bounds; missing or junk is the fastest', () => {
    expect(clampStatsHz(0.25)).toBe(0.25);
    expect(clampStatsHz(99)).toBe(STATS_HZ_MAX);
    expect(clampStatsHz(0)).toBe(STATS_HZ_MIN);
    expect(clampStatsHz(undefined)).toBe(STATS_HZ_MAX);
    expect(clampStatsHz(Number.NaN)).toBe(STATS_HZ_MAX);
  });
});

describe('history', () => {
  const sample = (at: number, cpu?: number, used = 1): StatsSample => ({ at, ...(cpu === undefined ? {} : { cpu: { pct: cpu } }), mem: { used, limit: 4 }, disks: [] });

  it('a point is [at, cpu, memory percent], null where not reported', () => {
    expect(statsPoint(sample(5, 12.5))).toEqual([5, 12.5, 25]);
    expect(statsPoint({ at: 6, disks: [] })).toEqual([6, null, null]);
  });

  it('is bounded in count and in time, and ordered', () => {
    let h: StatsPoint[] = [];
    for (let i = 0; i < 1000; i++) h = pushStatsPoint(h, [i * 1000, i % 100, 50]);
    expect(h.length).toBeLessThanOrEqual(STATS_HISTORY_MAX);
    expect(h[h.length - 1]![0]).toBe(999_000);
    expect(h[0]![0]).toBeGreaterThanOrEqual(999_000 - STATS_HISTORY_MS);
    // Fast points: the count bound holds too.
    for (let i = 0; i < 1000; i++) h = pushStatsPoint(h, [1_000_000 + i, 1, 1]);
    expect(h).toHaveLength(STATS_HISTORY_MAX);
    // A point that is not newer is ignored.
    expect(pushStatsPoint(h, [5, 1, 1])).toBe(h);
  });

  it('a client frame keeps a checked history and drops junk points', () => {
    const f = parseStatsFrame({ t: 'stats', machineId: 'm1', at: 9, disks: [], history: [[1, 5, 50], 'x', [2, 500, null], [3]] })!;
    expect(f).toEqual({ t: 'stats', machineId: 'm1', at: 9, disks: [], history: [[1, 5, 50], [2, null, null], [3, null, null]] });
    const many = parseStatsFrame({ at: 9, disks: [], history: Array.from({ length: 5000 }, (_, i) => [i, 1, 1]) })!;
    expect(many.history).toHaveLength(STATS_HISTORY_MAX);
  });
});

describe('levels', () => {
  it('memory is full exactly at the watchdog threshold', () => {
    expect(STATS_MEM_FULL).toBe(90);
    expect(statsMemLevel({ mem: { used: 899, limit: 1000 } })).toBe('high');
    expect(statsMemLevel({ mem: { used: 900, limit: 1000 } })).toBe('full');
    expect(statsMemLevel({ mem: { used: 799, limit: 1000 } })).toBe('ok');
    expect(statsMemLevel({ mem: { used: 5000, limit: 1000 } })).toBe('full');
  });

  it('CPU is high only after 30 s at or above 85 percent', () => {
    const at = 100_000;
    const busy = (from: number): StatsPoint[] => Array.from({ length: (at - from) / 2000 + 1 }, (_, i) => [from + i * 2000, 97, 10] as StatsPoint);
    const now = { at, cpu: { pct: 97 } };
    expect(statsCpuLevel(now, [])).toBe('ok');
    expect(statsCpuLevel(now, busy(at - 10_000))).toBe('ok');
    expect(statsCpuLevel(now, busy(at - 30_000))).toBe('high');
    expect(statsCpuLevel(now, busy(at - 60_000))).toBe('high');
    const dipped = busy(at - 30_000);
    dipped[5] = [dipped[5]![0], 20, 10];
    expect(statsCpuLevel(now, dipped)).toBe('ok');
    expect(statsCpuLevel({ at, cpu: { pct: 50 } }, busy(at - 30_000))).toBe('ok');
    expect(statsCpuLevel({ at, cpu: { cores: 4 } }, [])).toBe('none');
    expect(statsCpuLevel({ at }, [])).toBe('none');
  });
});

describe('formatBytes', () => {
  it('reads as people write sizes', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(512 * 1024 * 1024)).toBe('512 MiB');
    expect(formatBytes(2.4 * 1024 ** 3)).toBe('2.4 GiB');
    expect(formatBytes(48 * 1024 ** 3)).toBe('48 GiB');
    expect(formatBytes(-5)).toBe('0 B');
  });
});
