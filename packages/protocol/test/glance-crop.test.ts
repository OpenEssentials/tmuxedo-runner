/**
 * The per-client crop of a glance (src/glance.ts `cropGlance`; docs/protocol.md
 * "Crop and rate per socket"): the bottom rows with content and the left
 * columns, cut on cell boundaries, and the `rows`/`cols` a client may ask.
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { cropGlance, decodeGlance, glanceText, runColumns, GLANCE_CROP_MAX_MARKS, GLANCE_MAX_COLS, GLANCE_MAX_ROWS, type GlanceWireRun } from '../src/glance';
import { GLANCE_CROP_MAX_COLS, GLANCE_CROP_MAX_ROWS, parseClientFrame, parseGlanceCrop, parseRunnerFrame } from '../src/frames';

const fx = JSON.parse(readFileSync(new URL('../fixtures/glance.json', import.meta.url), 'utf8')) as {
  cases: Array<{ name: string; frame: { cols: number; rows: number; cursor?: { x: number; y: number }; lines: unknown[][] }; text: string[] }>;
};

function screen(cols: number, rows: number, lines: (string | GlanceWireRun[])[], cursor?: { x: number; y: number }) {
  const g = decodeGlance({
    t: 'glance', machineId: 'm1', paneId: 'w1:p1', rev: 4, cols, rows, ...(cursor ? { cursor } : {}),
    lines: lines.map((l) => (typeof l === 'string' ? (l ? [[l]] : []) : l)),
  });
  if (!g) throw new Error('bad test screen');
  return g;
}
/** Crop, and read the result back the way a client does. */
function crop(g: ReturnType<typeof screen>, rows: number, cols: number) {
  const frame = cropGlance(g, { rows, cols });
  const back = decodeGlance(JSON.parse(JSON.stringify(frame)));
  if (!back) throw new Error('the crop is not a glance');
  expect(parseRunnerFrame(JSON.stringify(frame)).ok).toBe(true); // the shape every glance has
  return { frame, back, text: glanceText(back) };
}
const width = (line: Parameters<typeof runColumns>[0]) => runColumns(line).reduce((n, r) => n + r.width, 0);

describe('cropGlance', () => {
  it('a wide screen: the bottom rows, the left columns, the header kept', () => {
    const lines = Array.from({ length: 40 }, (_, i) => `${String(i).padStart(2, '0')} ${'x'.repeat(i % 2 ? 100 : 10)}`);
    const c = crop(screen(120, 40, lines, { x: 13, y: 38 }), 10, 38);
    expect(c.frame).toMatchObject({ t: 'glance', machineId: 'm1', paneId: 'w1:p1', rev: 4, cols: 38, rows: 10, cursor: { x: 13, y: 8 } });
    expect(c.text[0]).toBe(`30 ${'x'.repeat(10)}`);
    expect(c.text[9]).toBe(`39 ${'x'.repeat(35)}`);
    expect(c.back.lines.every((l) => width(l) <= 38)).toBe(true);
  });

  it('a short or narrow pane is neither padded nor cut, and says its own width', () => {
    const c = crop(screen(20, 3, ['$ ls', 'a  b', '$ '], { x: 2, y: 2 }), 10, 38);
    expect(c.frame).toMatchObject({ cols: 20, rows: 3, cursor: { x: 2, y: 2 } });
    expect(c.text).toEqual(['$ ls', 'a  b', '$ ']);
  });

  it('trailing blank rows go first; blank rows inside stay; the cursor row counts as content', () => {
    expect(crop(screen(80, 24, ['$ ls', '', 'b', ...Array.from({ length: 21 }, () => '')]), 10, 38).text).toEqual(['$ ls', '', 'b']);
    const cleared = crop(screen(80, 24, Array.from({ length: 24 }, () => ''), { x: 0, y: 5 }), 3, 38);
    expect(cleared.frame).toMatchObject({ rows: 3, cursor: { x: 0, y: 2 } });
    expect(cleared.text).toEqual(['', '', '']);
    // A row of spaces on a background, or inverse, is content; plain spaces are not.
    expect(crop(screen(80, 3, ['top', [['   ', null, 4]], [['  ']]]), 10, 38).text).toEqual(['top', '   ']);
    expect(crop(screen(80, 3, ['top', [['  ', null, null, 16]], '']), 10, 38).text).toEqual(['top', '  ']);
  });

  it('an empty pane: one empty row, no cursor, still a valid glance', () => {
    const c = crop(screen(80, 24, []), 10, 38);
    expect(c.frame).toEqual({ t: 'glance', machineId: 'm1', paneId: 'w1:p1', rev: 4, cols: 38, rows: 1, lines: [[]] });
  });

  it('a wide character is never split: one that would straddle the edge is left out whole and ends the row', () => {
    expect(crop(screen(80, 1, ['abc日本語z']), 1, 4).text).toEqual(['abc']); // 日 needs columns 3 and 4
    expect(crop(screen(80, 1, ['abc日本語z']), 1, 5).text).toEqual(['abc日']);
    expect(crop(screen(80, 1, ['abc日本語z']), 1, 6).text).toEqual(['abc日']); // 本 does not fit, and the narrow z after it is not pulled in
    expect(crop(screen(80, 1, ['😀😀😀']), 1, 3).text).toEqual(['😀']); // an astral character is one piece, never half a pair
    expect(crop(screen(80, 1, ['😀']), 1, 1).text).toEqual(['']);
    for (let cols = 1; cols <= 12; cols++) {
      const c = crop(screen(80, 1, ['a日b本c語d😀e']), 1, cols);
      expect(width(c.back.lines[0]!)).toBeLessThanOrEqual(cols);
      expect('a日b本c語d😀e'.startsWith(c.text[0]!)).toBe(true);
      expect(c.text[0]).not.toMatch(/[\ud800-\udbff]$/); // no lone high surrogate at the cut
    }
  });

  it('a combining mark stays with its character, also at the edge; marks beyond the cap are dropped', () => {
    expect(crop(screen(80, 1, ['ééé']), 1, 2).text).toEqual(['éé']);
    // The mark is in the next run: it still follows its character over the edge.
    expect(crop(screen(80, 1, [[['ab'], ['́c', 1]]]), 1, 2).text).toEqual(['ab́']);
    const zalgo = `a${'́'.repeat(5000)}b`;
    const c = crop(screen(80, 1, [zalgo]), 1, 10);
    expect(c.text).toEqual([`a${'́'.repeat(GLANCE_CROP_MAX_MARKS)}b`]);
  });

  it('colours and attributes are kept as sent; a run cut in the middle keeps its style', () => {
    const c = cropGlance(screen(80, 1, [[['ERR', 1, null, 1], [' ok '], ['rgb', '#102030'], ['    ', null, 4]]]), { rows: 1, cols: 9 });
    expect(c.lines).toEqual([[['ERR', 1, null, 1], [' ok '], ['rg', '#102030']]]);
  });

  it('control characters never pass', () => {
    const c = crop(screen(80, 1, ['a\u001b[31mb\u0007c\u009bd\u007fe\u0000']), 1, 38);
    expect(c.text).toEqual(['a[31mbcde']);
  });

  it('the cursor: moved to its row in the crop, left out above the rows; right of the columns it keeps its own x', () => {
    const lines = Array.from({ length: 20 }, (_, i) => `row ${i}`);
    expect(cropGlance(screen(80, 20, lines, { x: 37, y: 19 }), { rows: 10, cols: 38 }).cursor).toEqual({ x: 37, y: 9 });
    expect(cropGlance(screen(80, 20, lines, { x: 70, y: 19 }), { rows: 10, cols: 38 }).cursor).toEqual({ x: 70, y: 9 });
    expect(cropGlance(screen(80, 20, lines, { x: 0, y: 9 }), { rows: 10, cols: 38 }).cursor).toBeUndefined();
    expect(cropGlance(screen(80, 20, lines, { x: 0, y: 10 }), { rows: 10, cols: 38 }).cursor).toEqual({ x: 0, y: 0 });
    expect(cropGlance(screen(80, 20, lines, { x: 0, y: 500 }), { rows: 10, cols: 38 }).cursor).toBeUndefined(); // a cursor below the screen is no row
  });

  it('hostile sizes are clamped, never trusted', () => {
    const big = screen(200, 60, Array.from({ length: 60 }, () => 'w'.repeat(200)));
    for (const [rows, cols] of [[0, 0], [-5, -5], [1e9, 1e9], [NaN, NaN], [Infinity, Infinity], [2.7, 3.9]] as const) {
      const f = cropGlance(big, { rows, cols });
      expect(f.rows).toBeGreaterThanOrEqual(1);
      expect(f.rows).toBeLessThanOrEqual(GLANCE_MAX_ROWS);
      expect(f.cols).toBeGreaterThanOrEqual(1);
      expect(f.cols).toBeLessThanOrEqual(GLANCE_MAX_COLS);
      expect(f.lines).toHaveLength(f.rows);
      for (const l of decodeGlance(f)!.lines) expect(width(l)).toBeLessThanOrEqual(f.cols);
    }
    expect(cropGlance(big, { rows: 2.7, cols: 3.9 })).toMatchObject({ rows: 2, cols: 3 });
  });

  it('a 10 by 38 crop of a full 200 by 60 screen is small', () => {
    const busy = screen(200, 60, Array.from({ length: 60 }, (_, i) => [[`${i} `, 2], ['x'.repeat(190), null, null, 1]] as GlanceWireRun[]));
    expect(JSON.stringify(cropGlance(busy, { rows: 10, cols: 38 })).length).toBeLessThan(800);
  });

  it.each(fx.cases.map((c) => [c.name, c] as const))('golden %s: each row is the left of one of the bottom rows', (_n, c) => {
    const g = decodeGlance({ t: 'glance', paneId: 'w1:p1', rev: 1, ...c.frame })!;
    const out = crop(g, 10, 38);
    let last = c.text.length - 1;
    while (last >= 0 && !g.lines[last]!.some((r) => r.text.trim() !== '' || (r.text !== '' && (r.bg !== null || (r.attrs & 16) !== 0)))) last--;
    last = Math.max(last, g.cursor && g.cursor.y < g.lines.length ? g.cursor.y : -1);
    const want = last < 0 ? [''] : c.text.slice(Math.max(0, last - 9), last + 1);
    expect(out.text).toHaveLength(want.length);
    out.text.forEach((row, i) => {
      expect(want[i]!.startsWith(row)).toBe(true);
      expect(width(out.back.lines[i]!)).toBeLessThanOrEqual(38);
    });
    // Asking for everything changes nothing a client reads (but for dropped trailing blank rows).
    const all = crop(g, GLANCE_MAX_ROWS, GLANCE_MAX_COLS);
    expect(all.text).toEqual(last < 0 ? [''] : c.text.slice(0, last + 1));
  });
});

describe('rows and cols on the client glance frame', () => {
  const set = (extra: string) => parseClientFrame(`{"t":"glance","panes":[{"machineId":"m1","paneId":"w1:p1"}],"hz":0.2${extra}}`);

  it('the limits are the runner\'s own', () => {
    expect([GLANCE_CROP_MAX_ROWS, GLANCE_CROP_MAX_COLS]).toEqual([GLANCE_MAX_ROWS, GLANCE_MAX_COLS]);
  });

  it('neither: no crop, the frame as before', () => {
    expect(set('')).toEqual({ ok: true, frame: { t: 'glance', panes: [{ machineId: 'm1', paneId: 'w1:p1' }], hz: 0.2 } });
    expect(parseGlanceCrop(undefined, undefined)).toBeNull();
  });

  it('both: kept, and clamped to the maximum', () => {
    expect(set(',"rows":10,"cols":38')).toMatchObject({ ok: true, frame: { rows: 10, cols: 38 } });
    expect(set(',"rows":1000000000,"cols":9007199254740991')).toMatchObject({ ok: true, frame: { rows: 60, cols: 200 } });
  });

  it.each([
    ',"rows":10', ',"cols":38', ',"rows":0,"cols":38', ',"rows":10,"cols":0', ',"rows":-1,"cols":38', ',"rows":1.5,"cols":38',
    ',"rows":"10","cols":38', ',"rows":null,"cols":38', ',"rows":10,"cols":[38]', ',"rows":1e400,"cols":38', ',"rows":{},"cols":{}',
    ',"rows":9007199254740993,"cols":38',
  ])('junk %s is refused', (extra) => {
    expect(set(extra)).toEqual({ ok: false, reason: 'bad_glance' });
  });
});
