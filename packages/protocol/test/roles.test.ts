import { describe, expect, it } from 'vitest';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import {
  ROLE_ACCESS, ROLE_COLORS, ROLE_DELEGATES_MAX, ROLE_INSTRUCTIONS_MAX_BYTES, ROLE_LIST_MAX, ROLE_MAX_DEPTH, ROLE_MAX_SUBAGENTS, ROLE_NAME_MAX,
  ROLE_SCHEMA, canonicalRole, parseRole, roleFromCanonical,
} from '../src/roles.js';

// The same file the runner checks (runner/crates/protocol/tests/roles.rs).
const fx = JSON.parse(readFileSync(new URL('../fixtures/roles.json', import.meta.url), 'utf8')) as {
  schema: number; access: string[]; colors: string[]; limits: Record<string, number>;
  valid: { name: string; input: unknown; role: unknown; canonical: string; hash: string }[];
  invalid: { name: string; input: unknown; paths: string[] }[];
};
const sha = (s: string) => createHash('sha256').update(s, 'utf8').digest('hex');

describe('roles', () => {
  it('constants match the fixture', () => {
    expect(ROLE_SCHEMA).toBe(fx.schema);
    expect([...ROLE_ACCESS]).toEqual(fx.access);
    expect([...ROLE_COLORS]).toEqual(fx.colors);
    expect(fx.limits).toMatchObject({ name: ROLE_NAME_MAX, instructionsBytes: ROLE_INSTRUCTIONS_MAX_BYTES, list: ROLE_LIST_MAX, delegates: ROLE_DELEGATES_MAX, maxSubagents: ROLE_MAX_SUBAGENTS, maxDepth: ROLE_MAX_DEPTH });
  });

  it('valid roles parse to the listed normal form, canonical text and hash', () => {
    expect(fx.valid.length).toBeGreaterThan(5);
    for (const c of fx.valid) {
      const r = parseRole(c.input);
      expect(r.ok, c.name).toBe(true);
      if (!r.ok) continue;
      expect(r.role, c.name).toEqual(c.role);
      const text = canonicalRole(r.role);
      expect(text, c.name).toBe(c.canonical);
      expect(sha(text), c.name).toBe(c.hash);
    }
  });

  it('the canonical text reads back as the same role', () => {
    for (const c of fx.valid) {
      const back = roleFromCanonical(c.canonical);
      expect(back, c.name).toEqual(c.role);
      expect(canonicalRole(back!), c.name).toBe(c.canonical);
    }
    expect(roleFromCanonical('{')).toBeNull();
    expect(roleFromCanonical('{"id":"x"}')).toBeNull();
  });

  it('field order and written-out defaults do not change the hash; any change of meaning does', () => {
    const by = (n: string) => fx.valid.find((c) => c.name === n)!;
    expect(by('every-field').hash).toBe(by('every-field-in-another-order').hash);
    expect(by('the-smallest-role').hash).toBe(by('defaults-written-out-and-empty-parts').hash);
    const base = by('every-field').input as Record<string, unknown>;
    const seen = new Set([by('every-field').hash]);
    const changes = [{ access: 'edit' }, { name: 'Reviewer 2' }, { instructions: 'Other.' }, { delegates: [] }, { limits: { max_subagents: 2, max_depth: 1 } }, { tools: { allow: ['Grep', 'Read'] } }];
    for (const change of changes) {
      const r = parseRole({ ...base, ...change });
      expect(r.ok).toBe(true);
      if (r.ok) seen.add(sha(canonicalRole(r.role)));
    }
    expect(seen.size).toBe(changes.length + 1);
  });

  it('invalid roles name every field that is wrong', () => {
    expect(fx.invalid.length).toBeGreaterThan(10);
    for (const c of fx.invalid) {
      const r = parseRole(c.input);
      expect(r.ok, c.name).toBe(false);
      if (r.ok) continue;
      expect(r.problems.map((p) => p.path), c.name).toEqual(c.paths);
      for (const p of r.problems) expect(p.message.length, c.name).toBeGreaterThan(5);
    }
  });

  it('a parsed role never shares arrays with its input', () => {
    const input = { id: 'ab', name: 'A', mcp: ['github'] };
    const r = parseRole(input);
    expect(r.ok).toBe(true);
    if (r.ok) { r.role.mcp.push('x'); expect(input.mcp).toEqual(['github']); }
  });
});
