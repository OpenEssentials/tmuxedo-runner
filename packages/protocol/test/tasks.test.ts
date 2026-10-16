import { describe, expect, it } from 'vitest';
import { automationLabels, TASK_ID_RE, TASK_METHODS, TASK_WEB_ONLY_METHODS } from '../src/tasks.js';

describe('headless tasks', () => {
  it('reads a machine’s automation labels and drops anything malformed', () => {
    expect(automationLabels(undefined)).toEqual([]);
    expect(automationLabels({})).toEqual([]);
    expect(automationLabels({ automation: { labels: 'ci' } })).toEqual([]);
    expect(automationLabels({ automation: { labels: ['ci', 'ci', 'Bad', '-x', 3, 'linux-arm'] } })).toEqual(['ci', 'linux-arm']);
    expect(automationLabels({ automation: { labels: Array.from({ length: 40 }, (_, i) => `l${i}`) } })).toHaveLength(16);
  });

  it('keeps starting and cancelling off client sockets', () => {
    for (const m of TASK_WEB_ONLY_METHODS) expect(TASK_METHODS).toContain(m);
    expect(TASK_WEB_ONLY_METHODS).not.toContain('task.get');
  });

  it('accepts short plain task ids only', () => {
    expect(TASK_ID_RE.test('ci-123_a')).toBe(true);
    expect(TASK_ID_RE.test('../x')).toBe(false);
    expect(TASK_ID_RE.test('a'.repeat(65))).toBe(false);
  });
});
