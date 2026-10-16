/**
 * Runner tokens: prefix `tmr` (tokens minted before the tmuxedo rename carry
 * `amr` and are still accepted); a token identifies one Machine.
 *
 * Minting and checking a runner credential. NO DATABASE, NO I/O.
 *
 * This is the same shape any website uses for an API key: the app issues
 * it, the app checks it, the app revokes it. It deliberately replaces the
 * earlier arrangement where a token lived in a secrets store and reached the
 * host through the cluster's secret sync — that made issuing a credential an
 * infrastructure operation across three systems, when a runner token is
 * not an infrastructure secret at all.
 *
 * ═══════════════════ THE TOKEN HAS TWO HALVES ═══════════════════
 *
 *     tmr.<lookup>.<secret>      (legacy: amr.<lookup>.<secret>)
 *
 * `lookup` is PUBLIC. It is stored in clear, it is unique, and it is how
 * verification finds exactly one row. `secret` is never stored — only its
 * SHA-256.
 *
 * WHY TWO HALVES RATHER THAN ONE OPAQUE STRING. With one, checking a
 * token means comparing it against every hash in the table: a linear scan
 * over secrets, which is both slow and the classic way a timing leak gets
 * built. With a lookup, it is one indexed read and one comparison.
 *
 * ═════════════ SHA-256, NOT BCRYPT — AND THAT IS ON PURPOSE ═════════════
 *
 * Slow KDFs exist to make brute force expensive against LOW-ENTROPY
 * secrets: passwords people chose. This secret is 256 bits from a CSPRNG.
 * There is nothing to brute force, and a deliberately slow hash would add
 * its cost to every lease poll — which a runner makes every few seconds,
 * for ever.
 *
 * The comparison is still constant-time. Not because the hash is weak,
 * but because a `===` on a secret is a habit worth not having.
 */
import { createHash, randomBytes, timingSafeEqual } from 'node:crypto';

/** The prefix every newly minted token carries. */
export const TOKEN_PREFIX = 'tmr';
/** Prefix of tokens minted before the aimux → tmuxedo rename. Still accepted
 *  ("forever-ish"): runners in the field hold them in their config. */
export const LEGACY_TOKEN_PREFIX = 'amr';
/** Every prefix parseToken accepts. */
export const ACCEPTED_TOKEN_PREFIXES: readonly string[] = [TOKEN_PREFIX, LEGACY_TOKEN_PREFIX];

/**
 * The separator. A DOT, NOT AN UNDERSCORE, and this is not cosmetic.
 *
 * The halves are base64url, whose alphabet is `A-Za-z0-9-_`. It INCLUDES
 * the underscore, so `tmr_<lookup>_<secret>` splits into four parts
 * whenever a random half happens to contain one — roughly one token in
 * nine, at these lengths. The first version did exactly that and the
 * tests caught it: a credential that fails to parse for one issue in nine
 * is far worse than one that never works, because it looks fine until it
 * does not.
 *
 * A dot cannot appear in base64url, so the split is unambiguous for every
 * token that will ever be minted.
 */
export const TOKEN_SEP = '.';

/** Bytes in each half. 16 → 128 bits of lookup, 32 → 256 bits of secret. */
const LOOKUP_BYTES = 16;
const SECRET_BYTES = 32;

export interface MintedToken {
  /** The ONLY time the plaintext exists. Show it once; never store it. */
  plaintext: string;
  lookup: string;
  secretHash: string;
  /** First 6 characters of the secret, for "which one is this?" in a
   *  list. Short enough to be useless on its own. */
  hint: string;
}

export function mintToken(): MintedToken {
  const lookup = randomBytes(LOOKUP_BYTES).toString('base64url');
  const secret = randomBytes(SECRET_BYTES).toString('base64url');
  return {
    plaintext: [TOKEN_PREFIX, lookup, secret].join(TOKEN_SEP),
    lookup,
    secretHash: hashSecret(secret),
    hint: secret.slice(0, 6),
  };
}

export function hashSecret(secret: string): string {
  return createHash('sha256').update(secret, 'utf8').digest('hex');
}

export interface ParsedToken {
  lookup: string;
  secret: string;
}

/**
 * Split a presented token, or null if it is not one of ours.
 *
 * RETURNS null RATHER THAN THROWING. This runs on every request including
 * unauthenticated ones, and a throw would turn "somebody sent junk" into
 * a 500 and an alert.
 */
export function parseToken(raw: string | null | undefined): ParsedToken | null {
  const s = (raw ?? '').trim();
  if (!s) return null;
  const parts = s.split(TOKEN_SEP);
  if (parts.length !== 3) return null;
  const [prefix, lookup, secret] = parts;
  if (!ACCEPTED_TOKEN_PREFIXES.includes(prefix) || !lookup || !secret) return null;
  // Length-checked so a malformed value is refused before it reaches a
  // database lookup — the cheapest possible rejection of noise.
  if (lookup.length < 20 || secret.length < 40) return null;
  return { lookup, secret };
}

/** Constant-time check of a presented secret against a stored hash. */
export function secretMatches(secret: string, storedHash: string): boolean {
  const a = Buffer.from(hashSecret(secret), 'hex');
  let b: Buffer;
  try {
    b = Buffer.from(storedHash, 'hex');
  } catch {
    return false;
  }
  if (a.length !== b.length || a.length === 0) return false;
  return timingSafeEqual(a, b);
}

export interface TokenRow {
  machineId: string;
  secretHash: string;
  revokedAt: Date | null;
}

export interface AuthOutcome {
  ok: boolean;
  machineId: string;
  /** Why it failed. Safe to log; NEVER returned to the caller verbatim,
   *  because "unknown token" and "revoked token" tell a prober which
   *  guesses were close. */
  reason: string;
}

/**
 * The decision, given a presented token and the row it names.
 *
 * A REVOKED TOKEN IS REFUSED AND SAYS SO — IN THE LOG. The response says
 * only "unrecognised", because distinguishing "no such token", "revoked"
 * and "wrong secret" to the caller hands a prober a map. The operator
 * reading the log needs the distinction; whoever is holding the token
 * does not.
 */
export function authenticate(
  presented: ParsedToken | null,
  row: TokenRow | null,
): AuthOutcome {
  if (!presented) return { ok: false, machineId: '', reason: 'not a runner token' };
  if (!row) return { ok: false, machineId: '', reason: 'no token with that lookup' };
  if (row.revokedAt) {
    return {
      ok: false,
      machineId: row.machineId,
      reason: `token for ${row.machineId} was revoked at ${row.revokedAt.toISOString()}`,
    };
  }
  if (!secretMatches(presented.secret, row.secretHash)) {
    // The lookup existed and the secret did not match. Worth seeing: it
    // means somebody has half a real credential.
    return {
      ok: false,
      machineId: row.machineId,
      reason: `secret mismatch for a known lookup (${row.machineId})`,
    };
  }
  return { ok: true, machineId: row.machineId, reason: 'ok' };
}

/**
 * How a token should be described in a list.
 *
 * NEVER USED IS ITS OWN STATE, and it is the one worth showing. A token
 * issued and never presented is a setup that did not finish, and it looks
 * exactly like a working one until somebody can see this.
 */
export function describeUse(lastUsedAt: Date | null, revokedAt: Date | null): string {
  if (revokedAt) return `revoked ${revokedAt.toISOString().slice(0, 16).replace('T', ' ')}`;
  if (!lastUsedAt) {
    return 'never used — issued, and no runner has ever presented it';
  }
  return `last used ${lastUsedAt.toISOString().slice(0, 16).replace('T', ' ')}`;
}
