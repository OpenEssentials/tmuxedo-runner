/**
 * The file browser's `fs.*` methods (docs/design/file-browser.md §4): the
 * method names, the limits, the one lexical path rule and the parameter
 * shapes. `runner/crates/protocol/src/fs.rs` is the same contract; both are
 * held to `fixtures/fs.json`.
 *
 * Every field of a request is hostile until parsed here: a wrong type, an
 * over-long string or an unknown shape is `invalid_params`, and a path the
 * lexical rule refuses is `invalid_path`. Nothing is normalised: a path that
 * would need cleaning is refused. The runner enforces all of it again.
 */

/** The runner hello feature (`hello.runner.features`). */
export const FEATURE_FS = 'fs';

/** The methods that only read. */
export const FS_READ_METHODS: readonly string[] = ['fs.roots', 'fs.list', 'fs.stat', 'fs.read', 'fs.search'];
/** The methods that change something on the machine: through web only, never a client socket. */
export const FS_MUTATING_METHODS: readonly string[] = ['fs.write', 'fs.mkdir', 'fs.rename', 'fs.delete', 'fs.import', 'fs.undo', 'fs.root_add', 'fs.root_remove'];

/**
 * The gateway's own method for an fs download (`/gw/files/fs/…`): one window
 * of a file in a root, as FILE frames. Never on a client socket (it is not a
 * reading method there, so the gateway refuses it) and never on the local one.
 */
export const FS_DOWNLOAD_METHOD = 'fs.download';

export const FS_MAX_PATH_BYTES = 4096;
export const FS_MAX_COMPONENTS = 64;
export const FS_MAX_COMPONENT_BYTES = 255;
export const FS_MAX_ROOT_ID = 64;
export const FS_LIST_MAX = 500;
export const FS_MAX_CURSOR_BYTES = FS_MAX_COMPONENT_BYTES + 2;
export const FS_SEARCH_QUERY_MAX = 100;
export const FS_SEARCH_MAX_RESULTS = 50;
export const FS_TEXT_MAX = 2 * 1024 * 1024;
export const FS_EDITABLE_MAX = 512 * 1024;

export function isFsMethod(method: string): boolean {
  return method.startsWith('fs.');
}

const utf8 = new TextEncoder();
const bytes = (s: string): number => utf8.encode(s).length;
/** Unicode Cc (what Rust's `char::is_control` is). */
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;
/** A string that is not well-formed UTF-16 has no UTF-8 form: never a path. */
const LONE_SURROGATE = /[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/;

function reservedDevice(component: string): boolean {
  const up = (component.split('.')[0] ?? '').replace(/ +$/, '').replace(/[a-z]/g, (c) => c.toUpperCase());
  if (['CON', 'PRN', 'AUX', 'NUL', 'CONIN$', 'CONOUT$'].includes(up)) return true;
  if (up.startsWith('COM') || up.startsWith('LPT')) return /^[1-9¹²³]$/.test(up.slice(3));
  return false;
}

/** One path component the rule accepts. */
export function validFsComponent(p: string): boolean {
  return !(
    p === '' || p === '.' || p === '..' || LONE_SURROGATE.test(p) || bytes(p) > FS_MAX_COMPONENT_BYTES
    || CONTROL.test(p) || /[\\/:]/.test(p) || p.endsWith('.') || p.endsWith(' ') || reservedDevice(p)
  );
}

/**
 * THE lexical rule (design §3.4 step 1): the components of a relative,
 * `/`-separated request path, or null. Refused, never cleaned.
 */
export function fsPathComponents(rel: unknown): string[] | null {
  if (typeof rel !== 'string' || rel === '' || rel.length > FS_MAX_PATH_BYTES || bytes(rel) > FS_MAX_PATH_BYTES) return null;
  const parts = rel.split('/');
  return parts.length <= FS_MAX_COMPONENTS && parts.every(validFsComponent) ? parts : null;
}

export function validFsRootId(id: unknown): id is string {
  return typeof id === 'string' && /^[A-Za-z0-9_-]{1,64}$/.test(id);
}

/** Where a list page starts: after this entry. `d/<name>` or `f/<name>`. */
export interface FsCursor { dir: boolean; name: string }

export function parseFsCursor(s: unknown): FsCursor | null {
  if (typeof s !== 'string' || s.length > FS_MAX_CURSOR_BYTES || LONE_SURROGATE.test(s) || bytes(s) > FS_MAX_CURSOR_BYTES) return null;
  const at = s.indexOf('/');
  if (at !== 1 || (s[0] !== 'd' && s[0] !== 'f')) return null;
  const name = s.slice(2);
  if (name === '' || bytes(name) > FS_MAX_COMPONENT_BYTES || name.includes('/') || name.includes('\u0000')) return null;
  return { dir: s[0] === 'd', name };
}

export type FsReadParams =
  | { method: 'fs.roots' }
  | { method: 'fs.list'; root: string; path: string; cursor?: FsCursor; limit: number }
  | { method: 'fs.stat'; root: string; path: string }
  | { method: 'fs.read'; root: string; path: string }
  | { method: 'fs.search'; root: string; query: string; limit: number };

export type FsParsed = { ok: true; params: FsReadParams } | { ok: false; code: 'invalid_params' | 'invalid_path'; reason: string };

const bad = (reason: string): FsParsed => ({ ok: false, code: 'invalid_params', reason });

function limitOf(v: unknown, max: number): number | null {
  if (v === undefined || v === null) return max;
  return typeof v === 'number' && Number.isInteger(v) && v >= 1 && v <= max ? v : null;
}

/** Parse the parameters of one of FS_READ_METHODS. Unknown fields are ignored; known ones must be exactly right. */
export function parseFsReadParams(method: string, params: unknown): FsParsed {
  if (!params || typeof params !== 'object' || Array.isArray(params)) return bad('params must be an object');
  const p = params as Record<string, unknown>;
  const own = (k: string): unknown => (Object.prototype.hasOwnProperty.call(p, k) ? p[k] : undefined);
  if (method === 'fs.roots') return { ok: true, params: { method } };
  if (!FS_READ_METHODS.includes(method)) return bad('not a reading fs method');
  const root = own('root');
  if (method === 'fs.search') {
    const q = own('query');
    if (typeof q !== 'string' || q.trim() === '' || [...q].length > FS_SEARCH_QUERY_MAX || CONTROL.test(q) || LONE_SURROGATE.test(q)) return bad('query must be 1 to 100 characters');
    if (!validFsRootId(root)) return bad('root must be a root id');
    const limit = limitOf(own('limit'), FS_SEARCH_MAX_RESULTS);
    if (limit === null) return bad('limit is out of range');
    return { ok: true, params: { method, root, query: q, limit } };
  }
  let cursor: FsCursor | undefined;
  if (method === 'fs.list' && own('cursor') !== undefined && own('cursor') !== null) {
    const c = parseFsCursor(own('cursor'));
    if (!c) return bad('cursor is not one this runner gave');
    cursor = c;
  }
  if (!validFsRootId(root)) return bad('root must be a root id');
  const path = own('path');
  if (typeof path !== 'string') return bad('path must be a string');
  if (!(path === '' && method === 'fs.list') && !fsPathComponents(path)) return { ok: false, code: 'invalid_path', reason: 'path is not a relative path inside the root' };
  if (method === 'fs.list') {
    const limit = limitOf(own('limit'), FS_LIST_MAX);
    if (limit === null) return bad('limit is out of range');
    return { ok: true, params: { method, root, path, ...(cursor ? { cursor } : {}), limit } };
  }
  return { ok: true, params: { method: method as 'fs.stat' | 'fs.read', root, path } };
}

/** A file version: `v1.<sha256 hex>.<size>.<mtimeNs>`. The hash decides. */
export interface FsVersion { sha256: string; size: bigint; mtimeNs: bigint }

const U64_MAX = (1n << 64n) - 1n;

export function parseFsVersion(s: unknown): FsVersion | null {
  if (typeof s !== 'string' || s.length > 128) return null;
  const m = /^v1\.([0-9a-f]{64})\.(0|[1-9][0-9]{0,19})\.(0|[1-9][0-9]{0,19})$/.exec(s);
  if (!m) return null;
  const size = BigInt(m[2]!);
  const mtimeNs = BigInt(m[3]!);
  return size > U64_MAX || mtimeNs > U64_MAX ? null : { sha256: m[1]!, size, mtimeNs };
}

/** The same content (the hash and the size; the time is not compared). */
export function sameFsContent(a: FsVersion, b: FsVersion): boolean {
  return a.sha256 === b.sha256 && a.size === b.size;
}

// ── slice A2: the changing methods ────────────────────────────────────────

/** The changing methods this runner builds; the rest of FS_MUTATING_METHODS answer `not_supported`. */
export const FS_WRITE_METHODS: readonly string[] = ['fs.write', 'fs.mkdir', 'fs.rename', 'fs.delete', 'fs.undo'];
/** Parameters only web sets on a changing method; the gateway strips them from client sockets. */
export const FS_WEB_FLAGS: readonly string[] = ['control', 'stepup_at', 'by'];
/** How old a step-up may be for a write (D-FB4), ms. */
export const FS_STEPUP_WINDOW_MS = 15 * 60 * 1000;
/** How far ahead of the runner's clock a step-up time may be, ms. */
export const FS_STEPUP_SKEW_MS = 5 * 60 * 1000;
export const FS_MAX_OP_ID = 64;

export interface FsWebFlags { control: boolean; stepupAt?: number; by?: string }

export type FsWriteOp =
  | { method: 'fs.write'; root: string; path: string; text: string; base?: string }
  | { method: 'fs.mkdir'; root: string; path: string }
  | { method: 'fs.rename'; root: string; from: string; to: string; base?: string }
  | { method: 'fs.delete'; root: string; path: string; base?: string }
  | { method: 'fs.undo'; undoId: string };

export type FsWriteParsed =
  | { ok: true; op: FsWriteOp; opId: string; flags: FsWebFlags }
  | { ok: false; code: 'invalid_params' | 'invalid_path' | 'too_large' | 'binary_file'; reason: string };

const OP_ID = /^[A-Za-z0-9_-]{1,64}$/;
const UNDO_ID = /^u[0-9a-f]{16}$/;

export function validFsUndoId(id: unknown): id is string {
  return typeof id === 'string' && UNDO_ID.test(id);
}

/** `Ok` (null) when control is held and the step-up is fresh at `nowMs`; else the contract's code. */
export function checkFsWebFlags(f: FsWebFlags, nowMs: number): 'control_required' | 'stepup_required' | null {
  if (!f.control) return 'control_required';
  const t = f.stepupAt;
  if (t === undefined || t > nowMs + FS_STEPUP_SKEW_MS || nowMs - t > FS_STEPUP_WINDOW_MS) return 'stepup_required';
  return null;
}

/** Parse the parameters of one of FS_WRITE_METHODS, as the runner does (fixtures/fs.json `writeParams`). */
export function parseFsWriteParams(method: string, params: unknown): FsWriteParsed {
  const no = (reason: string, code: 'invalid_params' | 'invalid_path' | 'too_large' | 'binary_file' = 'invalid_params'): FsWriteParsed => ({ ok: false, code, reason });
  if (!params || typeof params !== 'object' || Array.isArray(params)) return no('params must be an object');
  const p = params as Record<string, unknown>;
  const own = (k: string): unknown => (Object.prototype.hasOwnProperty.call(p, k) ? p[k] : undefined);
  const root = own('root');
  const pathOf = (k: string): string | FsWriteParsed => {
    const v = own(k);
    if (typeof v !== 'string') return no(`${k} must be a string`);
    return fsPathComponents(v) ? v : no('path is not a relative path inside the root', 'invalid_path');
  };
  const baseOf = (): string | undefined | FsWriteParsed => {
    const b = own('base');
    if (b === undefined || b === null) return undefined;
    return parseFsVersion(b) ? (b as string) : no('base must be a version');
  };
  let op: FsWriteOp;
  if (method === 'fs.write') {
    const text = own('text');
    if (typeof text !== 'string') return no('text must be a string');
    if (new TextEncoder().encode(text).length > FS_EDITABLE_MAX) return no('text is larger than a file the browser may write', 'too_large');
    if (text.includes('\u0000')) return no('text must not hold a NUL', 'binary_file');
    const c = own('create');
    if (!(c === undefined || c === null || typeof c === 'boolean')) return no('create must be a boolean');
    const base = baseOf();
    if (typeof base === 'object') return base;
    if ((c === true) === (base !== undefined)) return no('a write names exactly one of base and create');
    if (!validFsRootId(root)) return no('root must be a root id');
    const path = pathOf('path');
    if (typeof path !== 'string') return path;
    op = { method, root, path, text, ...(base ? { base } : {}) };
  } else if (method === 'fs.mkdir' || method === 'fs.delete') {
    if (!validFsRootId(root)) return no('root must be a root id');
    const path = pathOf('path');
    if (typeof path !== 'string') return path;
    if (method === 'fs.mkdir') op = { method, root, path };
    else {
      const base = baseOf();
      if (typeof base === 'object') return base;
      op = { method, root, path, ...(base ? { base } : {}) };
    }
  } else if (method === 'fs.rename') {
    if (!validFsRootId(root)) return no('root must be a root id');
    const from = pathOf('from');
    if (typeof from !== 'string') return from;
    const to = pathOf('to');
    if (typeof to !== 'string') return to;
    const base = baseOf();
    if (typeof base === 'object') return base;
    op = { method, root, from, to, ...(base ? { base } : {}) };
  } else if (method === 'fs.undo') {
    const u = own('undoId');
    if (!validFsUndoId(u)) return no('undoId must be an undo id');
    op = { method, undoId: u };
  } else return no('not a writing fs method');
  const opId = own('op_id');
  if (typeof opId !== 'string' || !OP_ID.test(opId)) return no('op_id must be 1 to 64 of A-Z a-z 0-9 _ -');
  const control = own('control');
  if (!(control === undefined || control === null || typeof control === 'boolean')) return no('control must be a boolean');
  const st = own('stepup_at');
  if (!(st === undefined || st === null || (typeof st === 'number' && Number.isSafeInteger(st) && st >= 0))) return no('stepup_at must be a time in milliseconds');
  const by = own('by');
  if (!(by === undefined || by === null || (typeof by === 'string' && by.length <= 200 && !CONTROL.test(by)))) return no('by must be a short label');
  const flags: FsWebFlags = { control: control === true, ...(typeof st === 'number' ? { stepupAt: st } : {}), ...(typeof by === 'string' ? { by } : {}) };
  return { ok: true, op, opId, flags };
}
