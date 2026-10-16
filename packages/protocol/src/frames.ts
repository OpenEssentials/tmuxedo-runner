/**
 * Relay frames (docs/protocol.md). Binary data frames have a fixed header so
 * the gateway can route without reading terminal bytes; control frames are
 * JSON. Both directions are parsed by functions that NEVER THROW: frames come
 * from the internet and from machines we do not control, so junk becomes
 * `null` (binary) or `{ok:false}` (JSON) and the caller counts it.
 *
 * Golden fixtures in ../fixtures are produced by `encodeData` and checked
 * byte-for-byte by the Rust runner's tests, so the two implementations cannot
 * drift silently.
 */
// Type-only (erased): the app's Jest and esbuild map this file alone, so it must not import values.
import type { GlanceCrop, GlanceFrame } from './glance.js';
import type { PluginInfo, UiItem } from './plugins.js';
import type { NeedsYou } from './needs.js';
import type { StatsFrame, StatsSample } from './stats.js';
import type { UsageFrame, UsageRecord } from './usage.js';

export const PROTOCOL_VERSION = 1;
/** The WebSocket subprotocol clients offer first and the gateway selects when offered. */
export const SUBPROTOCOL = 'tmuxedo.v1';
/** The pre-rename subprotocol. Still accepted ("forever-ish"): a client that
 *  offers only this is served, and the gateway selects it back. */
export const LEGACY_SUBPROTOCOL = 'aimux.v1';
/** Every protocol subprotocol, in preference order (what clients offer). */
export const SUBPROTOCOLS: readonly string[] = [SUBPROTOCOL, LEGACY_SUBPROTOCOL];
export const MAX_FRAME_BYTES = 1024 * 1024;
/**
 * A runner's JSON text frames may be larger (a `res` to a big read); the
 * gateway's runner socket accepts this much and answers a `res` over 4 MiB to
 * its caller as `too_large` (docs/protocol.md). Binary frames stay at 1 MiB.
 */
export const MAX_RUNNER_TEXT_BYTES = 16 * 1024 * 1024;

/**
 * Binary frame kinds. CHAT (agent chat, chat.ts) reuses the header with the
 * chat session id in the id field and the event's seq as the offset; the
 * payload is one event as JSON, opaque to the gateway.
 */
export const DataKind = { OUTPUT: 1, INPUT: 2, SNAPSHOT: 3, CHAT: 4 } as const;
export type DataKind = (typeof DataKind)[keyof typeof DataKind];
export const FLAG_GAP = 0x01;
/** CHAT only: a token delta, not stored, with no seq of its own (offset = the last durable seq). */
export const FLAG_EPHEMERAL = 0x02;
/** A chat session id (runner `chat.*`): 8..64 of [A-Za-z0-9_-]. */
export const CHAT_SESSION_ID = /^[A-Za-z0-9_-]{8,64}$/;
/** The hello feature of a runner that serves agent chat (docs/protocol.md "Agent chat"). */
export const CHAT_FEATURE = 'chat';

// ── glance limits (docs/protocol.md "Glances"; decoding in glance.ts) ──
/** Glance rate bounds and default (updates per second, per pane). */
export const GLANCE_HZ_MIN = 0.1;
export const GLANCE_HZ_MAX = 1;
export const GLANCE_HZ_DEFAULT = 0.5;
/** Most panes one client socket may glance at once. */
export const GLANCE_MAX_PANES = 64;
/** A runner glance frame larger than this is dropped by the gateway (the runner keeps them far below it). */
export const GLANCE_MAX_BYTES = 128 * 1024;
/** The hello feature a runner lists when it serves glances. */
export const GLANCE_FEATURE = 'glance';

// ── machine stats (docs/protocol.md "Machine stats"; samples in stats.ts) ──
/** The hello feature a runner lists when it answers `stats`. */
export const STATS_FEATURE = 'stats';
/** Sample rate bounds (samples per second); the default is the fastest. */
export const STATS_HZ_MIN = 0.1;
export const STATS_HZ_MAX = 0.5;
/** A runner `stats` frame larger than this is dropped by the gateway (a real one is about 250 bytes). */
export const STATS_MAX_BYTES = 4096;
/** Clamp a requested stats rate (missing or not a number: the fastest). */
export function clampStatsHz(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) ? Math.min(STATS_HZ_MAX, Math.max(STATS_HZ_MIN, v)) : STATS_HZ_MAX;
}

/** The most rows and columns a client's crop may ask (glance.ts GLANCE_MAX_ROWS and GLANCE_MAX_COLS; a test holds them equal). */
export const GLANCE_CROP_MAX_ROWS = 60;
export const GLANCE_CROP_MAX_COLS = 200;

/**
 * A client's `rows` and `cols` on `glance`. Both left out: no crop (null).
 * Both whole numbers of at least 1: the crop, each clamped to the maximum.
 * Anything else (one without the other, a fraction, 0, a negative, a string): `false`.
 */
export function parseGlanceCrop(rows: unknown, cols: unknown): GlanceCrop | null | false {
  if (rows === undefined && cols === undefined) return null;
  const ok = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 1;
  if (!ok(rows) || !ok(cols)) return false;
  return { rows: Math.min(rows, GLANCE_CROP_MAX_ROWS), cols: Math.min(cols, GLANCE_CROP_MAX_COLS) };
}

/** Clamp a requested glance rate. */
export function clampHz(hz: unknown): number {
  if (typeof hz !== 'number' || !Number.isFinite(hz)) return GLANCE_HZ_DEFAULT;
  return Math.min(GLANCE_HZ_MAX, Math.max(GLANCE_HZ_MIN, hz));
}

export interface DataFrame {
  kind: DataKind;
  /** Bytes before this frame were lost (the runner's ring wrapped). */
  gap: boolean;
  /** 0 = live broadcast; otherwise the attach request this answers. */
  req: number;
  paneId: string;
  /** Stream offset of the first payload byte (SNAPSHOT: where live output resumes; CHAT: the event's seq). */
  offset: bigint;
  payload: Uint8Array;
  /** CHAT only (FLAG_EPHEMERAL). */
  ephemeral?: boolean;
}

const HEADER_FIXED = 1 + 1 + 4 + 1 + 8;

export function encodeData(f: DataFrame): Uint8Array {
  const id = new TextEncoder().encode(f.paneId);
  if (id.length < 1 || id.length > 255) throw new RangeError('paneId must be 1..255 UTF-8 bytes');
  if (f.req < 0 || f.req > 0xffffffff || !Number.isInteger(f.req)) throw new RangeError('req must be a u32');
  if (f.offset < 0n || f.offset > 0xffffffffffffffffn) throw new RangeError('offset must be a u64');
  if (f.ephemeral && f.kind !== DataKind.CHAT) throw new RangeError('only a CHAT frame can be ephemeral');
  const out = new Uint8Array(HEADER_FIXED + id.length + f.payload.length);
  if (out.length > MAX_FRAME_BYTES) throw new RangeError('frame exceeds MAX_FRAME_BYTES');
  const v = new DataView(out.buffer);
  v.setUint8(0, f.kind);
  v.setUint8(1, (f.gap ? FLAG_GAP : 0) | (f.ephemeral ? FLAG_EPHEMERAL : 0));
  v.setUint32(2, f.req);
  v.setUint8(6, id.length);
  out.set(id, 7);
  v.setBigUint64(7 + id.length, f.offset);
  out.set(f.payload, HEADER_FIXED + id.length);
  return out;
}

/** Parse a binary frame, or null if it is not a valid v1 data frame. */
export function decodeData(buf: Uint8Array): DataFrame | null {
  if (buf.length < HEADER_FIXED + 1 || buf.length > MAX_FRAME_BYTES) return null;
  const v = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const kind = v.getUint8(0);
  if (kind !== DataKind.OUTPUT && kind !== DataKind.INPUT && kind !== DataKind.SNAPSHOT && kind !== DataKind.CHAT) return null;
  const flags = v.getUint8(1);
  if (flags & ~(kind === DataKind.CHAT ? FLAG_GAP | FLAG_EPHEMERAL : FLAG_GAP)) return null;
  const idLen = v.getUint8(6);
  if (idLen < 1 || buf.length < HEADER_FIXED + idLen) return null;
  let paneId: string;
  try {
    paneId = new TextDecoder('utf-8', { fatal: true }).decode(buf.subarray(7, 7 + idLen));
  } catch {
    return null;
  }
  return {
    kind: kind as DataKind,
    gap: (flags & FLAG_GAP) !== 0,
    req: v.getUint32(2),
    paneId,
    offset: v.getBigUint64(7 + idLen),
    payload: buf.subarray(HEADER_FIXED + idLen),
    ...(kind === DataKind.CHAT ? { ephemeral: (flags & FLAG_EPHEMERAL) !== 0 } : {}),
  };
}

/** Rewrite only the `req` field in place (the gateway's one mutation). */
export function setReq(buf: Uint8Array, req: number): void {
  new DataView(buf.buffer, buf.byteOffset, buf.byteLength).setUint32(2, req);
}

// ── JSON control frames ──────────────────────────────────────────────────

export type AgentStatus = 'working' | 'blocked' | 'done' | 'idle' | 'unknown';
export interface Agent {
  kind: string;
  status: AgentStatus;
  since: string;
  source: 'reported' | 'screen' | 'heuristic';
  sessionId?: string;
  promptId?: string;
  prompt?: { tool: string; input: unknown; suggestions?: unknown };
  summary?: string;
  /**
   * Agent chat (docs/protocol.md "Agent chat"): present when this agent session can open as a chat
   * (its plugin has a chat mode and the login may use it); `id` = the chat that already holds the
   * session (its history). Absent from runners without the `chat_offer` feature.
   */
  chat?: AgentChat;
  /** In-process sub-agents, "helpers" (agents.ts `parseAgentChildren`); absent from older runners and when there are none. */
  children?: import('./agents.js').AgentChild[];
}
export interface AgentChat { id?: string }
/** The hello feature of a runner that sends `Agent.chat` and keeps chats bound to a pane's agent session. */
export const FEATURE_CHAT_OFFER = 'chat_offer';
export type Layout = { pane: string } | { dir: 'h' | 'v'; ratio: number; a: Layout; b: Layout };
/** `activeTab` / `focusedPane` (and topology `focusedWorkspace`): shared focus, absent from older runners. */
export interface Workspace { id: string; name: string; cwd: string; position: number; activeTab?: string }
export interface Tab { id: string; workspaceId: string; name: string; position: number; layout: Layout; focusedPane?: string }
export interface Pane {
  id: string; tabId: string; cwd: string; command: string; title: string; cols: number; rows: number;
  exited?: { code: number | null; at: string }; agent: Agent | null;
  /** The name a person gave the pane (names.ts rules); absent when none, and from runners without the `names` feature. */
  label?: string;
}
export type ClaudeAuthState = 'ok' | 'missing' | 'expired' | 'failed';
/** Claude Code login health (docs/protocol.md "Claude auth health"); absent when the machine has no Claude Code. */
export interface ClaudeAuth { state: ClaudeAuthState; reason: string; since: string }
/** Durable event types that carry a Claude auth transition (`data` {state, reason, source}). */
export const AUTH_EVENT_TYPES = ['auth_ok', 'auth_missing', 'auth_expired', 'auth_failed'] as const;
/** `features`: optional protocol additions this runner serves (e.g. `glance`); absent on older runners. */
/** `plugins`: the runner's plugins (plugins.ts); a runner that sends it understands `plugin_action` and sends `ui`. */
export interface RunnerInfo { version: string; holderVersion: string; os: string; arch: string; hostname: string; claudeVersion?: string; claudeAuth?: ClaudeAuth; features?: string[]; plugins?: PluginInfo[]; /** Automation the machine's owner turned on (tasks.ts); absent = off. */ automation?: { labels: string[] } }
/** The welcome feature a gateway lists when it relays `ui` frames (a runner sends none before it). */
export const UI_FEATURE = 'ui';

/** Whether a runner's hello lists a feature (older runners list none). */
export function hasFeature(hello: { features?: unknown } | null | undefined, feature: string): boolean {
  return Array.isArray(hello?.features) && hello.features.includes(feature);
}

/** A valid ClaudeAuth or undefined (junk is dropped, never thrown). */
export function parseClaudeAuth(v: unknown): ClaudeAuth | undefined {
  if (!v || typeof v !== 'object') return undefined;
  const o = v as Record<string, unknown>;
  if (o.state !== 'ok' && o.state !== 'missing' && o.state !== 'expired' && o.state !== 'failed') return undefined;
  return { state: o.state, reason: typeof o.reason === 'string' ? o.reason.slice(0, 256) : '', since: typeof o.since === 'string' ? o.since.slice(0, 64) : '' };
}

export type RunnerToGateway =
  | { t: 'hello'; v: number; runner: RunnerInfo }
  | { t: 'topology'; rev: number; workspaces: Workspace[]; tabs: Tab[]; panes: Pane[]; focusedWorkspace?: string }
  | { t: 'status'; paneId: string; agent: Agent | null }
  /** `paneId` is "" for machine-level events (e.g. the credentials check's auth_*). */
  | { t: 'event'; seq: number; paneId: string; type: string; at: string; data: unknown }
  | { t: 'res'; id: string; result?: unknown; error?: { code: string; message?: string } }
  | { t: 'size'; paneId: string; cols: number; rows: number }
  | GlanceFrame
  /** One machine stats sample (stats.ts); only while the last `stats` frame asked for a rate above 0. */
  | ({ t: 'stats' } & StatsSample)
  /** The newest AI usage record of one account (usage.ts): on change, at most one per account every two seconds. */
  | ({ t: 'usage' } & UsageRecord)
  /** Answer to `challenge` (docs/protocol.md "Runner proof"): a signature by the token's key, base64url. */
  | { t: 'proof'; sig: string }
  /** After the replayed CHAT frames of `chat_attach {req}`: ok with the last seq, or why not (`chat_not_found`). */
  | { t: 'chat_attached'; req: number; sessionId: string; ok: boolean; lastSeq: number; code?: string }
  /** A plugin's declared surfaces (plugins.ts); replaces the set for (pluginId, scope, paneId). Only after a welcome listing `ui`. */
  | { t: 'ui'; pluginId: string; scope: 'machine' | 'pane'; paneId?: string; items: UiItem[] }
  /** The runner's X25519 public key (docs/protocol.md "Secrets"); answered with `secret_res` and a `secrets` frame. */
  | { t: 'secret_key'; id: string; alg: string; publicKey: string }
  /** Values for a use (`purpose`); answered with `secret_res {sealed}` (ciphertext only). */
  | { t: 'secret_req'; id: string; names: string[]; purpose: Record<string, unknown> }
  /** Files (files.ts): chunk `n` of an upload was written and its SHA-256 checked, or why not. */
  | { t: 'file_ack'; uploadId: string; n: number; ok: boolean; sha256?: string; code?: string; message?: string }
  /** The home disk (files.ts DiskState): after hello, and when the level or the numbers change much. */
  | { t: 'disk'; freeBytes: number; totalBytes: number; filesBytes: number; level: 'ok' | 'low' | 'critical' | 'alert' }
  /** A pane's open needs-you items (needs.ts); replaces the set for the pane ("" = machine-level). Only after a welcome listing `needs`. */
  | { t: 'needs'; paneId: string; items: NeedsYou[] }
  | { t: 'ping' | 'pong'; at: number };

export type GatewayToRunner =
  /** Key-bound tokens only, before `welcome`: sign `tmuxedo-runner-proof-v1\n<nonce>\n<lookup>` (runner-proof.ts). */
  | { t: 'challenge'; nonce: string }
  /** `features`: what this gateway relays (`ui`); absent from older gateways. */
  | { t: 'welcome'; machineId: string; userId: string; lastEventSeq: number; features?: string[] }
  | { t: 'events_ack'; upTo: number }
  /** Seqs web refused because it already holds a different event under them: renumber above lastEventSeq and resend. */
  | { t: 'events_rejected'; seqs: number[]; lastEventSeq: number }
  /** Launch and shell configuration from web (docs/protocol.md `config`): after welcome and on change. */
  | { t: 'config'; claude?: { args: string[] }; /** Per plugin (plugins.ts ConfigPlugin); sent beside `claude` (dual-write). */ plugins?: Array<{ id: string; settings?: { launch?: { args: string[] }; skill?: { enabled: boolean }; keystroke?: { enabled: boolean }; usage?: { enabled: boolean } } }>; shell?: { aliases: Record<string, string>; env: Record<string, string>; rc: string }; /** New panes' folder: `~`, `~/…` or absolute; "" = the default (~/work); absent = unchanged. */ workDir?: string; /** Web's agent chat policy for this machine's owner (D-C4); absent = unchanged. */ chat?: { subscription: 'allow' | 'deny' }; /** What each plugin gets on this machine, per project (plugin-set.ts; Phase 17). Absent = unchanged; an older runner ignores it. */ pluginSet?: import('./plugin-set.js').PluginSet; /** The roles effective on this machine, per project (roles.ts RoleSet; Phase 18). Absent = unchanged; an older runner ignores it. */ roles?: import('./roles.js').RoleSet }
  | { t: 'attach'; req: number; paneId: string; from?: number }
  | { t: 'detach'; paneId: string }
  | { t: 'req'; id: string; method: string; params: unknown }
  | { t: 'resize'; paneId: string; cols: number; rows: number }
  | { t: 'decision'; paneId: string; promptId: string; behavior: 'allow' | 'deny'; message?: string }
  /** The union of every client's glance set for this runner (empty = stop); only to runners with the `glance` feature. */
  | { t: 'glance'; paneIds: string[]; hz: number }
  /** Sample the machine `hz` times a second (the most any viewer asked); 0 = nobody is looking, stop. Only to runners with the `stats` feature. */
  | { t: 'stats'; hz: number }
  /** Deliver a pasted loopback OAuth callback URL (docs/protocol.md "Login callback relay"); answered with `res {id}`. Only to runners with the `login_callback` feature. */
  | { t: 'login_callback'; id: string; paneId: string; url: string }
  /** Agent chat (docs/protocol.md "Agent chat"): replay durable events from seq `from` (default 1) as CHAT frames tagged `req`, then `chat_attached`, then live. Only to runners with the `chat` feature. */
  | { t: 'chat_attach'; req: number; sessionId: string; from?: number }
  | { t: 'chat_detach'; sessionId: string }
  /** Files: chunk `n` of upload `uploadId` follows as FILE frames (kind 5) totalling `size` bytes; only to runners with the `files` feature. */
  | { t: 'file_put'; uploadId: string; n: number; size: number; sha256: string }
  /** The chunk's sender went away mid-chunk: forget what arrived of it. */
  | { t: 'file_put_cancel'; uploadId: string; n: number }
  | { t: 'ping' | 'pong'; at: number };

/** One pane a client wants glances of; `machineId` may be left out on a socket bound to one machine. */
export interface GlanceTarget { machineId?: string; paneId: string }

export type ClientToGateway =
  | { t: 'hello'; client: { kind: 'web' | 'pwa' | 'tui' | 'cli'; version: string } }
  | { t: 'attach'; paneId: string; from?: number }
  | { t: 'detach'; paneId: string }
  | { t: 'ack'; paneId: string; offset: number }
  | { t: 'resize'; paneId: string; cols: number; rows: number }
  | { t: 'req'; id: string; method: string; params: unknown }
  | { t: 'decision'; paneId: string; promptId: string; behavior: 'allow' | 'deny'; message?: string }
  /**
   * Replaces this socket's glance set (empty = none). `hz` is clamped to 0.1..1 (default 0.5) and is
   * also the most often this socket is sent one pane's glance. With `rows` and `cols` (both or
   * neither) the gateway crops every glance for this socket (glance.ts `cropGlance`).
   */
  | { t: 'glance'; panes: GlanceTarget[]; hz: number; rows?: number; cols?: number }
  /**
   * Machine stats for this socket's machine: `on: true` subscribes (again: changes the rate), `on: false`
   * unsubscribes. `hz` is clamped to 0.1..0.5 (default 0.5) and is the most often this socket is sent a sample.
   * Machine-bound sockets only; the machine's owner only.
   */
  | { t: 'stats'; on: boolean; hz: number }
  /** The address the user's browser ended on, for the pane's pending login (needs control); answered with `res {id}`. */
  | { t: 'login_callback'; id: string; paneId: string; url: string }
  /** Watch an agent chat from seq `from` (default 1): CHAT frames, then `chat_attached`, then live. Observe scope is enough. */
  | { t: 'chat_attach'; sessionId: string; from?: number }
  | { t: 'chat_detach'; sessionId: string }
  /** Run a plugin action the runner's manifest declares (control scope); answered with `res {id}`. */
  | { t: 'plugin_action'; id: string; pluginId: string; actionId: string; paneId?: string }
  | { t: 'ping' | 'pong'; at?: number };

export type GatewayToClient =
  | { t: 'welcome'; sub: string; mid: string | null; scope: 'observe' | 'control'; machine: { online: boolean; hello?: RunnerInfo } }
  | { t: 'machine'; online: boolean; hello?: RunnerInfo }
  | { t: 'topology'; rev: number; workspaces: Workspace[]; tabs: Tab[]; panes: Pane[] }
  | { t: 'status'; paneId: string; agent: Agent | null }
  | { t: 'event'; seq: number; paneId: string; type: string; at: string; data: unknown }
  | { t: 'size'; paneId: string; cols: number; rows: number }
  | { t: 'owner'; paneId: string; you: boolean }
  | { t: 'res'; id: string; result?: unknown; error?: { code: string; message?: string } }
  | { t: 'error'; code: string; message?: string }
  | GlanceFrame
  /** A machine stats sample; the first after subscribing carries `history` (stats.ts). */
  | StatsFrame
  /** The newest AI usage record of one account on a machine; only to the machine's owner (usage.ts). */
  | UsageFrame
  /** The replay of `chat_attach` is complete (or refused: `chat_not_found`, `not_supported`, `machine_offline`). */
  | { t: 'chat_attached'; sessionId: string; ok: boolean; lastSeq?: number; code?: string }
  /** A plugin's surfaces on this machine (the last set is replayed after welcome). */
  | { t: 'ui'; pluginId: string; scope: 'machine' | 'pane'; paneId?: string; items: UiItem[] }
  /** The runner's disk, as it reported it (with `machineId` on a machine-list socket). */
  | { t: 'disk'; freeBytes: number; totalBytes: number; filesBytes: number; level: 'ok' | 'low' | 'critical' | 'alert' }
  /** A pane's open needs-you items (the last set per pane is replayed after welcome). */
  | { t: 'needs'; paneId: string; items: NeedsYou[] }
  | { t: 'ping' | 'pong'; at?: number };

export type Parsed<T> = { ok: true; frame: T } | { ok: false; reason: string };

/** A runner API method name: `ping`, `pane.read`, `agent.explain`. */
export const METHOD_NAME = /^[a-z_]+(\.[a-z_]+)*$/;

const isStr = (v: unknown, max = 512): v is string => typeof v === 'string' && v.length > 0 && v.length <= max;
const isU32 = (v: unknown): v is number => typeof v === 'number' && Number.isInteger(v) && v >= 0 && v <= 0xffffffff;
const isOffset = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
const isDim = (v: unknown): v is number => typeof v === 'number' && Number.isInteger(v) && v >= 1 && v <= 1000;

function parseJson(text: string, max = MAX_FRAME_BYTES): Record<string, unknown> | null {
  if (text.length > max) return null;
  try {
    const v = JSON.parse(text);
    return v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

/**
 * Validate a client frame. Only the fields the gateway acts on are checked;
 * `req.params` is forwarded opaquely and validated by the runner.
 */
export function parseClientFrame(text: string): Parsed<ClientToGateway> {
  const o = parseJson(text);
  if (!o) return { ok: false, reason: 'not_json_object' };
  switch (o.t) {
    case 'hello': {
      const c = o.client as Record<string, unknown> | undefined;
      if (!c || !['web', 'pwa', 'tui', 'cli'].includes(c.kind as string) || typeof c.version !== 'string') return { ok: false, reason: 'bad_hello' };
      return { ok: true, frame: { t: 'hello', client: { kind: c.kind as 'web', version: String(c.version).slice(0, 64) } } };
    }
    case 'attach':
      if (!isStr(o.paneId, 255) || (o.from !== undefined && !isOffset(o.from))) return { ok: false, reason: 'bad_attach' };
      return { ok: true, frame: { t: 'attach', paneId: o.paneId, ...(o.from !== undefined ? { from: o.from as number } : {}) } };
    case 'detach':
      if (!isStr(o.paneId, 255)) return { ok: false, reason: 'bad_detach' };
      return { ok: true, frame: { t: 'detach', paneId: o.paneId } };
    case 'ack':
      if (!isStr(o.paneId, 255) || !isOffset(o.offset)) return { ok: false, reason: 'bad_ack' };
      return { ok: true, frame: { t: 'ack', paneId: o.paneId, offset: o.offset } };
    case 'resize':
      if (!isStr(o.paneId, 255) || !isDim(o.cols) || !isDim(o.rows)) return { ok: false, reason: 'bad_resize' };
      return { ok: true, frame: { t: 'resize', paneId: o.paneId, cols: o.cols, rows: o.rows } };
    case 'req':
      if (!isStr(o.id, 64) || !isStr(o.method, 64) || !METHOD_NAME.test(o.method)) return { ok: false, reason: 'bad_req' };
      return { ok: true, frame: { t: 'req', id: o.id, method: o.method, params: o.params ?? {} } };
    case 'decision':
      if (!isStr(o.paneId, 255) || !isStr(o.promptId, 128) || (o.behavior !== 'allow' && o.behavior !== 'deny')) return { ok: false, reason: 'bad_decision' };
      return {
        ok: true,
        frame: { t: 'decision', paneId: o.paneId, promptId: o.promptId, behavior: o.behavior, ...(typeof o.message === 'string' ? { message: o.message.slice(0, 2000) } : {}) },
      };
    case 'stats':
      if (typeof o.on !== 'boolean' || (o.hz !== undefined && typeof o.hz !== 'number')) return { ok: false, reason: 'bad_stats' };
      return { ok: true, frame: { t: 'stats', on: o.on, hz: clampStatsHz(o.hz) } };
    case 'glance': {
      if (!Array.isArray(o.panes) || o.panes.length > GLANCE_MAX_PANES) return { ok: false, reason: 'bad_glance' };
      const panes: GlanceTarget[] = [];
      for (const p of o.panes as unknown[]) {
        const x = p as Record<string, unknown> | null;
        if (!x || typeof x !== 'object' || !isStr(x.paneId, 255) || (x.machineId !== undefined && !isStr(x.machineId, 128))) return { ok: false, reason: 'bad_glance' };
        panes.push({ ...(x.machineId !== undefined ? { machineId: x.machineId as string } : {}), paneId: x.paneId });
      }
      const crop = parseGlanceCrop(o.rows, o.cols);
      if (crop === false) return { ok: false, reason: 'bad_glance' };
      return { ok: true, frame: { t: 'glance', panes, hz: clampHz(o.hz), ...(crop ? { rows: crop.rows, cols: crop.cols } : {}) } };
    }
    case 'login_callback':
      // Shape only; the runner checks the URL against its pending callbacks.
      if (!isStr(o.id, 64) || !isStr(o.paneId, 255) || !isStr(o.url, 8192) || !/^https?:\/\//i.test(o.url)) return { ok: false, reason: 'bad_login_callback' };
      return { ok: true, frame: { t: 'login_callback', id: o.id, paneId: o.paneId, url: o.url } };
    case 'chat_attach':
      if (typeof o.sessionId !== 'string' || !CHAT_SESSION_ID.test(o.sessionId) || (o.from !== undefined && !isOffset(o.from))) return { ok: false, reason: 'bad_chat_attach' };
      return { ok: true, frame: { t: 'chat_attach', sessionId: o.sessionId, ...(o.from !== undefined ? { from: o.from as number } : {}) } };
    case 'chat_detach':
      if (typeof o.sessionId !== 'string' || !CHAT_SESSION_ID.test(o.sessionId)) return { ok: false, reason: 'bad_chat_detach' };
      return { ok: true, frame: { t: 'chat_detach', sessionId: o.sessionId } };
    case 'plugin_action':
      // Shape only; the runner looks the action up in the manifest it holds.
      if (!isStr(o.id, 64) || typeof o.pluginId !== 'string' || !/^[a-z0-9-]{2,32}$/.test(o.pluginId) || typeof o.actionId !== 'string' || !/^[a-z0-9][a-z0-9._-]{0,63}$/.test(o.actionId) || (o.paneId !== undefined && !isStr(o.paneId, 255))) return { ok: false, reason: 'bad_plugin_action' };
      return { ok: true, frame: { t: 'plugin_action', id: o.id, pluginId: o.pluginId, actionId: o.actionId, ...(o.paneId !== undefined ? { paneId: o.paneId as string } : {}) } };
    case 'ping':
    case 'pong':
      return { ok: true, frame: { t: o.t, ...(typeof o.at === 'number' ? { at: o.at } : {}) } };
    default:
      return { ok: false, reason: 'unknown_type' };
  }
}

/** Validate a runner frame; shapes inside topology/status/event are trusted to the runner's own tests. */
export function parseRunnerFrame(text: string): Parsed<RunnerToGateway> {
  const o = parseJson(text, MAX_RUNNER_TEXT_BYTES);
  if (!o) return { ok: false, reason: 'not_json_object' };
  switch (o.t) {
    case 'hello': {
      const r = o.runner as Record<string, unknown> | undefined;
      if (o.v !== PROTOCOL_VERSION) return { ok: false, reason: 'unsupported_version' };
      if (!r || !isStr(r.version, 64) || !isStr(r.os, 32) || !isStr(r.arch, 32) || !isStr(r.hostname, 255)) return { ok: false, reason: 'bad_hello' };
      // claudeAuth is optional: a malformed one is dropped, the hello is kept.
      if (r.claudeAuth !== undefined) {
        const auth = parseClaudeAuth(r.claudeAuth);
        const runner = { ...r } as Record<string, unknown>;
        if (auth) runner.claudeAuth = auth;
        else delete runner.claudeAuth;
        return { ok: true, frame: { ...o, runner } as unknown as RunnerToGateway };
      }
      return { ok: true, frame: o as unknown as RunnerToGateway };
    }
    case 'topology':
      if (!isOffset(o.rev) || !Array.isArray(o.workspaces) || !Array.isArray(o.tabs) || !Array.isArray(o.panes)) return { ok: false, reason: 'bad_topology' };
      return { ok: true, frame: o as unknown as RunnerToGateway };
    case 'status':
      if (!isStr(o.paneId, 255) || (o.agent !== null && typeof o.agent !== 'object')) return { ok: false, reason: 'bad_status' };
      return { ok: true, frame: o as unknown as RunnerToGateway };
    case 'event':
      // paneId may be "" (machine-level event); it must still be a string.
      if (!isOffset(o.seq) || typeof o.paneId !== 'string' || o.paneId.length > 255 || !isStr(o.type, 64) || !isStr(o.at, 64)) return { ok: false, reason: 'bad_event' };
      return { ok: true, frame: o as unknown as RunnerToGateway };
    case 'res':
      if (!isStr(o.id, 64)) return { ok: false, reason: 'bad_res' };
      return { ok: true, frame: o as unknown as RunnerToGateway };
    case 'size':
      if (!isStr(o.paneId, 255) || !isDim(o.cols) || !isDim(o.rows)) return { ok: false, reason: 'bad_size' };
      return { ok: true, frame: o as unknown as RunnerToGateway };
    case 'glance':
      // Shape only: the gateway forwards it and never reads the text (clients decode with decodeGlance).
      if (text.length > GLANCE_MAX_BYTES) return { ok: false, reason: 'glance_too_large' };
      if (!isStr(o.paneId, 255) || !isOffset(o.rev) || !isDim(o.cols) || !isDim(o.rows) || !Array.isArray(o.lines)) return { ok: false, reason: 'bad_glance' };
      return { ok: true, frame: o as unknown as RunnerToGateway };
    case 'stats':
      // Shape only here (this file imports no values); the gateway checks and rebuilds it with stats.ts parseStatsSample.
      if (text.length > STATS_MAX_BYTES) return { ok: false, reason: 'stats_too_large' };
      if (!isOffset(o.at)) return { ok: false, reason: 'bad_stats' };
      return { ok: true, frame: o as unknown as RunnerToGateway };
    case 'usage':
      // Shape only here (this file imports no values); the gateway checks and rebuilds it with usage.ts parseUsage.
      if (text.length > 4096) return { ok: false, reason: 'usage_too_large' };
      if (typeof o.plugin !== 'string' || typeof o.accountKey !== 'string' || !isOffset(o.observedAt)) return { ok: false, reason: 'bad_usage' };
      return { ok: true, frame: o as unknown as RunnerToGateway };
    case 'chat_attached':
      if (!isU32(o.req) || typeof o.sessionId !== 'string' || !CHAT_SESSION_ID.test(o.sessionId) || typeof o.ok !== 'boolean') return { ok: false, reason: 'bad_chat_attached' };
      return { ok: true, frame: {
        t: 'chat_attached', req: o.req, sessionId: o.sessionId, ok: o.ok, lastSeq: isOffset(o.lastSeq) ? o.lastSeq : 0,
        ...(typeof o.code === 'string' && /^[a-z_]{1,64}$/.test(o.code) ? { code: o.code } : {}),
      } };
    case 'ui':
      // Shape only here (this file imports no values); the gateway checks each item with plugins.ts parseUiFrame.
      if (typeof o.pluginId !== 'string' || (o.scope !== 'machine' && o.scope !== 'pane') || !Array.isArray(o.items)) return { ok: false, reason: 'bad_ui' };
      return { ok: true, frame: o as unknown as RunnerToGateway };
    case 'secret_key':
      if (!isStr(o.id, 64) || !isStr(o.alg, 64) || !isStr(o.publicKey, 64)) return { ok: false, reason: 'bad_secret_key' };
      return { ok: true, frame: { t: 'secret_key', id: o.id, alg: o.alg, publicKey: o.publicKey } };
    case 'secret_req': {
      const names = o.names;
      if (!isStr(o.id, 64) || !Array.isArray(names) || names.length < 1 || names.length > 32 || !names.every((n) => isStr(n, 128))
        || !o.purpose || typeof o.purpose !== 'object' || Array.isArray(o.purpose) || JSON.stringify(o.purpose).length > 4096) return { ok: false, reason: 'bad_secret_req' };
      return { ok: true, frame: { t: 'secret_req', id: o.id, names: names as string[], purpose: o.purpose as Record<string, unknown> } };
    }
    case 'file_ack': {
      if (typeof o.uploadId !== 'string' || !/^[A-Za-z0-9_-]{8,64}$/.test(o.uploadId) || !isU32(o.n) || typeof o.ok !== 'boolean') return { ok: false, reason: 'bad_file_ack' };
      return { ok: true, frame: {
        t: 'file_ack', uploadId: o.uploadId, n: o.n, ok: o.ok,
        ...(typeof o.sha256 === 'string' && /^[0-9a-f]{64}$/.test(o.sha256) ? { sha256: o.sha256 } : {}),
        ...(typeof o.code === 'string' && /^[a-z_]{1,64}$/.test(o.code) ? { code: o.code } : {}),
        ...(typeof o.message === 'string' ? { message: o.message.slice(0, 500) } : {}),
      } };
    }
    case 'disk': {
      const n = (x: unknown) => typeof x === 'number' && Number.isSafeInteger(x) && x >= 0;
      if (!n(o.freeBytes) || !n(o.totalBytes) || !n(o.filesBytes) || !['ok', 'low', 'critical', 'alert'].includes(o.level as string)) return { ok: false, reason: 'bad_disk' };
      return { ok: true, frame: { t: 'disk', freeBytes: o.freeBytes as number, totalBytes: o.totalBytes as number, filesBytes: o.filesBytes as number, level: o.level as 'ok' } };
    }
    case 'needs':
      // Shape only here; the gateway checks each item with needs.ts parseNeedsFrame.
      if (typeof o.paneId !== 'string' || o.paneId.length > 255 || !Array.isArray(o.items)) return { ok: false, reason: 'bad_needs' };
      return { ok: true, frame: o as unknown as RunnerToGateway };
    case 'proof':
      if (!isStr(o.sig, 200)) return { ok: false, reason: 'bad_proof' };
      return { ok: true, frame: { t: 'proof', sig: o.sig as string } };
    case 'ping':
    case 'pong':
      return { ok: true, frame: { t: o.t, at: typeof o.at === 'number' ? o.at : 0 } };
    default:
      return { ok: false, reason: 'unknown_type' };
  }
}

export { isU32 };
