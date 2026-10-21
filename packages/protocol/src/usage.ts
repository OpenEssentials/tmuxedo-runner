/**
 * AI usage (docs/protocol.md "AI usage"): how much of a plan's limits an
 * agent CLI's account has used, as that CLI itself reported it. Numbers,
 * reset times and an opaque account key only: no tokens, keys, prompts,
 * paths, account names or vendor JSON. The Rust side is
 * `runner/crates/protocol/src/usage.rs`; both check `fixtures/usage.json`.
 */

/** The hello feature a runner lists when it sends `usage`. */
export const USAGE_FEATURE = 'usage';
/** At most this many windows in one record. */
export const USAGE_WINDOWS_MAX = 8;
/** A runner sends at most one record per account in this many milliseconds. */
export const USAGE_MIN_INTERVAL_MS = 2000;
/** A runner `usage` frame larger than this is dropped (a real one is about 300 bytes). */
export const USAGE_MAX_BYTES = 4096;
/** A gateway keeps at most this many accounts per machine (the newest record of each). */
export const USAGE_ACCOUNTS_MAX = 16;
export const USAGE_SOURCES = ['statusline', 'jsonl'] as const;
export const USAGE_MODES = ['plan', 'metered'] as const;
export type UsageSource = (typeof USAGE_SOURCES)[number];
export type UsageMode = (typeof USAGE_MODES)[number];

/** One limit window: `id` is `5h`, `7d` or `<minutes>m`. `resetsAt` is Unix seconds. */
export interface UsageWindow {
  id: string;
  usedPct: number;
  resetsAt?: number;
  windowMins?: number;
}

/** How full one pane's context window is; `size` in tokens. */
export interface UsageContext {
  paneId: string;
  usedPct: number;
  size?: number;
}

/** The newest record for one account of one plugin on one machine. `observedAt` is Unix milliseconds. */
export interface UsageRecord {
  plugin: string;
  /** Opaque: `h:<hex>` (a salted hash of the account) or `m:<hex>` (the machine's own key). */
  accountKey: string;
  mode: UsageMode;
  plan?: string;
  windows: UsageWindow[];
  context?: UsageContext;
  /** The id of a window that is used up. */
  reached?: string;
  source: UsageSource;
  observedAt: number;
}

/** What a client receives: the record and the machine it was read on. */
export type UsageFrame = { t: 'usage'; machineId: string } & UsageRecord;

const PLUGIN = /^[a-z0-9-]{2,32}$/;
const KEY = /^[hm]:[0-9a-f]{16,64}$/;
const ID = /^[a-z0-9:_-]{1,24}$/;
const PLAN = /^[a-z0-9_-]{1,24}$/;
const PANE = /^[A-Za-z0-9_:-]{1,64}$/;

const obj = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);
const num = (v: unknown, min: number, max: number): v is number => typeof v === 'number' && Number.isFinite(v) && v >= min && v <= max;
const int = (v: unknown, min: number, max: number): v is number => num(v, min, max) && Number.isInteger(v);
const str = (v: unknown, re: RegExp): v is string => typeof v === 'string' && re.test(v);

/**
 * A runner's record, checked and rebuilt (unknown fields are dropped). Null
 * when any field present is not what it should be. Never throws.
 */
export function parseUsage(v: unknown): UsageRecord | null {
  if (!obj(v) || !str(v.plugin, PLUGIN) || !str(v.accountKey, KEY)) return null;
  if (!(USAGE_MODES as readonly unknown[]).includes(v.mode) || !(USAGE_SOURCES as readonly unknown[]).includes(v.source)) return null;
  if (!int(v.observedAt, 0, Number.MAX_SAFE_INTEGER)) return null;
  const ws = v.windows === undefined ? [] : v.windows;
  if (!Array.isArray(ws) || ws.length > USAGE_WINDOWS_MAX) return null;
  const out: UsageRecord = { plugin: v.plugin, accountKey: v.accountKey, mode: v.mode as UsageMode, windows: [], source: v.source as UsageSource, observedAt: v.observedAt };
  for (const w of ws) {
    if (!obj(w) || !str(w.id, ID) || !num(w.usedPct, 0, 100)) return null;
    const win: UsageWindow = { id: w.id, usedPct: w.usedPct };
    if (w.resetsAt !== undefined) { if (!int(w.resetsAt, 1, 99_999_999_999)) return null; win.resetsAt = w.resetsAt; }
    if (w.windowMins !== undefined) { if (!int(w.windowMins, 1, 527_040)) return null; win.windowMins = w.windowMins; }
    out.windows.push(win);
  }
  if (v.plan !== undefined) { if (!str(v.plan, PLAN)) return null; out.plan = v.plan; }
  if (v.context !== undefined) {
    const c = v.context;
    if (!obj(c) || !str(c.paneId, PANE) || !num(c.usedPct, 0, 100)) return null;
    out.context = { paneId: c.paneId, usedPct: c.usedPct };
    if (c.size !== undefined) { if (!int(c.size, 0, 1_000_000_000)) return null; out.context.size = c.size; }
  }
  if (v.reached !== undefined) { if (!str(v.reached, ID)) return null; out.reached = v.reached; }
  return out;
}

/** A gateway's `usage` frame as a client reads it, or null. */
export function parseUsageFrame(v: unknown): UsageFrame | null {
  if (!obj(v) || v.t !== 'usage' || typeof v.machineId !== 'string' || !v.machineId || v.machineId.length > 64) return null;
  const r = parseUsage(v);
  return r ? { t: 'usage', machineId: v.machineId, ...r } : null;
}

/** The key a holder of records files one under. */
export const usageKey = (r: Pick<UsageRecord, 'plugin' | 'accountKey'>): string => `${r.plugin}\n${r.accountKey}`;

/**
 * What one session used (`session.summary`'s `usage`; runner/crates/protocol/src/usage.rs
 * `SessionUsage`): per limit window, the rise in the account's percentage that the session's own
 * readings showed (an estimate: other use of the same account in between counts too), and the
 * tokens its agent reported. Never empty.
 */
export interface SpentWindow { id: string; spentPct: number; windowMins?: number }
export interface SessionUsage { plugin: string; windows: SpentWindow[]; tokens?: number }
export const SESSION_TOKENS_MAX = 1_000_000_000_000;

/** A `session.summary`'s `usage`, checked and rebuilt, or null. Never throws. */
export function parseSessionUsage(v: unknown): SessionUsage | null {
  if (!obj(v) || !str(v.plugin, PLUGIN)) return null;
  const ws = v.windows === undefined ? [] : v.windows;
  if (!Array.isArray(ws) || ws.length > USAGE_WINDOWS_MAX) return null;
  const out: SessionUsage = { plugin: v.plugin, windows: [] };
  for (const w of ws) {
    if (!obj(w) || !str(w.id, ID) || !num(w.spentPct, 0, 100_000) || w.spentPct === 0) return null;
    const sw: SpentWindow = { id: w.id, spentPct: w.spentPct };
    if (w.windowMins !== undefined) { if (!int(w.windowMins, 1, 527_040)) return null; sw.windowMins = w.windowMins; }
    out.windows.push(sw);
  }
  if (v.tokens !== undefined) { if (!int(v.tokens, 1, SESSION_TOKENS_MAX)) return null; out.tokens = v.tokens; }
  return out.windows.length || out.tokens !== undefined ? out : null;
}

