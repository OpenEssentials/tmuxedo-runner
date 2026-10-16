// MCP package pinning: the shared fixture (also read by the runner's tests),
// the rewrite behind "Pin to <version>", and the sentence a refusal says.
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { mcpPackage, pinCommand, pinProblemText, unpinnedProblem } from '../src/plugin-set.js';

interface Case { command: string[]; package: null | { registry: string; name: string; version: string | null; pinned: boolean; problem: string | null } }
const fixture = JSON.parse(readFileSync(new URL('../fixtures/mcp-pin.json', import.meta.url), 'utf8')) as {
  cases: Case[]; pin: Array<{ command: string[]; version: string; out: string[] | null }>;
};

describe('mcpPackage on the shared fixture', () => {
  for (const c of fixture.cases) {
    it(c.command.join(' '), () => {
      const l = mcpPackage(c.command);
      if (c.package === null) return expect(l).toBeNull();
      expect(l && { registry: l.registry, name: l.name, version: l.version, pinned: l.pinned, problem: l.problem }).toEqual(c.package);
    });
  }

  it('reads nothing out of a list that is not all strings', () => {
    expect(mcpPackage(['npx', 3, 'x@1.0.0'])).toBeNull();
    expect(mcpPackage([])).toBeNull();
  });

  it('notes a private index, so nothing is looked up for it', () => {
    expect(mcpPackage(['npx', '--registry', 'https://npm.example.test', 'pkg-a@3.0.0'])!.customIndex).toBe(true);
    expect(mcpPackage(['uvx', '--index-url=https://pypi.example.test/simple', 'x==1.0'])!.customIndex).toBe(true);
    expect(mcpPackage(['npx', 'pkg-a@3.0.0'])!.customIndex).toBe(false);
  });
});

describe('pinCommand', () => {
  for (const p of fixture.pin) {
    it(`${p.command.join(' ')} → ${p.version}`, () => {
      const l = mcpPackage(p.command)!;
      expect(pinCommand(p.command, l, p.version)).toEqual(p.out);
    });
  }

  it('changes only the element that names the package', () => {
    const cmd = ['npx', '-y', 'server-x', '--token-file', 'server-x'];
    expect(pinCommand(cmd, mcpPackage(cmd)!, '2.0.0')).toEqual(['npx', '-y', 'server-x@2.0.0', '--token-file', 'server-x']);
  });
});

describe('the sentence', () => {
  it('says what to write, per registry, and nothing for a pinned launch', () => {
    expect(pinProblemText(mcpPackage(['npx', 'server-x'])!)).toMatch(/server-x@1\.2\.3/);
    expect(pinProblemText(mcpPackage(['uvx', 'tool-pkg>=1'])!)).toMatch(/">=1" can change under you.*tool-pkg==1\.2\.3/);
    expect(pinProblemText(mcpPackage(['docker', 'run', 'mcp/fetch:latest'])!)).toMatch(/mcp\/fetch@sha256:/);
    expect(pinProblemText(mcpPackage(['npx', 'github:a/b'])!)).toMatch(/from an address/);
    expect(pinProblemText(mcpPackage(['npx', 'server-x@1.0.0'])!)).toBeNull();
  });

  it('is asked of a stdio definition only', () => {
    expect(unpinnedProblem({ transport: 'stdio', command: ['npx', 'server-x'] })).toMatch(/Pin server-x/);
    expect(unpinnedProblem({ transport: 'stdio', command: ['npx', 'server-x@1.0.0'] })).toBeNull();
    expect(unpinnedProblem({ transport: 'stdio', command: ['node', 'x.js'] })).toBeNull();
    expect(unpinnedProblem({ transport: 'http', url: 'https://x.test' })).toBeNull();
    expect(unpinnedProblem(null)).toBeNull();
  });

  it('keeps a hostile name short in the sentence', () => {
    const t = pinProblemText(mcpPackage(['npx', `${'A'.repeat(500)}@1.0.0`])!)!;
    expect(t.length).toBeLessThan(200);
  });
});
