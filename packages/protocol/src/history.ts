/**
 * Pane history (docs/protocol.md "Pane history", decision 0009).
 *
 * A runner that keeps history starts each SNAPSHOT with `ESC c` and then
 *
 *     ESC ] 777 ; tmuxedo-history ; start=S ; first=F ; end=E ; limit=L BEL
 *
 * so a viewer knows the snapshot's scrollback is history lines F … E-1 and
 * that older ones (back to S) can be read with `pane.read` source `history`.
 * The runner writes it (runner/crates/emu/src/snapshot.rs `history_osc`);
 * both sides are checked against fixtures/history.json.
 */

/** The OSC number and the first field of the history mark. */
export const HISTORY_OSC = 777;
export const HISTORY_OSC_TAG = 'tmuxedo-history';

export interface HistoryMark {
  /** The oldest line the runner still has. */
  start: number;
  /** The first line the snapshot wrote as scrollback. */
  first: number;
  /** One past the newest line. */
  end: number;
  /** The pane's line limit. */
  limit: number;
}

const FIELDS = ['start', 'first', 'end', 'limit'] as const;

/**
 * The mark from an OSC 777 payload as xterm.js hands it to a handler (the
 * text after `777;`), or null when it is not ours or is malformed.
 */
export function parseHistoryOsc(data: string): HistoryMark | null {
  const parts = data.split(';');
  if (parts[0] !== HISTORY_OSC_TAG) return null;
  const out: Partial<Record<(typeof FIELDS)[number], number>> = {};
  for (const kv of parts.slice(1)) {
    const eq = kv.indexOf('=');
    if (eq < 0) continue;
    const k = kv.slice(0, eq) as (typeof FIELDS)[number];
    if (!FIELDS.includes(k)) continue;
    const v = kv.slice(eq + 1);
    if (!/^\d{1,15}$/.test(v)) return null;
    out[k] = Number(v);
  }
  if (FIELDS.some((k) => out[k] === undefined)) return null;
  const m = out as HistoryMark;
  if (!(m.start <= m.first && m.first <= m.end)) return null;
  return m;
}

/** The OSC as the runner writes it (for tests and stand-ins). */
export function historyOsc(m: HistoryMark): string {
  return `\x1b]${HISTORY_OSC};${HISTORY_OSC_TAG};start=${m.start};first=${m.first};end=${m.end};limit=${m.limit}\x07`;
}

/** The runner method and source that page through history. */
export const HISTORY_METHOD = 'pane.read';
export const HISTORY_SOURCE = 'history';

/** Lines asked for per page: about one screenful of scrolling at a time, well under the reply limit. */
export const HISTORY_PAGE_LINES = 2000;

export interface HistoryPage {
  /** ANSI: each line followed by CRLF. */
  text: string;
  first: number;
  count: number;
  start: number;
  end: number;
  limit: number;
  more: boolean;
}

/** The params of a history page request ending right before line `before`. */
export function historyPageParams(paneId: string, before: number, lines = HISTORY_PAGE_LINES) {
  return { pane_id: paneId, source: HISTORY_SOURCE, before, lines, format: 'ansi' as const };
}

/**
 * A `pane.read` result (`res.result`) as a history page, or null when the
 * runner answered without history (an older runner, a demo machine).
 */
export function parseHistoryPage(result: unknown): HistoryPage | null {
  const read = (result as { read?: { text?: unknown; history?: Record<string, unknown> } } | null)?.read;
  const h = read?.history;
  if (!read || typeof read.text !== 'string' || !h) return null;
  const n = (k: string) => (typeof h[k] === 'number' && Number.isSafeInteger(h[k]) && (h[k] as number) >= 0 ? (h[k] as number) : null);
  const first = n('first'), count = n('count'), start = n('start'), end = n('end'), limit = n('limit');
  if (first === null || count === null || start === null || end === null || limit === null || typeof h.more !== 'boolean') return null;
  return { text: read.text, first, count, start, end, limit, more: h.more };
}
