import { describe, expect, it } from 'vitest';
import { authenticate, LEGACY_TOKEN_PREFIX, mintToken, parseToken, TOKEN_PREFIX } from '../src/runner-token.js';

describe('runner tokens', () => {
  it('mints tmr tokens that parse and authenticate', () => {
    const t = mintToken();
    expect(TOKEN_PREFIX).toBe('tmr');
    expect(t.plaintext.startsWith('tmr.')).toBe(true);
    const p = parseToken(t.plaintext);
    expect(authenticate(p, { machineId: 'm1', secretHash: t.secretHash, revokedAt: null })).toMatchObject({ ok: true, machineId: 'm1' });
  });
  it('still parses and authenticates a legacy amr token (issued before the tmuxedo rename)', () => {
    expect(LEGACY_TOKEN_PREFIX).toBe('amr');
    const t = mintToken();
    const legacy = t.plaintext.replace(/^tmr\./, 'amr.');
    expect(legacy.startsWith('amr.')).toBe(true);
    const p = parseToken(legacy);
    expect(p).toEqual(parseToken(t.plaintext));
    expect(authenticate(p, { machineId: 'm1', secretHash: t.secretHash, revokedAt: null })).toMatchObject({ ok: true, machineId: 'm1' });
  });
  it('parses every token across many mints (base64url never contains the separator)', () => {
    for (let i = 0; i < 2000; i++) expect(parseToken(mintToken().plaintext)).not.toBeNull();
  });
  it('refuses revoked tokens and wrong secrets with a loggable reason', () => {
    const a = mintToken(), b = mintToken();
    expect(authenticate(parseToken(a.plaintext), { machineId: 'm1', secretHash: a.secretHash, revokedAt: new Date(0) })).toMatchObject({ ok: false, reason: expect.stringMatching(/revoked/) });
    expect(authenticate(parseToken(a.plaintext), { machineId: 'm1', secretHash: b.secretHash, revokedAt: null })).toMatchObject({ ok: false, reason: expect.stringMatching(/mismatch/) });
  });
  it('rejects foreign-prefixed and malformed tokens', () => {
    expect(parseToken('tbr.' + 'a'.repeat(22) + '.' + 'b'.repeat(43))).toBeNull();
    expect(parseToken('amr.short.x')).toBeNull();
    expect(parseToken('tmr.short.x')).toBeNull();
    expect(parseToken('xmr.' + 'a'.repeat(22) + '.' + 'b'.repeat(43))).toBeNull();
    expect(parseToken(undefined)).toBeNull();
  });
});
