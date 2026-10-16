import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { parseRunnerFrame } from '../src/frames.js';
import {
  MAX_AGENT_CHILDREN, SUBAGENT_COMPLETED, SUBAGENT_STARTED, helpersText, helpersWorking, parseAgentChildren,
} from '../src/agents.js';

// The same file the runner checks (runner/crates/protocol/tests/subagents.rs).
const fx = JSON.parse(readFileSync(new URL('../fixtures/subagents.json', import.meta.url), 'utf8')) as {
  maxChildren: number; eventTypes: string[];
  frames: { name: string; text: string; frame: { agent: { children?: unknown } } }[];
  events: { name: string; text: string; frame: { type: string } }[];
  lenient: { name: string; input: unknown; want: unknown }[];
};

describe('helpers (Agent.children)', () => {
  it('constants match the fixture', () => {
    expect(MAX_AGENT_CHILDREN).toBe(fx.maxChildren);
    expect([SUBAGENT_STARTED, SUBAGENT_COMPLETED]).toEqual(fx.eventTypes);
  });

  it('status frames with children parse to the listed frame, and the children read back unchanged', () => {
    for (const c of fx.frames) {
      const r = parseRunnerFrame(c.text);
      expect(r.ok, c.name).toBe(true);
      if (!r.ok) continue;
      expect(r.frame, c.name).toEqual(c.frame);
      expect(parseAgentChildren(c.frame.agent.children), c.name).toEqual(c.frame.agent.children ?? []);
    }
  });

  it('a frame from an older runner has no children', () => {
    const none = fx.frames.find((c) => c.name === 'no-helpers')!;
    expect('children' in none.frame.agent).toBe(false);
    expect(helpersWorking(none.frame.agent.children)).toBe(0);
  });

  it('the durable events parse as events', () => {
    for (const c of fx.events) {
      const r = parseRunnerFrame(c.text);
      expect(r.ok, c.name).toBe(true);
      if (r.ok) expect(r.frame, c.name).toEqual(c.frame);
      expect(fx.eventTypes).toContain(c.frame.type);
    }
  });

  it('reads junk without throwing', () => {
    for (const c of fx.lenient) expect(parseAgentChildren(c.input), c.name).toEqual(c.want);
  });

  it('counts helpers at work and words the count', () => {
    const nested = fx.frames.find((c) => c.name === 'nested-one-done')!;
    expect(helpersWorking(nested.frame.agent.children)).toBe(1);
    expect(helpersText(0)).toBe('');
    expect(helpersText(1)).toBe('1 helper');
    expect(helpersText(2)).toBe('2 helpers');
  });
});
