import { describe, expect, it } from 'vitest';
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { AGENT_STATUS_TEXT, AGENT_TITLES, agentShortName, agentTitle } from '../src/agents.js';

const AGENTS = path.join(path.dirname(fileURLToPath(import.meta.url)), '../../../runner/crates/detect/agents');

describe('agent names', () => {
  it('match the detect crate agent files exactly', () => {
    const files: Record<string, string> = {};
    for (const f of readdirSync(AGENTS).filter((f) => f.endsWith('.toml'))) {
      const text = readFileSync(path.join(AGENTS, f), 'utf8');
      const id = /^agent = "([^"]+)"$/m.exec(text)?.[1];
      const title = /^title = "([^"]+)"$/m.exec(text)?.[1];
      expect(id && title, f).toBeTruthy();
      files[id!] = title!;
    }
    expect(Object.keys(files).length).toBe(24);
    expect(AGENT_TITLES).toEqual(files);
  });

  it('titles, short names and fallbacks', () => {
    expect(agentTitle('claude')).toBe('Claude Code');
    expect(agentTitle('agy')).toBe('Antigravity CLI');
    expect(agentTitle('something-new-and-long-name-x')).toBe('something-new-and-long-n');
    expect(agentTitle('')).toBe('An agent');
    expect(agentTitle(null)).toBe('An agent');
    expect(agentTitle(7)).toBe('An agent');
    expect(agentShortName('claude')).toBe('Claude');
    expect(agentShortName('qodercli')).toBe('Qoder');
    expect(agentShortName('opencode')).toBe('opencode');
    expect(agentShortName('omp')).toBe('Oh My Pi');
    expect(agentShortName(undefined)).toBe('An agent');
    for (const id of Object.keys(AGENT_TITLES)) expect(agentShortName(id).length).toBeLessThanOrEqual(agentTitle(id).length);
  });

  it('every status has words', () => {
    expect(Object.keys(AGENT_STATUS_TEXT).sort()).toEqual(['blocked', 'done', 'idle', 'unknown', 'working']);
  });
});
