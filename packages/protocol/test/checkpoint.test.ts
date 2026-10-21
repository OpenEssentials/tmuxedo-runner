import { describe, expect, it } from 'vitest';
import { CHECKPOINT_SOCKET_METHODS, SESSION_SUMMARY, SESSION_SUMMARY_LINE_MAX, isCheckpointMethod, parseCheckpointParams, parseSessionSummary } from '../src/checkpoint.js';

describe('checkpoint params', () => {
  it('name one session and, for preview and restore, a number', () => {
    expect(parseCheckpointParams('checkpoint.list', { pane: 'w1:p1' })).toEqual({ ok: true, target: { pane: 'w1:p1' } });
    expect(parseCheckpointParams('checkpoint.restore', { chat: 'c1', id: 3 })).toEqual({ ok: true, target: { chat: 'c1' }, id: 3 });
    for (const [m, p] of [
      ['checkpoint.list', {}],
      ['checkpoint.list', { pane: 'a', chat: 'b' }],
      ['checkpoint.list', { pane: 'has space' }],
      ['checkpoint.preview', { pane: 'a' }],
      ['checkpoint.restore', { pane: 'a', id: 0 }],
      ['checkpoint.restore', { pane: 'a', id: '1' }],
      ['checkpoint.nope', { pane: 'a' }],
      ['checkpoint.list', []],
    ] as const) {
      expect(parseCheckpointParams(m, p).ok, JSON.stringify([m, p])).toBe(false);
    }
    expect(isCheckpointMethod('checkpoint.x')).toBe(true);
    expect(isCheckpointMethod('git.status')).toBe(false);
    expect(CHECKPOINT_SOCKET_METHODS).not.toContain('checkpoint.restore');
  });
});

describe('session.summary answers', () => {
  it('keep the counts, the line and the turn; anything else is dropped or refused', () => {
    expect(parseSessionSummary({ type: 'session_summary', available: true, turns: 2, from: 1, files: { added: 1, modified: 2, removed: 0 }, line: 'All tests pass.', turn: 't3' }))
      .toEqual({ turns: 2, files: { added: 1, modified: 2, removed: 0 }, line: 'All tests pass.', turn: 't3' });
    expect(parseSessionSummary({ type: 'session_summary', available: false, reason: 'not_a_repo', turns: 0, files: null }))
      .toEqual({ turns: 0, files: null, line: null, turn: null });
    // A line from a runner that does not cut it, and a turn name that is not one.
    const long = parseSessionSummary({ type: 'session_summary', available: true, turns: 0, files: null, line: 'x'.repeat(500), turn: '../t1' });
    expect(long?.line?.length).toBe(SESSION_SUMMARY_LINE_MAX);
    expect(long?.turn).toBeNull();
    for (const bad of [null, [], { type: 'checkpoint_list' }, { type: 'session_summary', turns: -1, files: null }, { type: 'session_summary', turns: 0, files: { added: 'x' } }]) {
      expect(parseSessionSummary(bad), JSON.stringify(bad)).toBeNull();
    }
    expect(SESSION_SUMMARY).toBe('session.summary');
    expect(isCheckpointMethod(SESSION_SUMMARY)).toBe(false);
  });
});

describe('session.summary usage', () => {
  it('carries what the session used of its plan, checked; a bad one is dropped, not the answer', () => {
    const base = { type: 'session_summary', available: true, turns: 1, files: null };
    const usage = { plugin: 'claude', windows: [{ id: '5h', spentPct: 6.5, windowMins: 300 }], tokens: 4000 };
    expect(parseSessionSummary({ ...base, usage })?.usage).toEqual(usage);
    expect(parseSessionSummary({ ...base, usage: { plugin: 'codex', tokens: 12 } })?.usage).toEqual({ plugin: 'codex', windows: [], tokens: 12 });
    expect(parseSessionSummary(base)).not.toHaveProperty('usage');
    for (const bad of [
      { plugin: 'Claude', windows: [] , tokens: 1 },
      { plugin: 'claude', windows: [] },
      { plugin: 'claude', windows: [{ id: '5h', spentPct: 0 }] },
      { plugin: 'claude', windows: [{ id: '5 h', spentPct: 1 }] },
      { plugin: 'claude', windows: [{ id: '5h', spentPct: 1, windowMins: 0 }] },
      { plugin: 'claude', tokens: -1 },
      { plugin: 'claude', tokens: 1.5 },
      'x',
    ]) {
      const s = parseSessionSummary({ ...base, usage: bad });
      expect(s, JSON.stringify(bad)).not.toBeNull();
      expect(s, JSON.stringify(bad)).not.toHaveProperty('usage');
    }
  });
});
