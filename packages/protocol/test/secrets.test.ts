import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { answerContext, isPublicKey, maskContext, open, publicKeyOf, seal } from '../src/secrets.js';

const f = JSON.parse(readFileSync(new URL('../fixtures/secrets.json', import.meta.url), 'utf8'));

describe('sealed delivery (fixtures/secrets.json)', () => {
  it('reproduces every vector byte for byte with the pinned keys', () => {
    expect(publicKeyOf(f.runnerPrivate)).toBe(f.runnerPublic);
    for (const v of f.vectors) {
      expect(seal(f.runnerPublic, Buffer.from(v.plain), v.context, { ephemeralPrivate: v.ephemeralPrivate, nonce: v.nonce })).toBe(v.sealed);
      expect(open(f.runnerPrivate, v.sealed, v.context)?.toString()).toBe(v.plain);
    }
  });

  it('binds the context, the key and the bytes', () => {
    const s = seal(f.runnerPublic, Buffer.from('value-12345678'), answerContext('m1', 's1'));
    expect(open(f.runnerPrivate, s, answerContext('m1', 's1'))?.toString()).toBe('value-12345678');
    expect(open(f.runnerPrivate, s, answerContext('m1', 's2'))).toBeNull();
    expect(open(f.runnerPrivate, s, maskContext('m1'))).toBeNull();
    const other = Buffer.alloc(32, 9).toString('base64url');
    expect(open(other, s, answerContext('m1', 's1'))).toBeNull();
    const parts = s.split('.');
    const ct = Buffer.from(parts[3]!, 'base64url');
    ct[0] = ct[0]! ^ 1;
    expect(open(f.runnerPrivate, [...parts.slice(0, 3), ct.toString('base64url')].join('.'), answerContext('m1', 's1'))).toBeNull();
    expect(open(f.runnerPrivate, 'x1.a.b', 'c')).toBeNull();
    expect(s).not.toContain('value-12345678');
    // Fresh ephemeral key and nonce every time.
    expect(seal(f.runnerPublic, Buffer.from('v'), 'c')).not.toBe(seal(f.runnerPublic, Buffer.from('v'), 'c'));
  });

  it('refuses keys that are not X25519 public keys', () => {
    expect(isPublicKey(f.runnerPublic)).toBe(true);
    for (const bad of ['', 'abc', f.runnerPublic + 'A', 42, null]) expect(isPublicKey(bad)).toBe(false);
    expect(() => seal('nope', Buffer.from('x'), 'c')).toThrow();
  });

  it('frames carry ciphertext only', () => {
    const text = JSON.stringify(f.frames);
    for (const v of f.vectors) for (const value of Object.values((JSON.parse(v.plain || '{}').values ?? {}) as Record<string, string>)) expect(text).not.toContain(value);
  });
});
