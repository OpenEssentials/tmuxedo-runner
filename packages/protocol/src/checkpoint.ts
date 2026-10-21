/**
 * Checkpoints and undo (Phase 23; docs/protocol.md "Checkpoints"): method names, limits and
 * parameter shapes. runner/crates/protocol/src/checkpoint.rs is the same contract. A request names
 * a session the runner knows (a pane or a chat), never a folder; a checkpoint is named by its
 * number in that session.
 */
import { validSessionId } from './review.js';
import { parseSessionUsage, type SessionUsage } from './usage.js';

export const FEATURE_CHECKPOINTS = 'checkpoints';
export const CHECKPOINT_LIST = 'checkpoint.list';
export const CHECKPOINT_PREVIEW = 'checkpoint.preview';
export const CHECKPOINT_RESTORE = 'checkpoint.restore';
export const CHECKPOINT_CREATE = 'checkpoint.create';
export const CHECKPOINT_METHODS: readonly string[] = [CHECKPOINT_LIST, CHECKPOINT_PREVIEW, CHECKPOINT_RESTORE, CHECKPOINT_CREATE];
/** What a client socket may send (control of the owner's machine). A restore changes files: web only. */
export const CHECKPOINT_SOCKET_METHODS: readonly string[] = [CHECKPOINT_LIST, CHECKPOINT_PREVIEW, CHECKPOINT_CREATE];

export const CHECKPOINT_KEEP = 50;
export const CHECKPOINT_PREVIEW_FILES_MAX = 500;
export const CHECKPOINT_ID_MAX = 999_999;

export function isCheckpointMethod(method: string): boolean {
  return method.startsWith('checkpoint.');
}

export type CheckpointTarget = { pane: string } | { chat: string };
export type CheckpointParsed = { ok: true; target: CheckpointTarget; id?: number } | { ok: false; reason: string };

/** Parse `{pane | chat, id?}`; `id` is required by preview and restore. */
export function parseCheckpointParams(method: string, params: unknown): CheckpointParsed {
  if (!params || typeof params !== 'object' || Array.isArray(params)) return { ok: false, reason: 'params must be an object' };
  const p = params as Record<string, unknown>;
  const own = (k: string): unknown => (Object.prototype.hasOwnProperty.call(p, k) ? p[k] : undefined);
  if (!CHECKPOINT_METHODS.includes(method)) return { ok: false, reason: 'not a checkpoint method' };
  const pane = own('pane');
  const chat = own('chat');
  const hasPane = pane !== undefined && pane !== null;
  const hasChat = chat !== undefined && chat !== null;
  if (hasPane === hasChat) return { ok: false, reason: 'exactly one of pane or chat is required' };
  let target: CheckpointTarget;
  if (hasPane) {
    if (!validSessionId(pane)) return { ok: false, reason: 'pane must be a session id' };
    target = { pane };
  } else {
    if (!validSessionId(chat)) return { ok: false, reason: 'chat must be a session id' };
    target = { chat };
  }
  const id = own('id');
  if (id !== undefined && id !== null) {
    if (typeof id !== 'number' || !Number.isInteger(id) || id < 1 || id > CHECKPOINT_ID_MAX) return { ok: false, reason: 'id must be a checkpoint number' };
    return { ok: true, target, id };
  }
  if (method === CHECKPOINT_PREVIEW || method === CHECKPOINT_RESTORE) return { ok: false, reason: 'id is required' };
  return { ok: true, target };
}

/**
 * `session.summary` (the away digest's facts; runner/crates/protocol/src/checkpoint.rs): what a
 * session's folder changed since the first checkpoint at or after `since`, the chat turns recorded
 * since then, and the agent's last line (masked by the runner). Web asks for it; never a client socket.
 */
export const SESSION_SUMMARY = 'session.summary';
export const FEATURE_SESSION_SUMMARY = 'session_summary';
export const SESSION_SUMMARY_LINE_MAX = 120;

export interface SessionSummary {
  turns: number;
  /** Files the agent added, changed and deleted; null when nothing was recorded to compare with. */
  files: { added: number; modified: number; removed: number } | null;
  line: string | null;
  /** The chat turn the line ended (`t3`), for a link to it. */
  turn: string | null;
  /** What the session used of its AI plan (AI usage on, a pane's agent reported it); absent otherwise. */
  usage?: SessionUsage;
}

const count = (v: unknown): v is number => typeof v === 'number' && Number.isInteger(v) && v >= 0 && v <= 1_000_000_000;

/** A runner's `session.summary` answer, or null when it is not one. */
export function parseSessionSummary(v: unknown): SessionSummary | null {
  if (!v || typeof v !== 'object' || Array.isArray(v)) return null;
  const r = v as Record<string, unknown>;
  if (r.type !== 'session_summary' || !count(r.turns)) return null;
  let files: SessionSummary['files'] = null;
  if (r.files !== null && r.files !== undefined) {
    const f = r.files as Record<string, unknown>;
    if (typeof f !== 'object' || !count(f.added) || !count(f.modified) || !count(f.removed)) return null;
    files = { added: f.added, modified: f.modified, removed: f.removed };
  }
  const line = typeof r.line === 'string' && r.line.trim() ? r.line.trim().slice(0, SESSION_SUMMARY_LINE_MAX) : null;
  const turn = typeof r.turn === 'string' && /^t\d{1,23}$/.test(r.turn) ? r.turn : null;
  const usage = parseSessionUsage(r.usage);
  return usage ? { turns: r.turns, files, line, turn, usage } : { turns: r.turns, files, line, turn };
}
