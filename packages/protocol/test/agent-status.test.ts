import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { parseRunnerFrame } from '../src/frames.js';
import { AGENT_STATUS_TEXT } from '../src/agents.js';

// The same file the runner checks (runner/crates/protocol/tests/fixtures.rs).
const fx = JSON.parse(readFileSync(new URL('../fixtures/agent-status.json', import.meta.url), 'utf8')) as {
  statuses: string[]; sources: string[];
  frames: { name: string; text: string; frame: unknown }[];
  invalid: { name: string; text: string }[];
};

describe('agent status fixtures', () => {
  it('cover every status from every source', () => {
    expect(fx.statuses).toEqual(['working', 'blocked', 'done', 'idle', 'unknown']);
    expect(fx.sources).toEqual(['reported', 'screen', 'heuristic']);
    for (const source of fx.sources) {
      for (const status of fx.statuses) {
        expect(fx.frames.some((c) => c.name === `${status}-${source}`), `${status}-${source}`).toBe(true);
      }
    }
    expect(fx.frames.some((c) => (c.frame as { agent: unknown }).agent === null)).toBe(true);
  });

  it('parse to the listed frame', () => {
    for (const c of fx.frames) {
      const r = parseRunnerFrame(c.text);
      expect(r.ok, c.name).toBe(true);
      if (r.ok) expect(r.frame, c.name).toEqual(c.frame);
    }
  });

  it('every status has words for people', () => {
    for (const s of fx.statuses) expect(AGENT_STATUS_TEXT[s as keyof typeof AGENT_STATUS_TEXT], s).toBeTruthy();
  });

  it('refuse malformed status frames', () => {
    for (const c of fx.invalid) expect(parseRunnerFrame(c.text).ok, c.name).toBe(false);
  });
});
