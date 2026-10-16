/**
 * Attach tickets: how a browser, PWA or `tmuxedo` client earns a gateway socket.
 *
 * WHY A TICKET AND NOT THE SESSION COOKIE. The gateway is the one process
 * that sees terminal bytes for every machine. Teaching it the session table,
 * the token pepper and the passkey step-up rules would put the most exposed
 * process in charge of the most sensitive decision. Instead web makes the
 * decision (who you are, whether you stepped up, which machine you may
 * reach) and hands the client a short-lived, signed statement of the
 * outcome. The gateway only checks the signature, the audience and the
 * clock. It never touches Postgres to admit a client.
 *
 *     t1.<base64url(json claims)>.<base64url(hmac-sha256)>
 *
 * Claims are deliberately few. `sub` is the user, `sid` the web session or
 * API token it came from (so revoking that can be honoured on reconnect),
 * `mid` the one machine the ticket opens (absent = machine list and events
 * only), `scope` what the holder may do, `exp` a hard end. Tickets live for
 * a minute: long enough to open a socket, too short to be worth stealing.
 * A reconnecting client asks web for a fresh one.
 *
 * NO I/O. Pure functions, so web, gateway and tests share one implementation.
 */
import { createHmac, timingSafeEqual } from 'node:crypto';

export const TICKET_VERSION = 't1';
/** The audience every newly minted ticket carries. */
export const TICKET_AUDIENCE = 'tmuxedo-gw';
/** Audience of tickets minted by a web pod from before the aimux → tmuxedo
 *  rename. Accepted so a rolling deploy (old web, new gateway) keeps working. */
export const LEGACY_TICKET_AUDIENCE = 'aimux-gw';
/** Every audience verifyTicket accepts. */
export const ACCEPTED_TICKET_AUDIENCES: readonly string[] = [TICKET_AUDIENCE, LEGACY_TICKET_AUDIENCE];
/** Default lifetime. Long enough to dial a socket; nothing more. */
export const TICKET_TTL_SECONDS = 60;
/** Clock skew tolerated between web and gateway pods. */
const SKEW_SECONDS = 5;
/** A secret shorter than this is a configuration mistake, not a key. */
const MIN_SECRET_BYTES = 32;

/** What a ticket lets its holder do on the socket. */
export type TicketScope = 'observe' | 'control';

export interface TicketClaims {
  /** User id. */
  sub: string;
  /** Web session or API token id the ticket was minted from. */
  sid: string;
  /** Machine the ticket opens; absent means list/events only. */
  mid?: string;
  /** `observe` reads panes; `control` may type, resize and approve. */
  scope: TicketScope;
  /**
   * Simulated machines (kind `demo`, App Store review accounts) this ticket
   * may wake: the gateway plays their runner side itself (gateway/src/demo.ts).
   * Absent for everyone else; at most DEMO_CLAIM_MAX ids.
   */
  demo?: string[];
  /**
   * The holder may use the file browser's reading methods (`fs.roots`,
   * `fs.list`, `fs.stat`, `fs.read`, `fs.search`) on this machine, whatever
   * `scope` says (docs/design/file-browser.md §3.3). Web sets it only for the
   * machine owner's browser session and for an API token with `read` AND
   * `terminal` (D-FB13); never for a glance ticket. Absent on tickets from
   * before the claim existed: the gateway then lets only `control` browse.
   */
  browse?: true;
  /**
   * A shared session (docs/design/sharing-and-integrations.md §7): the holder
   * is a guest the machine's owner shared ONE pane with. `id` is the share
   * (the gateway closes every socket of a share web revokes), `pane` the one
   * pane the socket may attach to, `owner` the machine's owner (the gateway's
   * ownership check uses it in place of `sub`), `until` when the share ends
   * (seconds since the epoch; the gateway closes the socket then). A guest
   * socket gets no machine list, topology, files, chat, stats or other pane;
   * `scope` `observe` never types. Never with `browse` or `demo`.
   */
  share?: ShareClaim;
  /** `tmuxedo-gw` when minted here; a verified legacy ticket may say `aimux-gw`. */
  aud: typeof TICKET_AUDIENCE | typeof LEGACY_TICKET_AUDIENCE;
  /** Issued-at and expiry, in whole seconds since the epoch. */
  iat: number;
  exp: number;
}

export interface ShareClaim { id: string; pane: string; owner: string; until: number }

/** The machine owner a ticket acts for: the share's owner for a guest, else the holder. */
export function ticketOwner(claims: Pick<TicketClaims, 'sub' | 'share'>): string {
  return claims.share ? claims.share.owner : claims.sub;
}

const SHARE_ID = /^[A-Za-z0-9_.:-]{1,255}$/;
function validShare(v: unknown): v is ShareClaim {
  if (!v || typeof v !== 'object' || Array.isArray(v)) return false;
  const o = v as Record<string, unknown>;
  return typeof o.id === 'string' && SHARE_ID.test(o.id) && typeof o.pane === 'string' && SHARE_ID.test(o.pane) &&
    typeof o.owner === 'string' && SHARE_ID.test(o.owner) && typeof o.until === 'number' && Number.isInteger(o.until) && o.until > 0;
}

export type TicketRefusal =
  | 'malformed'
  | 'bad_signature'
  | 'wrong_audience'
  | 'expired'
  | 'not_yet_valid';

export type TicketCheck = { ok: true; claims: TicketClaims } | { ok: false; reason: TicketRefusal };

function sign(body: string, secret: string): string {
  return createHmac('sha256', secret).update(`${TICKET_VERSION}.${body}`).digest('base64url');
}

function assertSecret(secret: string): void {
  if (Buffer.byteLength(secret, 'utf8') < MIN_SECRET_BYTES) {
    throw new Error(`gateway ticket secret must be at least ${MIN_SECRET_BYTES} bytes`);
  }
}

/** Most demo machine ids one ticket may name. */
export const DEMO_CLAIM_MAX = 4;

export interface MintInput {
  sub: string;
  sid: string;
  mid?: string;
  scope: TicketScope;
  demo?: string[];
  /** Sets the `browse` claim (only with `mid`). */
  browse?: boolean;
  /** Sets the `share` claim (only with `mid`; never with `browse` or `demo`). */
  share?: ShareClaim;
  ttlSeconds?: number;
  /** Injected for tests; defaults to the wall clock. */
  now?: Date;
}

export function mintTicket(input: MintInput, secret: string): string {
  assertSecret(secret);
  if (input.share && (!input.mid || input.browse || input.demo?.length || !validShare(input.share))) {
    throw new Error('a share ticket needs mid, a valid share claim, and no browse or demo');
  }
  const iat = Math.floor((input.now ?? new Date()).getTime() / 1000);
  const claims: TicketClaims = {
    sub: input.sub,
    sid: input.sid,
    ...(input.mid ? { mid: input.mid } : {}),
    scope: input.scope,
    ...(input.demo?.length ? { demo: input.demo.slice(0, DEMO_CLAIM_MAX) } : {}),
    ...(input.browse && input.mid ? { browse: true as const } : {}),
    ...(input.share ? { share: { id: input.share.id, pane: input.share.pane, owner: input.share.owner, until: input.share.until } } : {}),
    aud: TICKET_AUDIENCE,
    iat,
    exp: iat + (input.ttlSeconds ?? TICKET_TTL_SECONDS),
  };
  const body = Buffer.from(JSON.stringify(claims), 'utf8').toString('base64url');
  return `${TICKET_VERSION}.${body}.${sign(body, secret)}`;
}

/**
 * Check a presented ticket. NEVER THROWS on bad input: this runs on every
 * socket upgrade, and junk from the internet must become a refusal, not a 500.
 * The reason is for the log; the client is told only that it was refused.
 */
export function verifyTicket(raw: string | null | undefined, secret: string, now: Date = new Date()): TicketCheck {
  const parts = (raw ?? '').trim().split('.');
  if (parts.length !== 3 || parts[0] !== TICKET_VERSION || !parts[1] || !parts[2]) {
    return { ok: false, reason: 'malformed' };
  }
  const [, body, mac] = parts;
  const expected = Buffer.from(sign(body, secret), 'base64url');
  let presented: Buffer;
  try {
    presented = Buffer.from(mac, 'base64url');
  } catch {
    return { ok: false, reason: 'malformed' };
  }
  if (presented.length !== expected.length || !timingSafeEqual(presented, expected)) {
    return { ok: false, reason: 'bad_signature' };
  }
  let claims: TicketClaims;
  try {
    claims = JSON.parse(Buffer.from(body, 'base64url').toString('utf8')) as TicketClaims;
  } catch {
    return { ok: false, reason: 'malformed' };
  }
  if (
    typeof claims.sub !== 'string' || !claims.sub ||
    typeof claims.sid !== 'string' || !claims.sid ||
    (claims.scope !== 'observe' && claims.scope !== 'control') ||
    typeof claims.iat !== 'number' || typeof claims.exp !== 'number' ||
    (claims.mid !== undefined && (typeof claims.mid !== 'string' || !claims.mid)) ||
    (claims.demo !== undefined && (!Array.isArray(claims.demo) || claims.demo.length > DEMO_CLAIM_MAX ||
      !claims.demo.every((d) => typeof d === 'string' && d.length > 0 && d.length <= 128))) ||
    (claims.browse !== undefined && (claims.browse !== true || !claims.mid)) ||
    (claims.share !== undefined && (!validShare(claims.share) || !claims.mid || claims.browse !== undefined || claims.demo !== undefined))
  ) {
    return { ok: false, reason: 'malformed' };
  }
  if (!ACCEPTED_TICKET_AUDIENCES.includes(claims.aud)) return { ok: false, reason: 'wrong_audience' };
  const t = Math.floor(now.getTime() / 1000);
  if (claims.iat > t + SKEW_SECONDS) return { ok: false, reason: 'not_yet_valid' };
  if (claims.exp + SKEW_SECONDS <= t) return { ok: false, reason: 'expired' };
  // A share that has ended opens nothing, however fresh the ticket.
  if (claims.share && claims.share.until <= t) return { ok: false, reason: 'expired' };
  return { ok: true, claims };
}
