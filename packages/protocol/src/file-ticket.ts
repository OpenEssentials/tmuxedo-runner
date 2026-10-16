/**
 * Upload and download tickets (docs/protocol.md "Files"): web's signed
 * statement that this user may send these bytes to (or read this file
 * from) this machine. The gateway checks the signature, audience and clock,
 * exactly as for attach tickets, and never asks Postgres.
 *
 *     f1.<base64url(json claims)>.<base64url(hmac-sha256)>
 *
 * A separate version tag and audience from attach tickets (ticket.ts), with
 * the same secret: a socket ticket is never a file ticket and the other way
 * round, because each verifier refuses the other's prefix and audience.
 */
import { createHmac, timingSafeEqual } from 'node:crypto';
import { FILE_TICKET_TTL_SECONDS, TRANSFER_ID, validUploadShape, FILES_DIRS, type FilesDir } from './files.js';
import { fsPathComponents, validFsRootId } from './fs.js';

export const FILE_TICKET_VERSION = 'f1';
export const FILE_TICKET_AUDIENCE = 'tmuxedo-files';
const SKEW_SECONDS = 5;
const MIN_SECRET_BYTES = 32;

interface Common {
  /** User id. */
  sub: string;
  /** Web session or API token id it was minted from. */
  sid: string;
  /** The machine. */
  mid: string;
  aud: typeof FILE_TICKET_AUDIENCE;
  iat: number;
  exp: number;
}

/** PUT chunks / GET status / POST complete / DELETE of one upload. */
export interface UploadClaims extends Common {
  op: 'upload';
  /** The upload id (web made it; the runner holds its state). */
  uid: string;
  size: number;
  /** Chunk size. */
  cs: number;
}

/** GET (with Range) of one file in a project's inbox or outbox. */
export interface DownloadClaims extends Common {
  op: 'download';
  project: string;
  dir: FilesDir;
  name: string;
}

/**
 * GET of one file in a file-browser root (docs/design/file-browser.md §3.7):
 * `{machine, root, path, user}`, used once. Web mints it after the same checks
 * as a browse (owner; a token with `read` and `terminal`); the gateway refuses
 * a second use of the same `jti` and the runner opens the path through its jail.
 */
export interface FsDownloadClaims extends Common {
  op: 'fs_download';
  /** A root id the runner gave (`fs.roots`). */
  root: string;
  /** Root-relative path, by the protocol's one lexical rule; never empty. */
  path: string;
  /** One-time id: the gateway remembers it until `exp`. */
  jti: string;
}

/** How long an fs download ticket lives: one click, not a link to keep. */
export const FS_DOWNLOAD_TTL_SECONDS = 60;
/** A one-time id: 16 to 64 URL-safe characters. */
export const FS_DOWNLOAD_JTI = /^[A-Za-z0-9_-]{16,64}$/;

export type FileTicketClaims = UploadClaims | DownloadClaims | FsDownloadClaims;
export type FileTicketRefusal = 'malformed' | 'bad_signature' | 'wrong_audience' | 'expired' | 'not_yet_valid';
export type FileTicketCheck = { ok: true; claims: FileTicketClaims } | { ok: false; reason: FileTicketRefusal };

/** A project key as the runner makes them (`github.com_org_repo`, `path_<name>_<hash>`). */
export const PROJECT_KEY = /^[A-Za-z0-9_-][A-Za-z0-9._-]{0,159}$/;

function sign(body: string, secret: string): string {
  return createHmac('sha256', secret).update(`${FILE_TICKET_VERSION}.${body}`).digest('base64url');
}

type MintInput =
  | (Omit<UploadClaims, 'aud' | 'iat' | 'exp'> & { ttlSeconds?: number; now?: Date })
  | (Omit<DownloadClaims, 'aud' | 'iat' | 'exp'> & { ttlSeconds?: number; now?: Date })
  | (Omit<FsDownloadClaims, 'aud' | 'iat' | 'exp'> & { ttlSeconds?: number; now?: Date });

export function mintFileTicket(input: MintInput, secret: string): string {
  if (Buffer.byteLength(secret, 'utf8') < MIN_SECRET_BYTES) throw new Error(`gateway ticket secret must be at least ${MIN_SECRET_BYTES} bytes`);
  const iat = Math.floor((input.now ?? new Date()).getTime() / 1000);
  const { ttlSeconds, now: _now, ...rest } = input;
  const ttl = ttlSeconds ?? (rest.op === 'fs_download' ? FS_DOWNLOAD_TTL_SECONDS : FILE_TICKET_TTL_SECONDS);
  const claims = { ...rest, aud: FILE_TICKET_AUDIENCE, iat, exp: iat + ttl } as FileTicketClaims;
  if (!shapeOk(claims)) throw new RangeError('file ticket claims are not valid');
  const body = Buffer.from(JSON.stringify(claims), 'utf8').toString('base64url');
  return `${FILE_TICKET_VERSION}.${body}.${sign(body, secret)}`;
}

const isStr = (v: unknown, max = 128): v is string => typeof v === 'string' && v.length > 0 && v.length <= max;

function shapeOk(c: FileTicketClaims): boolean {
  if (!isStr(c.sub) || !isStr(c.sid) || !isStr(c.mid) || typeof c.iat !== 'number' || typeof c.exp !== 'number') return false;
  if (c.op === 'upload') return typeof c.uid === 'string' && TRANSFER_ID.test(c.uid) && validUploadShape(c.size, c.cs);
  if (c.op === 'download') {
    return typeof c.project === 'string' && PROJECT_KEY.test(c.project) && FILES_DIRS.includes(c.dir) && isStr(c.name, 400) && !/[/\\\0]/.test(c.name);
  }
  if (c.op === 'fs_download') {
    const parts = fsPathComponents(c.path);
    // Never longer-lived than a click: a longer one is not a ticket web would sign.
    return c.exp - c.iat <= FS_DOWNLOAD_TTL_SECONDS && validFsRootId(c.root) && parts !== null && parts.length > 0 && typeof c.jti === 'string' && FS_DOWNLOAD_JTI.test(c.jti);
  }
  return false;
}

/** Check a presented file ticket. NEVER THROWS (it runs on every request from the internet). */
export function verifyFileTicket(raw: string | null | undefined, secret: string, now: Date = new Date()): FileTicketCheck {
  const parts = (raw ?? '').trim().split('.');
  if (parts.length !== 3 || parts[0] !== FILE_TICKET_VERSION || !parts[1] || !parts[2]) return { ok: false, reason: 'malformed' };
  const [, body, mac] = parts;
  const expected = Buffer.from(sign(body, secret), 'base64url');
  const presented = Buffer.from(mac, 'base64url');
  if (presented.length !== expected.length || !timingSafeEqual(presented, expected)) return { ok: false, reason: 'bad_signature' };
  let claims: FileTicketClaims;
  try {
    claims = JSON.parse(Buffer.from(body, 'base64url').toString('utf8')) as FileTicketClaims;
  } catch {
    return { ok: false, reason: 'malformed' };
  }
  if (!claims || typeof claims !== 'object' || !shapeOk(claims)) return { ok: false, reason: 'malformed' };
  if (claims.aud !== FILE_TICKET_AUDIENCE) return { ok: false, reason: 'wrong_audience' };
  const t = Math.floor(now.getTime() / 1000);
  if (claims.iat > t + SKEW_SECONDS) return { ok: false, reason: 'not_yet_valid' };
  if (claims.exp + SKEW_SECONDS <= t) return { ok: false, reason: 'expired' };
  return { ok: true, claims };
}

/** The ticket from `Authorization: Ticket <t>` or, for a plain link (downloads), `?t=<t>`. */
export function presentedFileTicket(authorization: string | undefined, url: string | undefined): string | null {
  if (authorization?.startsWith('Ticket ')) return authorization.slice(7).trim();
  const q = (url ?? '').split('?')[1];
  if (!q) return null;
  return new URLSearchParams(q).get('t');
}
