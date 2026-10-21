/**
 * Session recording and replay's `replay.*` methods (docs/design/sharing-and-integrations.md §3, §7):
 * names, limits, parameter shapes and the marker kinds. runner/crates/protocol/src/replay.rs is the same
 * contract; both are held to fixtures/replay.json. A request names a session (a pane), never a file;
 * recordings stay on the machine and travel through web only.
 */
import { validSessionId } from './review.js';

export const FEATURE_REPLAY = 'replay';
export const REPLAY_METHODS: readonly string[] = ['replay.list', 'replay.set', 'replay.read', 'replay.delete'];

export const REPLAY_READ_BYTES_MAX = 512 * 1024;
export const REPLAY_READ_BYTES_DEFAULT = 256 * 1024;
/** Retention (D-SI4): 14 days, 1 GiB per machine, oldest first. */
export const REPLAY_RETENTION_DAYS = 14;
export const REPLAY_MAX_BYTES = 1024 * 1024 * 1024;
export const REPLAY_SEGMENT_BYTES = 16 * 1024 * 1024;
export const REPLAY_SEGMENT_MS = 6 * 60 * 60 * 1000;

export function isReplayMethod(method: string): boolean {
  return method.startsWith('replay.');
}

/** `part.offset.at`: where the previous page ended. */
export function validReplayCursor(c: unknown): c is string {
  return typeof c === 'string' && /^\d{1,16}\.\d{1,16}\.\d{1,16}$/.test(c);
}

export type ReplayParams =
  | { method: 'replay.list'; pane?: string }
  | { method: 'replay.set'; pane: string; on: boolean }
  | { method: 'replay.read'; pane: string; fromMs?: number; cursor?: string; limitBytes: number; markersOnly: boolean }
  | { method: 'replay.delete'; pane: string };
export type ReplayParsed = { ok: true; params: ReplayParams } | { ok: false; code: 'invalid_params'; reason: string };

const bad = (reason: string): ReplayParsed => ({ ok: false, code: 'invalid_params', reason });
const whole = (v: unknown): boolean => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;

/** Parse the parameters of one of REPLAY_METHODS. Unknown fields are ignored; known ones must be exactly right. */
export function parseReplayParams(method: string, params: unknown): ReplayParsed {
  if (!params || typeof params !== 'object' || Array.isArray(params)) return bad('params must be an object');
  const p = params as Record<string, unknown>;
  const own = (k: string): unknown => (Object.prototype.hasOwnProperty.call(p, k) ? p[k] : undefined);
  const pane = own('pane');
  const given = (v: unknown) => v !== undefined && v !== null;
  switch (method) {
    case 'replay.list':
      if (!given(pane)) return { ok: true, params: { method } };
      return validSessionId(pane) ? { ok: true, params: { method, pane } } : bad('pane must be a session id');
    case 'replay.set': {
      if (!validSessionId(pane)) return bad('pane must be a session id');
      const on = own('on');
      if (typeof on !== 'boolean') return bad('on must be true or false');
      return { ok: true, params: { method, pane, on } };
    }
    case 'replay.read': {
      if (!validSessionId(pane)) return bad('pane must be a session id');
      const fromMs = own('from_ms');
      const cursor = own('cursor');
      const limit = own('limit_bytes');
      const markers = own('markers_only');
      if (given(fromMs) && !whole(fromMs)) return bad('from_ms must be a whole number');
      if (given(cursor) && !validReplayCursor(cursor)) return bad('cursor is not one this runner gave');
      if (given(fromMs) && given(cursor)) return bad('from_ms and cursor are exclusive');
      if (given(limit) && !(whole(limit) && (limit as number) >= 1 && (limit as number) <= REPLAY_READ_BYTES_MAX)) return bad('limit_bytes is 1 to 524288');
      if (given(markers) && typeof markers !== 'boolean') return bad('markers_only must be true or false');
      return {
        ok: true,
        params: {
          method, pane,
          ...(given(fromMs) ? { fromMs: fromMs as number } : {}),
          ...(given(cursor) ? { cursor: cursor as string } : {}),
          limitBytes: given(limit) ? (limit as number) : REPLAY_READ_BYTES_DEFAULT,
          markersOnly: markers === true,
        },
      };
    }
    case 'replay.delete':
      return validSessionId(pane) ? { ok: true, params: { method, pane } } : bad('pane must be a session id');
    default:
      return bad('not a replay method');
  }
}

/** The marker kinds the player draws (design §0: prompt ▸, needs you ■, approved ✓, error ✕, done ✓, idle). */
export const REPLAY_MARKERS = ['prompt', 'needs', 'approved', 'error', 'done', 'idle'] as const;
export type ReplayMarker = (typeof REPLAY_MARKERS)[number];

const MARKER_OF: Record<string, ReplayMarker> = {
  'status:working': 'prompt',
  blocked: 'needs',
  needs_resolved: 'approved',
  done: 'done',
  idle: 'idle',
  memory_stopped: 'error',
  auth_failed: 'error',
  auth_expired: 'error',
};

/** The marker a recorded label is drawn as; null for labels the player does not draw (unknown ones too). */
export function replayMarkerOf(label: string): ReplayMarker | null {
  return Object.prototype.hasOwnProperty.call(MARKER_OF, label) ? MARKER_OF[label]! : null;
}
