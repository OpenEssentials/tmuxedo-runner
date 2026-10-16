import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { parseRunnerFrame } from '../src/frames.js';
import { parseUsage, parseUsageFrame, usageKey, USAGE_FEATURE, USAGE_MAX_BYTES, USAGE_MIN_INTERVAL_MS, USAGE_WINDOWS_MAX } from '../src/usage.js';

// The same file the runner checks (runner/crates/protocol/tests/fixtures.rs `usage_*`).
const fx = JSON.parse(readFileSync(new URL('../fixtures/usage.json', import.meta.url), 'utf8'));
const withoutT = (f: Record<string, unknown>) => { const { t: _t, ...rest } = f; return rest; };

describe('usage fixtures', () => {
  it('agrees on the feature and the bounds', () => {
    expect(USAGE_FEATURE).toBe(fx.feature);
    expect(USAGE_MIN_INTERVAL_MS).toBe(fx.minIntervalMs);
    expect(USAGE_WINDOWS_MAX).toBe(fx.windowsMax);
  });

  it.each(fx.records.map((c: any) => [c.name, c]))('record %s parses to itself', (_n, c: any) => {
    const text = JSON.stringify(c.frame);
    expect(parseRunnerFrame(text).ok).toBe(true);
    expect(parseUsage(c.frame)).toEqual(withoutT(c.frame));
    expect(text.length).toBeLessThan(500);
  });

  it.each(fx.invalid.map((c: any) => [c.name, c]))('%s is refused', (_n, c: any) => {
    expect(parseUsage(c.frame)).toBeNull();
  });

  it('drops what a record does not have', () => {
    expect(parseUsage(fx.stripped.frame)).toEqual(fx.stripped.clean);
  });
});

describe('usage frames', () => {
  const ok = fx.records[0].frame;

  it('refuses a runner frame that is too large or has no account', () => {
    expect(parseRunnerFrame(JSON.stringify({ ...ok, pad: 'x'.repeat(USAGE_MAX_BYTES) }))).toEqual({ ok: false, reason: 'usage_too_large' });
    expect(parseRunnerFrame(JSON.stringify({ t: 'usage', plugin: 'claude' }))).toEqual({ ok: false, reason: 'bad_usage' });
  });

  it('never throws on junk', () => {
    for (const v of [null, 1, 'x', [], {}, { plugin: 1 }, { ...ok, windows: 'no' }, { ...ok, windows: [null] }, { ...ok, context: 3 }, { ...ok, observedAt: -1 }, { ...ok, observedAt: 1.5 }]) {
      expect(parseUsage(v)).toBeNull();
    }
  });

  it('refuses fractions and junk where whole numbers belong', () => {
    const w = (extra: object) => parseUsage({ ...ok, windows: [{ id: '5h', usedPct: 1, ...extra }] });
    expect(w({ resetsAt: 1.5 })).toBeNull();
    expect(w({ resetsAt: 0 })).toBeNull();
    expect(w({ windowMins: 0 })).toBeNull();
    expect(w({ usedPct: Number.NaN })).toBeNull();
    expect(w({ usedPct: -1 })).toBeNull();
    expect(parseUsage({ ...ok, plan: 'Max Plan' })).toBeNull();
    expect(parseUsage({ ...ok, reached: 'a b' })).toBeNull();
  });

  it('a client frame needs a machine and a sound record', () => {
    expect(parseUsageFrame({ ...ok, machineId: 'm1' })).toEqual({ ...ok, machineId: 'm1' });
    expect(parseUsageFrame(ok)).toBeNull();
    expect(parseUsageFrame({ ...ok, machineId: 'm1', accountKey: 'x' })).toBeNull();
  });

  it('files a record under its plugin and account', () => {
    expect(usageKey(fx.records[0].frame)).toBe('claude\nm:0123456789abcdef');
    expect(usageKey(fx.records[0].frame)).not.toBe(usageKey(fx.records[1].frame));
  });
});
