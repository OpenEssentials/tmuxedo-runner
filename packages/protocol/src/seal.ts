/**
 * The client seal: the same sealed envelope as ./secrets (docs/protocol.md
 * "Secrets", "Queued prompts"), built from audited pure-JS primitives so a
 * browser or the app can seal to a runner's X25519 key and the server only
 * ever receives ciphertext.
 *
 *   sealed = "x1." b64url(epk) "." b64url(nonce) "." b64url(ciphertext ‖ tag)
 *   key    = HKDF-SHA256(salt = epk ‖ runner_pk, ikm = X25519(esk, runner_pk), info = "tmuxedo-secret-v1")
 *   AEAD   = ChaCha20-Poly1305(key, nonce, aad = context)
 *
 * Byte for byte the format of ./secrets `seal` and of the runner's
 * runner/crates/runner/src/secrets/seal.rs: fixtures/secrets.json is checked
 * against all three. No node: imports and no Buffer, so it bundles for the
 * browser; it needs only `crypto.getRandomValues`.
 */
import { chacha20poly1305 } from '@noble/ciphers/chacha.js';
import { x25519 } from '@noble/curves/ed25519.js';
import { hkdf } from '@noble/hashes/hkdf.js';
import { sha256 } from '@noble/hashes/sha2.js';
import { QUEUED_MAX_SEALED_BYTES, QUEUED_MAX_TEXT_BYTES, queuedContext } from './queued.js';

export { QUEUED_MAX_SEALED_BYTES, QUEUED_MAX_TEXT_BYTES, queuedContext, validQueuedId } from './queued.js';

const INFO = new TextEncoder().encode('tmuxedo-secret-v1');
const enc = new TextEncoder();

const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';
const B64_INDEX = new Map([...B64].map((c, i) => [c, i]));

/** base64url without padding. */
export function toB64url(b: Uint8Array): string {
  let out = '';
  for (let i = 0; i < b.length; i += 3) {
    const n = (b[i]! << 16) | ((b[i + 1] ?? 0) << 8) | (b[i + 2] ?? 0);
    const left = b.length - i;
    out += B64[(n >> 18) & 63]! + B64[(n >> 12) & 63]! + (left > 1 ? B64[(n >> 6) & 63]! : '') + (left > 2 ? B64[n & 63]! : '');
  }
  return out;
}

/** Null when `s` is not unpadded base64url. */
export function fromB64url(s: string): Uint8Array | null {
  if (s.length % 4 === 1) return null;
  const out = new Uint8Array(Math.floor((s.length * 3) / 4));
  let bits = 0;
  let acc = 0;
  let j = 0;
  for (const c of s) {
    const v = B64_INDEX.get(c);
    if (v === undefined) return null;
    acc = (acc << 6) | v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out[j++] = (acc >> bits) & 0xff;
    }
  }
  // Leftover bits must be zero (canonical encoding).
  if (bits > 0 && (acc & ((1 << bits) - 1)) !== 0) return null;
  return out;
}

/** Is `s` a base64url X25519 public key? */
export function isSealKey(s: unknown): s is string {
  return typeof s === 'string' && /^[A-Za-z0-9_-]{43}$/.test(s) && fromB64url(s)?.length === 32;
}

/**
 * The shape of a sealed envelope (`x1.<epk>.<nonce>.<ct‖tag>`), without
 * opening it: what a server that cannot open it checks before storing it.
 */
export function isSealedEnvelope(s: unknown, maxLength = QUEUED_MAX_SEALED_BYTES): s is string {
  if (typeof s !== 'string' || s.length > maxLength) return false;
  const p = s.split('.');
  if (p.length !== 4 || p[0] !== 'x1') return false;
  if (!/^[A-Za-z0-9_-]{43}$/.test(p[1]!) || fromB64url(p[1]!)?.length !== 32) return false;
  if (!/^[A-Za-z0-9_-]{16}$/.test(p[2]!) || fromB64url(p[2]!)?.length !== 12) return false;
  const ct = /^[A-Za-z0-9_-]+$/.test(p[3]!) ? fromB64url(p[3]!) : null;
  return !!ct && ct.length >= 16;
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

function derive(shared: Uint8Array, epk: Uint8Array, rpk: Uint8Array): Uint8Array {
  return hkdf(sha256, shared, concat(epk, rpk), INFO, 32);
}

function randomBytes(n: number): Uint8Array {
  return globalThis.crypto.getRandomValues(new Uint8Array(n));
}

/**
 * Seal `plain` to the runner key `publicKey` (base64url) under `context`.
 * `fixed` pins the ephemeral key and nonce: fixtures only.
 */
export function sealTo(publicKey: string, plain: Uint8Array | string, context: string, fixed?: { ephemeralPrivate: string; nonce: string }): string {
  if (!isSealKey(publicKey)) throw new Error('bad runner key');
  const rpk = fromB64url(publicKey)!;
  const esk = fixed ? fromB64url(fixed.ephemeralPrivate) : x25519.utils.randomSecretKey();
  const nonce = fixed ? fromB64url(fixed.nonce) : randomBytes(12);
  if (!esk || esk.length !== 32 || !nonce || nonce.length !== 12) throw new Error('bad fixed key or nonce');
  const epk = x25519.getPublicKey(esk);
  // Rejects low-order runner keys (an all-zero shared secret) by throwing.
  const shared = x25519.getSharedSecret(esk, rpk);
  if (shared.every((b) => b === 0)) throw new Error('degenerate key');
  const key = derive(shared, epk, rpk);
  const body = typeof plain === 'string' ? enc.encode(plain) : plain;
  const ct = chacha20poly1305(key, nonce, enc.encode(context)).encrypt(body);
  return ['x1', toB64url(epk), toB64url(nonce), toB64url(ct)].join('.');
}

/** Open with a runner private key (tests and fixtures; a client never holds one). Null when it does not open. */
export function openWith(privateKey: string, sealed: string, context: string): Uint8Array | null {
  if (!isSealedEnvelope(sealed, Number.MAX_SAFE_INTEGER)) return null;
  const [, e, n, c] = sealed.split('.');
  try {
    const sk = fromB64url(privateKey);
    if (!sk || sk.length !== 32) return null;
    const epk = fromB64url(e!)!;
    const shared = x25519.getSharedSecret(sk, epk);
    const key = derive(shared, epk, x25519.getPublicKey(sk));
    return chacha20poly1305(key, fromB64url(n!)!, enc.encode(context)).decrypt(fromB64url(c!)!);
  } catch {
    return null;
  }
}

export type QueuedSealTarget = { kind: 'chat' | 'pane'; id: string };

/** A fresh queued-prompt id (`q_` + 24 base64url characters): the client picks it, since the seal binds it. */
export function newQueuedId(): string {
  return `q_${toB64url(randomBytes(18))}`;
}

/** The plaintext the runner opens (runner `tmuxedo_protocol::queued`). */
export function queuedPlaintext(text: string, target: QueuedSealTarget, expiresAt: number | null): string {
  return JSON.stringify({ v: 1, text, target: target.kind === 'chat' ? { chat: target.id } : { pane: target.id }, ...(expiresAt != null ? { expiresAt } : {}) });
}

/**
 * Seal one queued prompt for machine `machineId` whose runner key is
 * `publicKey`: target and expiry go inside, so nobody between can redirect or
 * extend it. Throws when the text is over 64 KiB.
 */
export function sealQueuedPrompt(publicKey: string, machineId: string, id: string, text: string, target: QueuedSealTarget, expiresAt: number | null): string {
  if (enc.encode(text).length > QUEUED_MAX_TEXT_BYTES) throw new Error('too_large');
  return sealTo(publicKey, queuedPlaintext(text, target, expiresAt), queuedContext(machineId, id));
}
