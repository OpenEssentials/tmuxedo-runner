/**
 * `glance` frames (docs/protocol.md "Glances"): a pane's VISIBLE screen as
 * styled text runs, for dashboard thumbnails (docs/design/dashboard-and-
 * terminal-page.md §2.2, decision D-15). The runner encodes them from the
 * same emulator that SNAPSHOT and `pane.read` read (so they pass the same
 * output choke point), never from scrollback; this module decodes them.
 *
 * WIRE. `lines` has one entry per screen row; each row is an array of runs
 * `[text, fg?, bg?, attrs?]` with trailing default values left out:
 *
 *   fg, bg   null = the terminal's default colour, 0..255 = palette index,
 *            "#rrggbb" = true colour
 *   attrs    bit 0 bold, 1 dim, 2 italic, 3 underline, 4 inverse
 *
 * Trailing blank cells (a space with the default background, no underline
 * or inverse) are dropped, so a blank row is `[]`. A wide character (CJK,
 * most emoji) covers two columns.
 *
 * Golden fixtures: ../fixtures/glance.json is checked here (decode) and by the
 * runner (`runner/crates/runner/src/glance.rs`, encode from the same input
 * bytes), so the encoder and the renderer cannot drift.
 *
 * This file imports nothing (the app's Jest and esbuild map it on its own);
 * rate limits and the hello feature name live in frames.ts.
 */

export const GLANCE_ATTR = { bold: 1, dim: 2, italic: 4, underline: 8, inverse: 16 } as const;
/** The runner crops a glance to at most this many rows (the bottom ones) and columns (the left ones). */
export const GLANCE_MAX_ROWS = 60;
export const GLANCE_MAX_COLS = 200;

/** A colour on the wire: null (default), palette index, or "#rrggbb". */
export type GlanceColor = null | number | string;
/** A run as sent. */
export type GlanceWireRun = [string, GlanceColor?, GlanceColor?, number?];

export interface GlanceRun { text: string; fg: GlanceColor; bg: GlanceColor; attrs: number }

export interface GlanceFrame {
  t: 'glance';
  /** Added by the gateway on the way to a client. */
  machineId?: string;
  paneId: string;
  /** Per pane, increases with every glance the runner sends (resets with the runner). */
  rev: number;
  cols: number;
  rows: number;
  cursor?: { x: number; y: number };
  lines: GlanceWireRun[][];
}

export interface Glance {
  machineId?: string;
  paneId: string;
  rev: number;
  cols: number;
  rows: number;
  cursor: { x: number; y: number } | null;
  lines: GlanceRun[][];
}

const isColor = (v: unknown): v is GlanceColor =>
  v === null || v === undefined || (typeof v === 'number' && Number.isInteger(v) && v >= 0 && v <= 255) || (typeof v === 'string' && /^#[0-9a-f]{6}$/.test(v));

const isDim = (v: unknown, max: number): v is number => typeof v === 'number' && Number.isInteger(v) && v >= 1 && v <= max;

/**
 * Decode and validate a glance frame (from the runner, or from the gateway
 * on a client). Never throws: junk is null. Runs with bad parts are dropped,
 * rows beyond `rows` are ignored.
 */
export function decodeGlance(f: unknown): Glance | null {
  if (!f || typeof f !== 'object') return null;
  const o = f as Record<string, unknown>;
  if (typeof o.paneId !== 'string' || !o.paneId || o.paneId.length > 255) return null;
  if (typeof o.rev !== 'number' || !Number.isSafeInteger(o.rev) || o.rev < 0) return null;
  if (!isDim(o.cols, 1000) || !isDim(o.rows, 1000) || !Array.isArray(o.lines)) return null;
  const c = o.cursor as { x?: unknown; y?: unknown } | undefined;
  const cursor = c && typeof c === 'object' && Number.isInteger(c.x) && Number.isInteger(c.y) && (c.x as number) >= 0 && (c.y as number) >= 0
    ? { x: c.x as number, y: c.y as number } : null;
  const lines: GlanceRun[][] = [];
  for (const row of (o.lines as unknown[]).slice(0, o.rows)) {
    const out: GlanceRun[] = [];
    if (Array.isArray(row)) {
      for (const run of row) {
        if (!Array.isArray(run) || typeof run[0] !== 'string' || !isColor(run[1]) || !isColor(run[2])) continue;
        const attrs = run[3] === undefined ? 0 : run[3];
        if (typeof attrs !== 'number' || !Number.isInteger(attrs) || attrs < 0 || attrs > 31) continue;
        out.push({ text: run[0], fg: (run[1] ?? null) as GlanceColor, bg: (run[2] ?? null) as GlanceColor, attrs });
      }
    }
    lines.push(out);
  }
  while (lines.length < o.rows) lines.push([]);
  return {
    ...(typeof o.machineId === 'string' ? { machineId: o.machineId } : {}),
    paneId: o.paneId, rev: o.rev, cols: o.cols, rows: o.rows, cursor, lines,
  };
}

/** Encode runs the way the runner does (trailing defaults left out). For tests and fakes. */
export function encodeRun(r: GlanceRun): GlanceWireRun {
  const out: GlanceWireRun = [r.text, r.fg, r.bg, r.attrs];
  while (out.length > 1) {
    const last = out[out.length - 1];
    if (last === null || last === undefined || last === 0) out.pop();
    else break;
  }
  return out;
}

/**
 * Columns a character covers: 2 for East Asian wide and fullwidth forms and
 * most emoji, 0 for combining marks, else 1. The same ranges as the runner's
 * emulator in practice (it reports wide cells; it is the source of truth).
 */
export function charWidth(cp: number): number {
  if (cp === 0) return 0;
  if ((cp >= 0x300 && cp <= 0x36f) || (cp >= 0x200b && cp <= 0x200f) || (cp >= 0xfe00 && cp <= 0xfe0f)) return 0;
  if (
    (cp >= 0x1100 && cp <= 0x115f) || (cp >= 0x2e80 && cp <= 0x303e) || (cp >= 0x3041 && cp <= 0x33ff) ||
    (cp >= 0x3400 && cp <= 0x4dbf) || (cp >= 0x4e00 && cp <= 0x9fff) || (cp >= 0xa000 && cp <= 0xa4cf) ||
    (cp >= 0xac00 && cp <= 0xd7a3) || (cp >= 0xf900 && cp <= 0xfaff) || (cp >= 0xfe30 && cp <= 0xfe4f) ||
    (cp >= 0xff00 && cp <= 0xff60) || (cp >= 0xffe0 && cp <= 0xffe6) || (cp >= 0x1f300 && cp <= 0x1f64f) ||
    (cp >= 0x1f900 && cp <= 0x1f9ff) || (cp >= 0x20000 && cp <= 0x3fffd)
  ) return 2;
  return 1;
}

/** Each run with its starting column. */
export function runColumns(line: GlanceRun[]): Array<{ run: GlanceRun; col: number; width: number }> {
  let col = 0;
  return line.map((run) => {
    let w = 0;
    for (const ch of run.text) w += charWidth(ch.codePointAt(0)!);
    const at = col;
    col += w;
    return { run, col: at, width: w };
  });
}

/** The visible screen as plain text, one string per row (trailing blanks already trimmed by the runner). */
export function glanceText(g: Pick<Glance, 'lines'>): string[] {
  return g.lines.map((l) => l.map((r) => r.text).join(''));
}

// ── the per-client crop (docs/protocol.md "Glances", "Crop and rate per socket") ──

/** A client's crop of a glance: at most this many rows (the bottom ones with content) and columns (the left ones). */
export interface GlanceCrop { rows: number; cols: number }

/** Zero-width characters kept after one cell's character; more are dropped (a cropped frame's size stays bounded by its cells). */
export const GLANCE_CROP_MAX_MARKS = 8;

/** A row with something to see: a character that is not a space, or a space with a background or inverse. */
const rowHasContent = (line: GlanceRun[]) =>
  line.some((r) => r.text.trim() !== '' || (r.text !== '' && (r.bg !== null || (r.attrs & GLANCE_ATTR.inverse) !== 0)));

/** C0 and C1 control characters and DEL: an emulator's cells never hold them, so a crop never passes them on. */
const isControl = (cp: number) => cp < 0x20 || (cp >= 0x7f && cp <= 0x9f);

/**
 * Crop a decoded glance for one client: the bottom `rows` rows that have
 * content (trailing blank rows go first; the cursor's row counts as content)
 * and the left `cols` columns. Cut on cell boundaries: a character that does
 * not fit is left out whole and ends its row, so a wide character is never
 * split and a combining mark stays with its character. Colours and
 * attributes are kept as sent. The result is a glance frame like any other:
 * `rows` is the number of rows kept (1 with one empty row when the screen is
 * blank), `cols` the smaller of the pane's and the crop's. `cursor` is moved
 * to its row in the crop and left out when that row is not in it; its `x` is
 * the pane's own, so a cursor right of the crop has `x >= cols`.
 */
export function cropGlance(g: Glance, crop: GlanceCrop): GlanceFrame {
  const rows = Math.max(1, Math.min(Math.floor(crop.rows) || 1, GLANCE_MAX_ROWS));
  const cols = Math.max(1, Math.min(Math.floor(crop.cols) || 1, GLANCE_MAX_COLS));
  const head = { t: 'glance' as const, ...(g.machineId !== undefined ? { machineId: g.machineId } : {}), paneId: g.paneId, rev: g.rev, cols: Math.min(g.cols, cols) };
  let last = -1;
  for (let i = g.lines.length - 1; i >= 0; i--) {
    if (rowHasContent(g.lines[i]!)) { last = i; break; }
  }
  const cy = g.cursor && g.cursor.y < g.lines.length ? g.cursor.y : -1;
  last = Math.max(last, cy);
  if (last < 0) return { ...head, rows: 1, lines: [[]] };
  const first = Math.max(0, last - rows + 1);
  const lines: GlanceWireRun[][] = [];
  for (let i = first; i <= last; i++) {
    const out: GlanceWireRun[] = [];
    let left = cols;
    let marks = 0;
    let full = false;
    for (const r of g.lines[i]!) {
      let text = '';
      for (const ch of r.text) {
        const cp = ch.codePointAt(0)!;
        if (isControl(cp)) continue;
        const w = charWidth(cp);
        if (w === 0) {
          if (marks++ < GLANCE_CROP_MAX_MARKS) text += ch;
          continue;
        }
        if (w > left) { full = true; break; }
        text += ch;
        left -= w;
        marks = 0;
      }
      if (text) out.push(encodeRun({ ...r, text }));
      if (full) break;
    }
    lines.push(out);
  }
  // Kept with its own column even right of the crop: its row counted as content, and a client must see why.
  const cursor = g.cursor && cy >= first ? { x: g.cursor.x, y: cy - first } : null;
  return { ...head, rows: lines.length, ...(cursor ? { cursor } : {}), lines };
}

/** The last non-blank row, for labels and the old-runner fallback. */
export function glanceLastLine(g: Pick<Glance, 'lines'>): string {
  const text = glanceText(g);
  for (let i = text.length - 1; i >= 0; i--) if (text[i]!.trim()) return text[i]!.trim();
  return '';
}
