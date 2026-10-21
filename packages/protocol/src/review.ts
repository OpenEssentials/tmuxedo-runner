/**
 * Review and ship's `git.*` methods (docs/design/away-and-review.md §4.2, §5): names, limits and
 * parameter shapes. runner/crates/protocol/src/review.rs is the same contract; both are held to
 * fixtures/review.json. A request names a session the runner knows (a pane or a chat), never a
 * folder; a file is named relative to the repository's top with the file browser's lexical rule.
 */
import { fsPathComponents } from './fs.js';

export const FEATURE_REVIEW = 'review';
export const REVIEW_READ_METHODS: readonly string[] = ['git.status', 'git.diff'];
/** They never travel on a client socket (the gateway refuses them there). */
export const REVIEW_MUTATING_METHODS: readonly string[] = ['git.commit', 'git.push', 'git.discard'];
/** Served for web only (`machine.call`, after control and a fresh step-up); `git.discard` records a checkpoint first. */
export const REVIEW_WEB_METHODS: readonly string[] = ['git.commit', 'git.push', 'git.discard'];
export const REVIEW_MESSAGE_MAX = 10_000;
export const REVIEW_BRANCH_MAX = 200;

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

/** A branch name tmuxedo hands to git (review.rs `valid_branch_name`). */
export function validBranchName(s: unknown): s is string {
  return typeof s === 'string' && s.length >= 1 && new TextEncoder().encode(s).length <= REVIEW_BRANCH_MAX
    && /^[A-Za-z0-9._/-]+$/.test(s) && !/^[-./]/.test(s) && !/[/.]$/.test(s) && !s.endsWith('.lock')
    && !s.includes('..') && !s.includes('//') && !s.includes('/.') && !s.includes('@{') && s !== 'HEAD';
}

export type ReviewShipParams =
  | { method: 'git.commit'; target: ReviewTarget; message: string; branch: string | null }
  | { method: 'git.push'; target: ReviewTarget; pr: boolean; base: string | null }
  | { method: 'git.discard'; target: ReviewTarget };
export type ReviewShipParsed = { ok: true; params: ReviewShipParams } | { ok: false; code: 'invalid_params' | 'invalid_branch'; reason: string };

/** Parse the parameters of one of REVIEW_WEB_METHODS (review.rs `parse_ship_params`). */
export function parseReviewShipParams(method: string, params: unknown): ReviewShipParsed {
  const fail = (reason: string): ReviewShipParsed => ({ ok: false, code: 'invalid_params', reason });
  if (!params || typeof params !== 'object' || Array.isArray(params)) return fail('params must be an object');
  if (!REVIEW_WEB_METHODS.includes(method)) return fail('not a git method served here');
  // The session rule is the reading methods' own.
  const t = parseReviewReadParams('git.status', params);
  if (!t.ok) return fail(t.reason);
  const target = t.params.target;
  if (method === 'git.discard') return { ok: true, params: { method: 'git.discard', target } };
  const p = params as Record<string, unknown>;
  const branchOf = (k: string): string | null | undefined => {
    const v = Object.prototype.hasOwnProperty.call(p, k) ? p[k] : undefined;
    if (v === undefined || v === null) return null;
    return validBranchName(v) ? v : undefined;
  };
  if (method === 'git.commit') {
    const m = p.message;
    if (typeof m !== 'string' || !m.trim() || new TextEncoder().encode(m).length > REVIEW_MESSAGE_MAX || m.includes('\0')) {
      return fail('message must be 1..=10000 bytes of text');
    }
    const branch = branchOf('branch');
    if (branch === undefined) return { ok: false, code: 'invalid_branch', reason: 'That branch name is not allowed.' };
    return { ok: true, params: { method: 'git.commit', target, message: m, branch } };
  }
  if (p.pr !== undefined && p.pr !== null && typeof p.pr !== 'boolean') return fail('pr must be true or false');
  const base = branchOf('base');
  if (base === undefined) return { ok: false, code: 'invalid_branch', reason: 'That branch name is not allowed.' };
  return { ok: true, params: { method: 'git.push', target, pr: p.pr === true, base } };
}
