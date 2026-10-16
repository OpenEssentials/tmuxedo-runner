import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { cleanName, defaultName, NAME_MAX, paneDisplayName, RENAME_ID_PARAM, RENAME_METHOD, storedName } from '../src/names.js';

// The same file the runner checks (runner/crates/protocol/tests/fixtures.rs `names_*`).
const fx = JSON.parse(readFileSync(new URL('../fixtures/names.json', import.meta.url), 'utf8'));

describe('names fixtures', () => {
  it('agrees on the limit', () => expect(NAME_MAX).toBe(fx.max));
  it.each(fx.clean.map((c: any) => [c.name, c]))('clean %s', (_n, c: any) => {
    expect(cleanName(c.in)).toBe(c.out);
  });
  it.each(fx.defaults.map((c: any) => [`${c.kind} ${c.id}`, c]))('default %s', (_n, c: any) => {
    expect(defaultName(c.kind, c.id)).toBe(c.name);
  });
});

describe('cleanName', () => {
  it('is idempotent on every fixture', () => {
    for (const c of fx.clean) expect(cleanName(cleanName(c.in))).toBe(c.out);
  });
  it('never returns more than NAME_MAX code points, a control character or an outer space', () => {
    const junk = ['\u0001', ' ', '‮', 'x', '😀', '\n', ' ', 'é'];
    for (let seed = 1; seed < 400; seed++) {
      let s = '';
      let r = seed;
      for (let i = 0; i < (seed % 90) + 1; i++) { r = (r * 1103515245 + 12345) % 2147483648; s += junk[r % junk.length]; }
      const out = cleanName(s);
      expect([...out].length).toBeLessThanOrEqual(NAME_MAX);
      expect(out).toBe(out.trim());
      expect(out).not.toMatch(/[\u0000-\u001f\u007f-\u009f‪-‮⁦-⁩​﻿]|\s\s/);
    }
  });
  it('treats anything but a string as empty', () => {
    for (const v of [null, undefined, 7, {}, [], true]) expect(cleanName(v)).toBe('');
  });
});

describe('storedName and defaults', () => {
  it('falls back to the default name when the clean name is empty', () => {
    expect(storedName('workspace', 'w1', '   ')).toBe('default');
    expect(storedName('workspace', 'w4', null)).toBe('4');
    expect(storedName('tab', 'w4:t3', '\u0007')).toBe('3');
    expect(storedName('pane', 'w4:p3', '')).toBeNull();
    expect(storedName('tab', 'w4:t3', '  logs ')).toBe('logs');
  });
  it('keeps an id it cannot parse rather than inventing one', () => {
    expect(defaultName('workspace', 'odd')).toBe('odd');
    expect(defaultName('tab', 'odd')).toBe('odd');
  });
  it('names the method and id parameter per kind', () => {
    expect(RENAME_METHOD).toEqual({ workspace: 'workspace.rename', tab: 'tab.rename', pane: 'pane.rename' });
    expect(RENAME_ID_PARAM).toEqual({ workspace: 'workspace_id', tab: 'tab_id', pane: 'pane_id' });
  });
});

describe('paneDisplayName', () => {
  it('prefers the label, then the title, then the id', () => {
    expect(paneDisplayName({ id: 'w1:p1', label: 'build', title: 'bash' })).toBe('build');
    expect(paneDisplayName({ id: 'w1:p1', label: null, title: 'bash' })).toBe('bash');
    expect(paneDisplayName({ paneId: 'w1:p2', label: '', title: '' })).toBe('w1:p2');
  });
});
