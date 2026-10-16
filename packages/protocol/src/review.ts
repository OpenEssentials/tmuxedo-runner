/**
 * Review and ship's `git.*` methods (docs/design/away-and-review.md §4.2, §5): names, limits and
 * parameter shapes. runner/crates/protocol/src/review.rs is the same contract; both are held to
 * fixtures/review.json. A request names a session the runner knows (a pane or a chat), never a
 * folder; a file is named relative to the repository's top with the file browser's lexical rule.
 */
import { fsPathComponents } from './fs.js';

export const FEATURE_REVIEW = 'review';
export const REVIEW_READ_METHODS: readonly string[] = ['git.status', 'git.diff'];
/** Not served yet: they will travel through web only, with control and a fresh step-up. */
export const REVIEW_MUTATING_METHODS: readonly string[] = ['git.commit', 'git.push', 'git.discard'];

export const REVIEW_MAX_SESSION_ID = 128;
export const REVIEW_STATUS_FILES_MAX = 1000;
export const REVIEW_PATCH_MAX = 512 * 1024;
export const REVIEW_LARGE_LINES = 2000;

export function isReviewMethod(method: string): boolean {
  return method.startsWith('git.');
}

export function validSessionId(id: unknown): id is string {
  return typeof id === 'string' && id.length >= 1 && id.length <= REVIEW_MAX_SESSION_ID && /^[A-Za-z0-9_:.-]+$/.test(id);
}

export type ReviewTarget = { pane: string } | { chat: string };
export type ReviewReadParams = { method: 'git.status'; target: ReviewTarget } | { method: 'git.diff'; target: ReviewTarget; path: string };
export type ReviewParsed = { ok: true; params: ReviewReadParams } | { ok: false; code: 'invalid_params' | 'invalid_path'; reason: string };

const bad = (reason: string): ReviewParsed => ({ ok: false, code: 'invalid_params', reason });

/** Parse the parameters of one of REVIEW_READ_METHODS. Unknown fields are ignored; known ones must be exactly right. */
export function parseReviewReadParams(method: string, params: unknown): ReviewParsed {
  if (!params || typeof params !== 'object' || Array.isArray(params)) return bad('params must be an object');
  const p = params as Record<string, unknown>;
  const own = (k: string): unknown => (Object.prototype.hasOwnProperty.call(p, k) ? p[k] : undefined);
  if (!REVIEW_READ_METHODS.includes(method)) return bad('not a reading git method');
  const pane = own('pane');
  const chat = own('chat');
  const hasPane = pane !== undefined && pane !== null;
  const hasChat = chat !== undefined && chat !== null;
  if (hasPane === hasChat) return bad('exactly one of pane or chat is required');
  let target: ReviewTarget;
  if (hasPane) {
    if (!validSessionId(pane)) return bad('pane must be a session id');
    target = { pane };
  } else {
    if (!validSessionId(chat)) return bad('chat must be a session id');
    target = { chat };
  }
  if (method === 'git.status') return { ok: true, params: { method, target } };
  const path = own('path');
  if (typeof path !== 'string') return bad('path is required');
  if (path === '' || !fsPathComponents(path)) return { ok: false, code: 'invalid_path', reason: 'That path is not allowed.' };
  return { ok: true, params: { method: 'git.diff', target, path } };
}
