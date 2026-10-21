import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { fromB64url, isSealKey, isSealedEnvelope, newQueuedId, openWith, queuedPlaintext, sealQueuedPrompt, sealTo, toB64url, validQueuedId } from '../src/seal.js';
import { open, queuedContext, seal } from '../src/secrets.js';

const f = JSON.parse(readFileSync(new URL('../fixtures/secrets.json', import.meta.url), 'utf8'));

describe('client seal (the same envelope as the Node seal and the runner)', () => {
  it('reproduces every fixture vector byte for byte, and opens them', () => {
    for (const v of f.vectors) {
      expect(sealTo(f.runnerPublic, v.plain, v.context, { ephemeralPrivate: v.ephemeralPrivate, nonce: v.nonce })).toBe(v.sealed);
      expect(new TextDecoder().decode(openWith(f.runnerPrivate, v.sealed, v.context)!)).toBe(v.plain);
      expect(openWith(f.runnerPrivate, v.sealed, f.wrongContext)).toBeNull();
    }
    expect(f.vectors.some((v: { name: string }) => v.name === 'queued-client')).toBe(true);
  });

  it('round-trips with the Node seal both ways', () => {
    const ctx = queuedContext('m1', 'q_abcdef12');
    const a = sealTo(f.runnerPublic, 'from the browser', ctx);
    expect(open(f.runnerPrivate, a, ctx)?.toString()).toBe('from the browser');
    const b = seal(f.runnerPublic, Buffer.from('from node'), ctx);
    expect(new TextDecoder().decode(openWith(f.runnerPrivate, b, ctx)!)).toBe('from node');
    // Fresh ephemeral key and nonce every time.
    expect(sealTo(f.runnerPublic, 'v', 'c')).not.toBe(sealTo(f.runnerPublic, 'v', 'c'));
  });

  it('binds the context, and a flipped bit does not open', () => {
    const s = sealTo(f.runnerPublic, 'hello there', 'm1|queued:q_1');
    expect(openWith(f.runnerPrivate, s, 'm1|queued:q_2')).toBeNull();
    const p = s.split('.');
    const ct = fromB64url(p[3]!)!;
    ct[0] ^= 1;
    expect(openWith(f.runnerPrivate, [...p.slice(0, 3), toB64url(ct)].join('.'), 'm1|queued:q_1')).toBeNull();
  });

  it('refuses bad and low-order keys', () => {
    expect(isSealKey(f.runnerPublic)).toBe(true);
    for (const bad of ['', 'abc', f.runnerPublic + 'A', 42, null]) expect(isSealKey(bad)).toBe(false);
    expect(() => sealTo('nope', 'x', 'c')).toThrow();
    expect(() => sealTo(toB64url(new Uint8Array(32)), 'x', 'c')).toThrow();
  });

  it('base64url matches Node for every length', () => {
    for (let n = 0; n < 40; n++) {
      const b = Uint8Array.from({ length: n }, (_, i) => (i * 37 + n) & 0xff);
      expect(toB64url(b)).toBe(Buffer.from(b).toString('base64url'));
      expect(Buffer.from(fromB64url(toB64url(b))!).equals(Buffer.from(b))).toBe(true);
    }
    expect(fromB64url('a')).toBeNull();
    expect(fromB64url('ab=c')).toBeNull();
    expect(fromB64url('AB')).toBeNull(); // non-zero leftover bits
  });

  it('checks the envelope shape without opening it', () => {
    const s = sealTo(f.runnerPublic, 'x', 'c');
    expect(isSealedEnvelope(s)).toBe(true);
    for (const bad of ['', 'plain text', 'x1.a.b.c', s.replace(/^x1/, 'x2'), s + '.x', s.split('.').slice(0, 3).join('.') + '.AAAA', 7, null]) {
      expect(isSealedEnvelope(bad)).toBe(false);
    }
    expect(isSealedEnvelope(s, 10)).toBe(false);
  });

  it('seals a queued prompt the runner can read: target and expiry inside', () => {
    const id = newQueuedId();
    expect(validQueuedId(id)).toBe(true);
    expect(id).toMatch(/^q_[A-Za-z0-9_-]{24}$/);
    const s = sealQueuedPrompt(f.runnerPublic, 'm1', id, 'run the tests', { kind: 'pane', id: 'w1:p2' }, 123);
    expect(s).not.toContain('run the tests');
    const body = JSON.parse(new TextDecoder().decode(openWith(f.runnerPrivate, s, queuedContext('m1', id))!));
    expect(body).toEqual({ v: 1, text: 'run the tests', target: { pane: 'w1:p2' }, expiresAt: 123 });
    expect(JSON.parse(queuedPlaintext('t', { kind: 'chat', id: 'c1234567' }, null))).toEqual({ v: 1, text: 't', target: { chat: 'c1234567' } });
    expect(() => sealQueuedPrompt(f.runnerPublic, 'm1', id, 'x'.repeat(64 * 1024 + 1), { kind: 'chat', id: 'c1234567' }, null)).toThrow('too_large');
  });

  it('imports nothing from node', () => {
    const src = readFileSync(new URL('../src/seal.ts', import.meta.url), 'utf8') + readFileSync(new URL('../src/queued.ts', import.meta.url), 'utf8');
    expect(src).not.toMatch(/from 'node:|Buffer\./);
  });
});
