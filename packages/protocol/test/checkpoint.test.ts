import { describe, expect, it } from 'vitest';
import { CHECKPOINT_SOCKET_METHODS, isCheckpointMethod, parseCheckpointParams } from '../src/checkpoint.js';

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
