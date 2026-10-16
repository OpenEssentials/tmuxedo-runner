import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { HISTORY_METHOD, HISTORY_SOURCE, historyOsc, historyPageParams, parseHistoryOsc, parseHistoryPage } from '../src/history.js';

// The same file the runner checks (runner/crates/emu/tests/history.rs).
const fx = JSON.parse(readFileSync(new URL('../fixtures/history.json', import.meta.url), 'utf8'));

describe('history marks', () => {
  it.each(fx.marks.map((c: any) => [c.name, c]))('%s: written and parsed alike', (_n, c: any) => {
    expect(historyOsc(c.mark)).toBe(c.osc);
    // xterm.js hands an OSC handler the text after "777;" (without ESC ] and BEL).
    const data = c.osc.slice('\x1b]777;'.length, -1);
    expect(parseHistoryOsc(data)).toEqual(c.mark);
  });
  it.each(fx.rejected.map((c: any) => [c.name, c]))('%s is not a mark', (_n, c: any) => {
    expect(parseHistoryOsc(c.data)).toBeNull();
  });
  it('ignores unknown fields (forward compatibility)', () => {
    expect(parseHistoryOsc('tmuxedo-history;start=0;first=1;end=2;limit=3;future=x')).toEqual({ start: 0, first: 1, end: 2, limit: 3 });
  });
});

describe('history pages', () => {
  it.each(fx.pages.map((c: any) => [c.name, c]))('%s', (_n, c: any) => {
    expect(parseHistoryPage(c.result)).toEqual(c.page);
  });
  it('rejects nonsense', () => {
    expect(parseHistoryPage(null)).toBeNull();
    expect(parseHistoryPage({ read: { text: 'x', history: { first: -1, count: 0, start: 0, end: 0, limit: 0, more: false } } })).toBeNull();
    expect(parseHistoryPage({ read: { text: 'x', history: { first: 0, count: 0, start: 0, end: 0, limit: 0, more: 'no' } } })).toBeNull();
  });
  it('asks for an ANSI page ending before a line', () => {
    expect(HISTORY_METHOD).toBe('pane.read');
    expect(historyPageParams('w1:p1', 40, 100)).toEqual({ pane_id: 'w1:p1', source: HISTORY_SOURCE, before: 40, lines: 100, format: 'ansi' });
  });
});
