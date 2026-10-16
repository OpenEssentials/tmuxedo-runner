/**
 * Names people give workspaces, tabs and panes (queue 3c, docs/protocol.md
 * "Names"). ONE rule, implemented here and in the runner
 * (runner/crates/protocol/src/names.rs) and checked by both against
 * fixtures/names.json, so a name typed in the web, the app or the TUI is
 * stored and shown the same everywhere.
 *
 *   1. Whitespace of any kind (tab, newline, no-break and wide spaces) is a
 *      plain space.
 *   2. Control and invisible format characters are removed: C0 and C1
 *      controls, DEL, the bidi overrides and isolates (which could make a
 *      name read backwards next to the machine name in a push title), zero
 *      width space and the BOM. ZWJ and variation selectors stay (emoji).
 *   3. Runs of spaces become one; the ends are trimmed.
 *   4. At most NAME_MAX characters (Unicode code points; a cut never splits
 *      a surrogate pair), trimmed again.
 *
 * An empty result means "go back to the default name": a workspace's is
 * `default` for w1 and its number otherwise, a tab's is its number, and a
 * pane has none (its title is shown).
 */

export const NAME_MAX = 64;

/** The hello feature of a runner that applies these rules and sends `Pane.label` in its topology. */
export const NAMES_FEATURE = 'names';

/** Code points treated as a space (Unicode White_Space minus the C1 NEL, which is removed). */
const SPACE = new Set([
  0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x20, 0xa0, 0x1680,
  0x2000, 0x2001, 0x2002, 0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200a,
  0x2028, 0x2029, 0x202f, 0x205f, 0x3000,
]);

/** Removed outright. */
function dropped(cp: number): boolean {
  return cp < 0x20 || (cp >= 0x7f && cp <= 0x9f)
    || cp === 0x200b || cp === 0x200e || cp === 0x200f
    || (cp >= 0x202a && cp <= 0x202e) || (cp >= 0x2066 && cp <= 0x2069)
    || cp === 0xfeff;
}

/** A name as stored: see the rules above. Anything that is not a string is "". */
export function cleanName(raw: unknown): string {
  if (typeof raw !== 'string') return '';
  let out = '';
  let space = false;
  for (const ch of raw) {
    const cp = ch.codePointAt(0)!;
    if (SPACE.has(cp)) { space = out.length > 0; continue; }
    if (dropped(cp)) continue;
    if (space) { out += ' '; space = false; }
    out += ch;
  }
  const cps = [...out];
  return cps.length > NAME_MAX ? cps.slice(0, NAME_MAX).join('').trimEnd() : out;
}

export type NameKind = 'workspace' | 'tab' | 'pane';

/**
 * The name a runner gives when none is set: a workspace `w<n>` is `default`
 * for n = 1, else `n`; a tab `w<n>:t<m>` is `m`; a pane has none (null).
 */
export function defaultName(kind: NameKind, id: string): string | null {
  if (kind === 'pane') return null;
  if (kind === 'workspace') {
    const n = /^w(\d+)$/.exec(id)?.[1];
    return n === undefined ? id : n === '1' ? 'default' : String(Number(n));
  }
  const m = /:t(\d+)$/.exec(id)?.[1];
  return m === undefined ? id : String(Number(m));
}

/** What a rename stores: the clean name, or the default when it is empty (null for a pane). */
export function storedName(kind: NameKind, id: string, raw: unknown): string | null {
  const n = cleanName(raw);
  return n || defaultName(kind, id);
}

/** The runner API method that renames each kind. */
export const RENAME_METHOD: Record<NameKind, 'workspace.rename' | 'tab.rename' | 'pane.rename'> = {
  workspace: 'workspace.rename', tab: 'tab.rename', pane: 'pane.rename',
};

/** The id parameter each rename method takes. */
export const RENAME_ID_PARAM: Record<NameKind, 'workspace_id' | 'tab_id' | 'pane_id'> = {
  workspace: 'workspace_id', tab: 'tab_id', pane: 'pane_id',
};

/** A pane's name for people: its label, else its title, else its id. */
export function paneDisplayName(p: { id?: string; paneId?: string; label?: string | null; title?: string | null }): string {
  return (p.label || p.title || p.id || p.paneId || '').trim();
}
