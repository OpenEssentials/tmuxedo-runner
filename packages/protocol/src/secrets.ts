/**
 * Secrets on the wire (docs/protocol.md "Secrets", docs/design/secrets-and-env.md §3.3).
 *
 * Web seals every value it hands a runner to that runner's X25519 public key,
 * so the gateway and Cloudflare relay only ciphertext:
 *
 *   sealed = "x1." b64url(epk) "." b64url(nonce) "." b64url(ciphertext ‖ tag)
 *   key    = HKDF-SHA256(salt = epk ‖ runner_pk, ikm = X25519(esk, runner_pk), info = "tmuxedo-secret-v1")
 *   AEAD   = ChaCha20-Poly1305(key, nonce, aad = context)
 *
 * `context` binds a blob to one use: `<machineId>|req:<id>` for the answer to
 * `secret_req` id, `<machineId>|mask` for the mask list in a `secrets` frame.
 * The runner side is runner/crates/runner/src/secrets/seal.rs; both are checked
 * against packages/protocol/fixtures/secrets.json.
 *
 * Node-only (node:crypto): imported by web and the gateway tests, never by the app.
 */
import { createCipheriv, createDecipheriv, createPrivateKey, createPublicKey, diffieHellman, generateKeyPairSync, hkdfSync, randomBytes, type KeyObject } from 'node:crypto';

export const SECRETS_FEATURE = 'secrets';
export const SEAL_ALG = 'x25519-hkdf-sha256-chacha20poly1305';
const INFO = Buffer.from('tmuxedo-secret-v1');
const PKCS8_X25519 = Buffer.from('302e020100300506032b656e04220420', 'hex');
const SPKI_X25519 = Buffer.from('302a300506032b656e032100', 'hex');

/** What a runner may ask for (`secret_req.purpose.kind`). */
export const SECRET_PURPOSES = ['exec', 'mcp', 'agent', 'git', 'pane', 'ssh'] as const;
export type SecretPurposeKind = (typeof SECRET_PURPOSES)[number];

export interface SecretPurpose {
  kind: SecretPurposeKind;
  /** The plugin whose agent or MCP server needs it. */
  plugin?: string;
  paneId?: string;
  /** MCP server name (kind `mcp`). */
  mcp?: string;
  /** Resolved absolute program path (kind `exec`). */
  program?: string;
  /** Destination host (kind `git`). */
  host?: string;
  /** Project key of the pane's cwd (scope matching). */
  project?: string;
}

/** runner → gateway */
export interface SecretKeyFrame { t: 'secret_key'; id: string; alg: string; publicKey: string }
export interface SecretReqFrame { t: 'secret_req'; id: string; names: string[]; purpose: SecretPurpose }
/** gateway → runner: the answer to either. */
export interface SecretResFrame { t: 'secret_res'; id: string; result?: Record<string, unknown>; error?: { code: string; message?: string } }
/** gateway → runner: the mask list and the machine's secret policy (after `welcome` and on change). */
export interface SecretsFrame { t: 'secrets'; mask?: string; policy?: SecretsPolicy; revoked?: string[] }

/** Names only (no values): what the runner may tell agents and what it may cache offline. */
export interface SecretsPolicy {
  /** Secret names bound to this machine, with how each may be used. */
  names: Array<{ name: string; bindings: SecretBinding[] }>;
  /** BYO machines only: keep a sealed copy on disk for offline use. */
  offlineCache: boolean;
  /** SSH keys this machine serves (secrets-and-env.md §6a). */
  ssh?: unknown[];
}

export type SecretBinding =
  | { kind: 'exec'; programs: string[] }
  | { kind: 'mcp'; plugin: string; server: string }
  | { kind: 'agent'; plugin: string }
  | { kind: 'git'; hosts: string[] }
  | { kind: 'pane' };

/** Plaintext of an answer: name → value (and the version each came from). */
export interface SecretAnswer { values: Record<string, string>; versions: Record<string, number> }
/** Plaintext of the mask list: every value delivered to the machine in the last 30 days (all versions). */
export interface SecretMaskList { values: string[] }

export const answerContext = (machineId: string, id: string) => `${machineId}|req:${id}`;
export const maskContext = (machineId: string) => `${machineId}|mask`;

/** Queued prompts (./queued; the client seal is ./seal). */
export { QUEUED_FEATURE, QUEUED_MAX_SEALED_BYTES, QUEUED_MAX_TEXT_BYTES, QUEUED_METHOD, queuedContext, validQueuedId } from './queued.js';

const b64 = (b: Buffer | Uint8Array) => Buffer.from(b).toString('base64url');

/** Is `s` a base64url X25519 public key? */
export function isPublicKey(s: unknown): s is string {
  return typeof s === 'string' && /^[A-Za-z0-9_-]{43}$/.test(s) && Buffer.from(s, 'base64url').length === 32;
}

function publicKeyObject(raw: Buffer): KeyObject {
  return createPublicKey({ key: Buffer.concat([SPKI_X25519, raw]), format: 'der', type: 'spki' });
}
function privateKeyObject(raw: Buffer): KeyObject {
  return createPrivateKey({ key: Buffer.concat([PKCS8_X25519, raw]), format: 'der', type: 'pkcs8' });
}
function rawPublic(k: KeyObject): Buffer {
  return (k.export({ format: 'der', type: 'spki' }) as Buffer).subarray(SPKI_X25519.length);
}

function derive(shared: Buffer, epk: Buffer, rpk: Buffer): Buffer {
  return Buffer.from(hkdfSync('sha256', shared, Buffer.concat([epk, rpk]), INFO, 32));
}

/**
 * Seal `plain` to the runner key `publicKey` (base64url). `fixed` pins the
 * ephemeral key and nonce: fixtures only.
 */
export function seal(publicKey: string, plain: Uint8Array, context: string, fixed?: { ephemeralPrivate: string; nonce: string }): string {
  if (!isPublicKey(publicKey)) throw new Error('bad runner key');
  const rpk = Buffer.from(publicKey, 'base64url');
  const esk = fixed ? privateKeyObject(Buffer.from(fixed.ephemeralPrivate, 'base64url')) : generateKeyPairSync('x25519').privateKey;
  const epk = rawPublic(createPublicKey(esk));
  const shared = diffieHellman({ privateKey: esk, publicKey: publicKeyObject(rpk) });
  if (shared.every((b) => b === 0)) throw new Error('degenerate key');
  const key = derive(shared, epk, rpk);
  const nonce = fixed ? Buffer.from(fixed.nonce, 'base64url') : randomBytes(12);
  const c = createCipheriv('chacha20-poly1305', key, nonce, { authTagLength: 16 });
  c.setAAD(Buffer.from(context), { plaintextLength: plain.length });
  const ct = Buffer.concat([c.update(plain), c.final(), c.getAuthTag()]);
  return ['x1', b64(epk), b64(nonce), b64(ct)].join('.');
}

/** Open with a runner private key (tests and fixtures; web never holds one). Null when it does not open. */
export function open(privateKey: string, sealed: string, context: string): Buffer | null {
  const parts = sealed.split('.');
  if (parts.length !== 4 || parts[0] !== 'x1') return null;
  try {
    const sk = privateKeyObject(Buffer.from(privateKey, 'base64url'));
    const rpk = rawPublic(createPublicKey(sk));
    const epk = Buffer.from(parts[1]!, 'base64url');
    const nonce = Buffer.from(parts[2]!, 'base64url');
    const all = Buffer.from(parts[3]!, 'base64url');
    if (epk.length !== 32 || nonce.length !== 12 || all.length < 16) return null;
    const shared = diffieHellman({ privateKey: sk, publicKey: publicKeyObject(epk) });
    const key = derive(shared, epk, rpk);
    const d = createDecipheriv('chacha20-poly1305', key, nonce, { authTagLength: 16 });
    d.setAAD(Buffer.from(context), { plaintextLength: all.length - 16 });
    d.setAuthTag(all.subarray(all.length - 16));
    return Buffer.concat([d.update(all.subarray(0, all.length - 16)), d.final()]);
  } catch {
    return null;
  }
}

/** The public key (base64url) of a raw private key (base64url): tests. */
export function publicKeyOf(privateKey: string): string {
  return b64(rawPublic(createPublicKey(privateKeyObject(Buffer.from(privateKey, 'base64url')))));
}

/** Secret names: what a shell variable may be called (`GH_TOKEN`). */
export const SECRET_NAME = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
/** Shorter values are refused: the masker would hit ordinary text (runner secrets::mask::MIN_LEN). */
export const MIN_SECRET_LENGTH = 8;
export const MAX_SECRET_BYTES = 64 * 1024;
