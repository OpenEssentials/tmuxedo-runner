/**
 * Files and uploads (docs/protocol.md "Files", design files-and-uploads.md).
 *
 * A file travels between a person's device and a folder ON THE MACHINE,
 * outside every repository: uploads land in `<root>/<project>/inbox`, files
 * an agent leaves for the person sit in `<root>/<project>/outbox`. The bytes
 * cross the gateway in binary FILE frames (kind 5) that reuse the pane-frame
 * header, so the gateway routes them without reading them:
 *
 *     kind 5 · flags 0 · req = chunk number (upload) or read number (download)
 *     id = the transfer id ([A-Za-z0-9_-]{8,64}) · offset = byte offset in the file
 *
 * Everything here is pure and has no value imports (the app's Jest maps this
 * file alone). Tickets, which need node:crypto, are in file-ticket.ts.
 */

/** The hello feature of a runner that serves files (docs/protocol.md "Files"). */
export const FILES_FEATURE = 'files';
/** The kind byte of a FILE frame. */
export const KIND_FILE = 5;
/** Upload ids and download transfer ids. */
export const TRANSFER_ID = /^[A-Za-z0-9_-]{8,64}$/;

const KiB = 1024;
const MiB = 1024 * KiB;
const GiB = 1024 * MiB;

/** Chunk sizes (one PUT each; Cloudflare allows 100 MB a request, 100 s to answer). */
export const CHUNK_DEFAULT = 8 * MiB;
/** Picked by a client that expects a slow link (cellular, or < 150 KiB/s last time). */
export const CHUNK_SLOW = 2 * MiB;
export const CHUNK_MIN = 256 * KiB;
export const CHUNK_MAX = 16 * MiB;
/** Most chunks one upload may have. */
export const MAX_CHUNKS = 10_000;
/** Bytes of file data in one FILE frame (frames stay well under the 1 MiB cap). */
export const FILE_FRAME_PAYLOAD = 512 * KiB;
/** Most bytes one `files.read` returns (a download asks for windows of this). */
export const READ_WINDOW = 4 * MiB;
/** Largest upload by default, and the most the setting may allow. */
export const MAX_FILE_DEFAULT = 2 * GiB;
export const MAX_FILE_CAP = 10 * GiB;
/** Upload and download tickets live this long (a resumed upload asks for a fresh one). */
export const FILE_TICKET_TTL_SECONDS = 15 * 60;
/** Partial uploads are swept this long after their last chunk. */
export const PARTIAL_TTL_MS = 24 * 3600 * 1000;
/** Inbox and outbox files are swept this long after they arrived (unless pinned). */
export const RETENTION_MS = 14 * 24 * 3600 * 1000;
/** Upload inits per user per minute (web). */
export const UPLOAD_INITS_PER_MINUTE = 30;
/** Most characters of a stored file name. */
export const FILE_NAME_MAX = 120;

/** The two folders a project has. */
export type FilesDir = 'inbox' | 'outbox';
export const FILES_DIRS: readonly FilesDir[] = ['inbox', 'outbox'];

// ── FILE frames ─────────────────────────────────────────────────────────

export interface FileFrame {
  /** The upload id, or the gateway's download transfer id. */
  id: string;
  /** Upload: the chunk number. Download: the gateway's read number. */
  req: number;
  /** Byte offset in the file of the first payload byte. */
  offset: bigint;
  payload: Uint8Array;
}

const HEADER_FIXED = 1 + 1 + 4 + 1 + 8;
const FRAME_MAX = 1024 * 1024;

export function encodeFileFrame(f: FileFrame): Uint8Array {
  const id = new TextEncoder().encode(f.id);
  if (!TRANSFER_ID.test(f.id)) throw new RangeError('id must be a transfer id');
  if (f.req < 0 || f.req > 0xffffffff || !Number.isInteger(f.req)) throw new RangeError('req must be a u32');
  if (f.offset < 0n || f.offset > 0xffffffffffffffffn) throw new RangeError('offset must be a u64');
  const out = new Uint8Array(HEADER_FIXED + id.length + f.payload.length);
  if (out.length > FRAME_MAX) throw new RangeError('frame exceeds 1 MiB');
  const v = new DataView(out.buffer);
  v.setUint8(0, KIND_FILE);
  v.setUint8(1, 0);
  v.setUint32(2, f.req);
  v.setUint8(6, id.length);
  out.set(id, 7);
  v.setBigUint64(7 + id.length, f.offset);
  out.set(f.payload, HEADER_FIXED + id.length);
  return out;
}

/** Parse a FILE frame, or null for anything else (never throws). */
export function decodeFileFrame(buf: Uint8Array): FileFrame | null {
  if (buf.length < HEADER_FIXED + 1 || buf.length > FRAME_MAX) return null;
  const v = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  if (v.getUint8(0) !== KIND_FILE || v.getUint8(1) !== 0) return null;
  const idLen = v.getUint8(6);
  if (idLen < 8 || idLen > 64 || buf.length < HEADER_FIXED + idLen) return null;
  let id: string;
  try {
    id = new TextDecoder('utf-8', { fatal: true }).decode(buf.subarray(7, 7 + idLen));
  } catch {
    return null;
  }
  if (!TRANSFER_ID.test(id)) return null;
  return { id, req: v.getUint32(2), offset: v.getBigUint64(7 + idLen), payload: buf.subarray(HEADER_FIXED + idLen) };
}

/** Is this binary frame a FILE frame (cheap check before decoding)? */
export function isFileFrame(buf: Uint8Array): boolean {
  return buf.length > 0 && buf[0] === KIND_FILE;
}

// ── chunk arithmetic ────────────────────────────────────────────────────

export function chunkCount(size: number, chunkSize: number): number {
  return size <= 0 ? 0 : Math.ceil(size / chunkSize);
}

/** Bytes in chunk `n` (the last one may be short); -1 when n is out of range. */
export function chunkLength(size: number, chunkSize: number, n: number): number {
  const count = chunkCount(size, chunkSize);
  if (!Number.isInteger(n) || n < 0 || n >= count) return -1;
  return n === count - 1 ? size - n * chunkSize : chunkSize;
}

/** Is `(size, chunkSize)` an upload the protocol allows (before any quota)? */
export function validUploadShape(size: unknown, chunkSize: unknown, maxFile = MAX_FILE_CAP): boolean {
  return (
    typeof size === 'number' && Number.isSafeInteger(size) && size >= 0 && size <= maxFile &&
    typeof chunkSize === 'number' && Number.isSafeInteger(chunkSize) && chunkSize >= CHUNK_MIN && chunkSize <= CHUNK_MAX &&
    chunkCount(size, chunkSize) <= MAX_CHUNKS
  );
}

export const SHA256_HEX = /^[0-9a-f]{64}$/;

// ── file names ──────────────────────────────────────────────────────────

/**
 * The name an upload is stored under (before the time stamp the runner
 * adds), or null when nothing usable is left. The runner applies the same
 * rules (`tmuxedo_protocol::files::sanitize_name`); fixtures/files.json holds
 * both to one list.
 *
 * - only the last path component (`/` and `\` both separate);
 * - controls (C0, C1, DEL), bidi overrides and isolates, zero-width marks
 *   and BOM are removed;
 * - whitespace and `<>:"|?*'`$` become `_`, so the path can be typed into
 *   a prompt or a shell without quoting (and saved on Windows later); runs
 *   of `_` collapse;
 * - leading dots, `_` and `-` go (no hidden files, no option-like names);
 *   trailing dots and `_` go;
 * - at most FILE_NAME_MAX characters, keeping an extension of up to 16.
 */
export function sanitizeFileName(raw: unknown): string | null {
  if (typeof raw !== 'string') return null;
  const last = raw.split(/[/\\]/).pop() ?? '';
  let s = '';
  for (const ch of last) {
    const c = ch.codePointAt(0)!;
    if (c < 0x20 || (c >= 0x7f && c <= 0x9f)) {
      if (c === 0x09 || c === 0x0a || c === 0x0d) s += '_';
      continue;
    }
    if ((c >= 0x202a && c <= 0x202e) || (c >= 0x2066 && c <= 0x2069) || c === 0x200b || c === 0x200e || c === 0x200f || c === 0xfeff || c === 0x061c) continue;
    s += /\s/u.test(ch) || '<>:"|?*\'`$'.includes(ch) ? '_' : ch;
  }
  s = s.replace(/_{2,}/g, '_').replace(/^[._-]+/, '').replace(/[._]+$/, '');
  if (!s) return null;
  const chars = [...s];
  if (chars.length > FILE_NAME_MAX) {
    const dot = s.lastIndexOf('.');
    const ext = dot > 0 ? [...s.slice(dot)] : [];
    const keepExt = ext.length > 1 && ext.length <= 16 ? ext : [];
    s = chars.slice(0, FILE_NAME_MAX - keepExt.length).join('').replace(/[._]+$/, '') + keepExt.join('');
  }
  return s || null;
}

/** The stamp prefix of a stored upload: `2026-10-03T14-02-11Z`. */
export function uploadStamp(at: Date): string {
  return at.toISOString().replace(/\.\d{3}Z$/, 'Z').replace(/:/g, '-');
}

// ── HTTP Range (downloads) ──────────────────────────────────────────────

export type RangeResult = { start: number; end: number } | 'unsatisfiable' | null;

/**
 * One `bytes=` range against a file of `size` bytes: inclusive `{start, end}`,
 * 'unsatisfiable' (416), or null (no usable Range header: send the whole file).
 * Multiple ranges are not served (null: the whole file, as RFC 9110 allows).
 */
export function parseRange(header: string | null | undefined, size: number): RangeResult {
  if (!header) return null;
  const m = /^\s*bytes\s*=\s*(\d*)\s*-\s*(\d*)\s*$/i.exec(header);
  if (!m) return null;
  const [, a, b] = m;
  if (a === '' && b === '') return null;
  if (a === '') {
    const n = Number(b);
    if (!Number.isSafeInteger(n) || n === 0) return 'unsatisfiable';
    if (size === 0) return 'unsatisfiable';
    return { start: Math.max(0, size - n), end: size - 1 };
  }
  const start = Number(a);
  if (!Number.isSafeInteger(start)) return 'unsatisfiable';
  if (start >= size) return 'unsatisfiable';
  let end = b === '' ? size - 1 : Number(b);
  if (!Number.isSafeInteger(end) || end < start) return b === '' ? 'unsatisfiable' : null;
  end = Math.min(end, size - 1);
  return { start, end };
}

// ── disk ────────────────────────────────────────────────────────────────

/**
 * How full the machine's home disk is (design §5): `ok`; `low` under 10 %
 * free (banner, uploads refused); `critical` under 5 % (pane history
 * stops, a durable `disk_low` event); `alert` under 2 % (the operator's
 * alert). The runner computes the same levels.
 */
export type DiskLevel = 'ok' | 'low' | 'critical' | 'alert';

export function diskLevel(freeBytes: number, totalBytes: number): DiskLevel {
  if (!(totalBytes > 0)) return 'ok';
  const r = freeBytes / totalBytes;
  return r < 0.02 ? 'alert' : r < 0.05 ? 'critical' : r < 0.1 ? 'low' : 'ok';
}

/** Uploads are refused when less than this would be left after them. */
export function freeSpaceFloor(totalBytes: number): number {
  return Math.max(2 * GiB, Math.floor(totalBytes * 0.1));
}

/** The runner's `disk` frame (and `files.quota`'s disk part). */
export interface DiskState {
  freeBytes: number;
  totalBytes: number;
  filesBytes: number;
  level: DiskLevel;
}

/** A valid DiskState or null (never throws). */
export function parseDisk(v: unknown): DiskState | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  const n = (x: unknown) => typeof x === 'number' && Number.isSafeInteger(x) && x >= 0;
  if (!n(o.freeBytes) || !n(o.totalBytes) || !n(o.filesBytes)) return null;
  if (o.level !== 'ok' && o.level !== 'low' && o.level !== 'critical' && o.level !== 'alert') return null;
  return { freeBytes: o.freeBytes as number, totalBytes: o.totalBytes as number, filesBytes: o.filesBytes as number, level: o.level };
}

// ── listings ────────────────────────────────────────────────────────────

/** One file in `files.list` (and the CLI's `tmuxedo files list`). */
export interface FileEntry {
  name: string;
  dir: FilesDir;
  /** The absolute path on the machine (what "Insert path" types). */
  path: string;
  size: number;
  /** Unix ms. */
  mtime: number;
  pinned: boolean;
  /** Inbox: who uploaded it and from where (the provenance sidecar). */
  source?: 'web' | 'app' | 'cli' | 'agent';
  sha256?: string;
  /** Outbox: the note `tmuxedo files share --note` left. */
  note?: string;
  /** Unix ms when the retention sweep removes it (absent when pinned). */
  expiresAt?: number;
}

/** What an agent CLI is told to type for a path: `@path` (Claude, Cursor) or the plain path (Codex). */
export function insertPathText(path: string, agent: string | null | undefined): string {
  const quoted = /[\s'"\\$`]/.test(path) ? `'${path.replace(/'/g, `'\\''`)}'` : path;
  return agent === 'claude' || agent === 'cursor' ? `@${quoted} ` : `${quoted} `;
}
