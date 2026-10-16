/**
 * Runner proof (docs/protocol.md "Runner proof", docs/design/mac-pairing.md §3.2).
 *
 * A key-bound runner token is useless on its own: on every connection the
 * gateway sends `{t:"challenge", nonce}` and the runner must answer
 * `{t:"proof", sig}`, an ECDSA P-256 / SHA-256 signature (base64url, DER or
 * raw r‖s) over
 *
 *     tmuxedo-runner-proof-v1\n<nonce>\n<token lookup>
 *
 * made by the key the token is bound to (a Secure Enclave key on a Mac). The
 * nonce is 32 random bytes, base64url. Golden fixture:
 * fixtures/runner-proof.json (checked here and by the runner).
 */
import { createPublicKey, randomBytes, verify, type JsonWebKey } from 'node:crypto';

export const RUNNER_PROOF_DOMAIN = 'tmuxedo-runner-proof-v1';
/** How long the gateway waits for the proof after its challenge. */
export const RUNNER_PROOF_TIMEOUT_MS = 10_000;

export interface P256PublicJwk { kty: 'EC'; crv: 'P-256'; x: string; y: string }

export const runnerProofInput = (nonce: string, lookup: string) => `${RUNNER_PROOF_DOMAIN}\n${nonce}\n${lookup}`;

export const newRunnerNonce = () => randomBytes(32).toString('base64url');

export function isP256PublicJwk(v: unknown): v is P256PublicJwk {
  const k = v as Record<string, unknown> | null;
  return !!k && typeof k === 'object' && k.kty === 'EC' && k.crv === 'P-256' && typeof k.x === 'string' && typeof k.y === 'string' && !('d' in k);
}

/** True only for a valid signature by `key` over the proof input. Never throws. */
export function verifyRunnerProof(key: unknown, nonce: string, lookup: string, sig: unknown): boolean {
  if (!isP256PublicJwk(key) || typeof sig !== 'string' || sig.length < 16 || sig.length > 200 || !/^[A-Za-z0-9_-]+$/.test(sig)) return false;
  try {
    const pub = createPublicKey({ key: { kty: 'EC', crv: 'P-256', x: key.x, y: key.y } as JsonWebKey, format: 'jwk' });
    const raw = Buffer.from(sig, 'base64url');
    const data = Buffer.from(runnerProofInput(nonce, lookup), 'utf8');
    return verify('sha256', data, { key: pub, dsaEncoding: raw.length === 64 ? 'ieee-p1363' : 'der' }, raw);
  } catch {
    return false;
  }
}
