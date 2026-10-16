import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { charWidth, decodeGlance, encodeRun, glanceLastLine, glanceText, runColumns, GLANCE_MAX_COLS, GLANCE_MAX_ROWS } from '../src/glance';
import { clampHz, GLANCE_MAX_BYTES, hasFeature, parseClientFrame, parseRunnerFrame } from '../src/frames';

const fx = JSON.parse(readFileSync(new URL('../fixtures/glance.json', import.meta.url), 'utf8')) as {
  cases: Array<{ name: string; cols: number; rows: number; input: string; frame: { cols: number; rows: number; cursor?: { x: number; y: number }; lines: unknown[][] }; text: string[] }>;
};

describe('glance golden fixtures (the runner encodes the same cases: runner/crates/runner/src/glance.rs)', () => {
  it('has the cases', () => expect(fx.cases.length).toBeGreaterThanOrEqual(7));

  it.each(fx.cases.map((c) => [c.name, c]))('%s decodes to its text', (_n, c) => {
    const g = decodeGlance({ t: 'glance', paneId: 'w1:p1', rev: 1, ...c.frame });
    expect(g).not.toBeNull();
    expect(glanceText(g!)).toEqual(c.text);
    expect(g!.cursor).toEqual(c.frame.cursor ?? null);
    expect(g!.rows).toBe(c.frame.lines.length);
    expect(g!.cols).toBeLessThanOrEqual(GLANCE_MAX_COLS);
    expect(g!.rows).toBeLessThanOrEqual(GLANCE_MAX_ROWS);
    // Re-encoding the decoded runs gives the wire runs back (trailing defaults left out).
    expect(g!.lines.map((l) => l.map(encodeRun))).toEqual(c.frame.lines);
    // No row is wider than the frame says.
    for (const l of g!.lines) {
      const cols = runColumns(l);
      const end = cols.length ? cols[cols.length - 1]!.col + cols[cols.length - 1]!.width : 0;
      expect(end).toBeLessThanOrEqual(g!.cols);
    }
  });

  it('decodes styles: palette, true colour, attrs', () => {
    const c = fx.cases.find((x) => x.name === 'sgr-colours')!;
    const g = decodeGlance({ t: 'glance', paneId: 'p', rev: 3, ...c.frame })!;
    expect(g.lines[0]).toEqual([
      { text: 'ERR', fg: 1, bg: null, attrs: 1 },
      { text: ' ok ', fg: null, bg: null, attrs: 0 },
      { text: 'rgb', fg: '#102030', bg: null, attrs: 0 },
      { text: '  ', fg: null, bg: 4, attrs: 0 },
    ]);
  });

  it('places wide characters on two columns', () => {
    const c = fx.cases.find((x) => x.name === 'wide-and-inverse')!;
    const g = decodeGlance({ t: 'glance', paneId: 'p', rev: 1, ...c.frame })!;
    expect(runColumns(g.lines[0]!).map((r) => [r.run.text, r.col, r.width])).toEqual([['日本', 0, 4], ['X', 4, 1]]);
    expect(charWidth('a'.codePointAt(0)!)).toBe(1);
    expect(charWidth('😀'.codePointAt(0)!)).toBe(2);
    expect(charWidth(0x301)).toBe(0);
  });

  it('last line for labels and the old-runner fallback', () => {
    const c = fx.cases.find((x) => x.name === 'crop')!;
    expect(glanceLastLine(decodeGlance({ t: 'glance', paneId: 'p', rev: 1, ...c.frame })!)).toBe('bottom');
    expect(glanceLastLine({ lines: [[], []] })).toBe('');
  });
});

describe('glance decoding never throws', () => {
  it.each([
    null, 1, 'x', {}, { paneId: '', rev: 1, cols: 1, rows: 1, lines: [] }, { paneId: 'p', rev: -1, cols: 1, rows: 1, lines: [] },
    { paneId: 'p', rev: 1, cols: 0, rows: 1, lines: [] }, { paneId: 'p', rev: 1, cols: 1, rows: 1, lines: 'no' },
  ])('junk %j → null', (f) => expect(decodeGlance(f)).toBeNull());

  it('drops bad runs, pads missing rows, ignores extra rows', () => {
    const g = decodeGlance({ paneId: 'p', rev: 1, cols: 5, rows: 3, cursor: { x: -1, y: 0 }, lines: [[['ok'], [5], ['c', 'red'], ['d', 999], ['e', null, null, 64]], 'row', [['x']], [['extra']]] })!;
    expect(g.lines).toEqual([[{ text: 'ok', fg: null, bg: null, attrs: 0 }], [], [{ text: 'x', fg: null, bg: null, attrs: 0 }]]);
    expect(g.cursor).toBeNull();
    expect(decodeGlance({ paneId: 'p', rev: 1, cols: 5, rows: 2, lines: [] })!.lines).toEqual([[], []]);
  });
});

describe('glance frames in the relay parsers', () => {
  it('client glance: targets, hz clamped, limits', () => {
    expect(parseClientFrame(JSON.stringify({ t: 'glance', panes: [{ machineId: 'm1', paneId: 'w1:p1' }, { paneId: 'w1:p2' }], hz: 5 }))).toEqual({
      ok: true, frame: { t: 'glance', panes: [{ machineId: 'm1', paneId: 'w1:p1' }, { paneId: 'w1:p2' }], hz: 1 },
    });
    expect(parseClientFrame('{"t":"glance","panes":[]}')).toEqual({ ok: true, frame: { t: 'glance', panes: [], hz: 0.5 } });
    expect(parseClientFrame('{"t":"glance","panes":[],"hz":0.01}')).toMatchObject({ ok: true, frame: { hz: 0.1 } });
    expect(parseClientFrame('{"t":"glance"}')).toEqual({ ok: false, reason: 'bad_glance' });
    expect(parseClientFrame('{"t":"glance","panes":[{"paneId":""}]}')).toEqual({ ok: false, reason: 'bad_glance' });
    expect(parseClientFrame(JSON.stringify({ t: 'glance', panes: Array.from({ length: 65 }, (_, i) => ({ paneId: `p${i}` })) }))).toEqual({ ok: false, reason: 'bad_glance' });
    expect(clampHz('fast')).toBe(0.5);
  });

  it('runner glance: shape checked, size capped', () => {
    const ok = { t: 'glance', paneId: 'w1:p1', rev: 2, cols: 80, rows: 24, lines: [[['hi']]] };
    expect(parseRunnerFrame(JSON.stringify(ok))).toEqual({ ok: true, frame: ok });
    expect(parseRunnerFrame(JSON.stringify({ ...ok, lines: 'x' }))).toEqual({ ok: false, reason: 'bad_glance' });
    expect(parseRunnerFrame(JSON.stringify({ ...ok, rev: -1 }))).toEqual({ ok: false, reason: 'bad_glance' });
    expect(parseRunnerFrame(JSON.stringify({ ...ok, lines: [[['x'.repeat(GLANCE_MAX_BYTES)]]] }))).toEqual({ ok: false, reason: 'glance_too_large' });
  });

  it('hello features', () => {
    expect(hasFeature({ features: ['glance'] }, 'glance')).toBe(true);
    expect(hasFeature({}, 'glance')).toBe(false);
    expect(hasFeature(null, 'glance')).toBe(false);
    expect(hasFeature({ features: 'glance' }, 'glance')).toBe(false);
  });
});
