/**
 * Login callback relay (docs/protocol.md "Login callback relay").
 *
 * A CLI's browser login (Claude Code's `/login`, `codex login`) listens on
 * the machine's `localhost:<port>` for the OAuth redirect, which the user's
 * device cannot reach. The runner marks such `open_url` events with
 * `callback {host, port, path, state}`; the user pastes the address their
 * browser ended on and a client sends `login_callback {paneId, url}`; the
 * runner delivers it once, inside the machine.
 *
 * These helpers apply the SAME rules as the runner (runner/crates/runner/src/
 * login_relay.rs), checked against packages/protocol/fixtures/login-callback.json
 * from both sides, so a client can tell the user "that is not the right
 * address" before asking the machine. The runner checks again regardless:
 * pending, single use, expiry, listening port and rate limit are its alone.
 *
 * Dependency-free and value-only (no imports), so the app's Jest and the
 * web bundle can map this file alone.
 */

/** A loopback OAuth redirect target, from an authorize URL's own query. */
export interface LoginCallback {
  /** `localhost` or `127.0.0.1` (lowercase), exactly as in `redirect_uri`. */
  host: string;
  port: number;
  /** The redirect path without query, e.g. `/callback`, `/auth/callback`. */
  path: string;
  /** The authorize URL's `state`; the pasted URL must carry the same. */
  state: string;
}

/** `res.result` of a delivered `login_callback`: what the CLI's server answered. */
export interface LoginCallbackResult {
  /** 2xx or 3xx: the CLI took the code. */
  ok: boolean;
  status: number;
  /** Errors only: a short, masked excerpt of the server's body. */
  excerpt?: string;
}

/** Every `res.error.code` a `login_callback` can get (runner refusals, then gateway/transport ones). */
export const LOGIN_CALLBACK_REFUSALS = [
  'invalid_url', 'no_pending', 'mismatch', 'state_mismatch', 'expired', 'already_used', 'not_listening', 'rate_limited',
  'not_supported', 'unreachable', 'timeout', 'machine_offline',
] as const;
export type LoginCallbackRefusal = (typeof LOGIN_CALLBACK_REFUSALS)[number];

/** The hello feature a runner lists when it serves `login_callback`. */
export const LOGIN_CALLBACK_FEATURE = 'login_callback';
/** Longest URL a `login_callback` may carry (as `open_url`). */
export const LOGIN_CALLBACK_MAX_URL = 8192;

const MAX_STATE = 512;
const MAX_PATH = 1024;

function decode(s: string): string | null {
  // %XX and + (form encoding); null for broken escapes or invalid UTF-8.
  if (/%(?![0-9A-Fa-f]{2})/.test(s)) return null;
  try {
    return decodeURIComponent(s.replace(/\+/g, ' '));
  } catch {
    return null;
  }
}

function queryParam(query: string, name: string): string | null {
  for (const kv of query.split('&')) {
    const i = kv.indexOf('=');
    const k = decode(i < 0 ? kv : kv.slice(0, i));
    if (k !== name) continue;
    const v = decode(i < 0 ? '' : kv.slice(i + 1));
    if (v !== null) return v;
  }
  return null;
}

interface Parts { scheme: string; host: string; port: number; path: string; query: string }

function split(url: string): Parts | null {
  if (typeof url !== 'string' || !url || url.length > LOGIN_CALLBACK_MAX_URL || /[\s\x00-\x1f\x7f]/.test(url)) return null;
  const sep = url.indexOf('://');
  if (sep < 0) return null;
  const scheme = url.slice(0, sep).toLowerCase();
  const dflt = scheme === 'http' ? 80 : scheme === 'https' ? 443 : 0;
  if (!dflt) return null;
  const rest = url.slice(sep + 3).split('#')[0]!;
  const m = rest.search(/[/?]/);
  const end = m < 0 ? rest.length : m;
  const authority = rest.slice(0, end);
  const tail = rest.slice(end);
  if (!authority || authority.includes('@')) return null;
  let host = authority;
  let port = dflt;
  const c = authority.lastIndexOf(':');
  if (c >= 0) {
    const h = authority.slice(0, c);
    if (!h.includes(']') || h.endsWith(']')) {
      const p = authority.slice(c + 1);
      if (!/^\d{1,5}$/.test(p)) return null;
      const n = Number(p);
      if (n === 0 || n > 65535) return null;
      host = h;
      port = n;
    }
  }
  if (!host) return null;
  const q = tail.indexOf('?');
  const path = q < 0 ? tail : tail.slice(0, q);
  return { scheme, host: host.toLowerCase(), port, path: path || '/', query: q < 0 ? '' : tail.slice(q + 1) };
}

const loopback = (h: string) => h === 'localhost' || h === '127.0.0.1';

/**
 * The loopback callback an OAuth/OIDC authorize URL asks for, or null: a
 * `redirect_uri` of `http://localhost:<port>/…` or `http://127.0.0.1:<port>/…`
 * (explicit port, no userinfo), a non-empty `state`, and `response_type` or
 * `client_id`.
 */
export function detectLoginCallback(url: string): LoginCallback | null {
  const u = split(url);
  if (!u) return null;
  const redirect = queryParam(u.query, 'redirect_uri');
  const state = queryParam(u.query, 'state');
  if (redirect === null || !state || state.length > MAX_STATE) return null;
  if (queryParam(u.query, 'response_type') === null && queryParam(u.query, 'client_id') === null) return null;
  const s = redirect.indexOf('://');
  if (s < 0 || !redirect.slice(s + 3).split(/[/?#]/)[0]!.includes(':')) return null;
  const r = split(redirect);
  if (!r || r.scheme !== 'http' || !loopback(r.host) || !r.path.startsWith('/') || r.path.length > MAX_PATH) return null;
  return { host: r.host, port: r.port, path: r.path, state };
}

/**
 * Does `pasted` (the address the browser ended on) belong to `cb`? `ok`, or
 * the runner's refusal code for it (`invalid_url`, `mismatch`, `state_mismatch`).
 */
export function matchLoginCallback(cb: LoginCallback, pasted: string): 'ok' | 'invalid_url' | 'mismatch' | 'state_mismatch' {
  const u = split(typeof pasted === 'string' ? pasted.trim() : '');
  if (!u) return 'invalid_url';
  if (u.scheme !== 'http' || u.host !== cb.host || u.port !== cb.port || u.path !== cb.path) return 'mismatch';
  return queryParam(u.query, 'state') === cb.state ? 'ok' : 'state_mismatch';
}

/** A valid `callback` from an `open_url` event's data, or null (junk is dropped, never thrown). */
export function parseLoginCallback(v: unknown): LoginCallback | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  if (o.host !== 'localhost' && o.host !== '127.0.0.1') return null;
  if (typeof o.port !== 'number' || !Number.isInteger(o.port) || o.port < 1 || o.port > 65535) return null;
  if (typeof o.path !== 'string' || !o.path.startsWith('/') || o.path.length > MAX_PATH) return null;
  if (typeof o.state !== 'string' || !o.state || o.state.length > MAX_STATE) return null;
  return { host: o.host, port: o.port, path: o.path, state: o.state };
}

/** The host of a URL for audit and logs (never the port, path or query): `localhost`, or `?`. */
export function loginCallbackHost(url: unknown): string {
  const u = split(typeof url === 'string' ? url : '');
  return u ? u.host.slice(0, 255) : '?';
}
