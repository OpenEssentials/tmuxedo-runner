import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { generateKeyPairSync, sign } from 'node:crypto';
import { RUNNER_PROOF_DOMAIN, isP256PublicJwk, newRunnerNonce, parseRunnerFrame, runnerProofInput, verifyRunnerProof } from '../src/index.js';

// The same file the runner checks (gateway.rs proof tests).
const fx = JSON.parse(readFileSync(new URL('../fixtures/runner-proof.json', import.meta.url), 'utf8'));

describe('runner proof', () => {
  it('golden fixture: the input, valid DER and raw signatures, invalid ones', () => {
    expect(runnerProofInput(fx.nonce, fx.lookup)).toBe(fx.input);
    expect(fx.input.startsWith(`${RUNNER_PROOF_DOMAIN}\n`)).toBe(true);
    for (const s of fx.valid) expect(verifyRunnerProof(fx.publicKey, fx.nonce, fx.lookup, s)).toBe(true);
    for (const s of fx.invalid) expect(verifyRunnerProof(fx.publicKey, fx.nonce, fx.lookup, s)).toBe(false);
    // Bound to the nonce and the lookup.
    expect(verifyRunnerProof(fx.publicKey, newRunnerNonce(), fx.lookup, fx.valid[0])).toBe(false);
    expect(verifyRunnerProof(fx.publicKey, fx.nonce, 'other', fx.valid[0])).toBe(false);
  });

  it('refuses keys that are not P-256 public JWKs, and junk signatures, without throwing', () => {
    expect(isP256PublicJwk(fx.publicKey)).toBe(true);
    expect(isP256PublicJwk({ ...fx.publicKey, d: 'x' })).toBe(false);
    expect(isP256PublicJwk({ ...fx.publicKey, crv: 'P-384' })).toBe(false);
    expect(isP256PublicJwk(null)).toBe(false);
    expect(verifyRunnerProof({ ...fx.publicKey, x: 'AAAA' }, fx.nonce, fx.lookup, fx.valid[0])).toBe(false);
    for (const s of [undefined, 1, 'a', 'x'.repeat(300), 'not+base64/url']) expect(verifyRunnerProof(fx.publicKey, fx.nonce, fx.lookup, s)).toBe(false);
    const { privateKey } = generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
    const foreign = sign('sha256', Buffer.from(fx.input), { key: privateKey, dsaEncoding: 'der' }).toString('base64url');
    expect(verifyRunnerProof(fx.publicKey, fx.nonce, fx.lookup, foreign)).toBe(false);
  });

  it('frames: proof parses; a bad one is refused', () => {
    expect(parseRunnerFrame(JSON.stringify(fx.frames.proof))).toEqual({ ok: true, frame: fx.frames.proof });
    expect(parseRunnerFrame(JSON.stringify({ t: 'proof' }))).toEqual({ ok: false, reason: 'bad_proof' });
    expect(parseRunnerFrame(JSON.stringify({ t: 'proof', sig: 'x'.repeat(201) }))).toEqual({ ok: false, reason: 'bad_proof' });
    expect(newRunnerNonce()).toMatch(/^[A-Za-z0-9_-]{43}$/);
  });
});
