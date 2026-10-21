/**
 * Plugin items and settings (docs/protocol.md "Plugin settings"): what an
 * agent CLI is given besides its own configuration (MCP servers, skills,
 * sub-agents, commands, instruction blocks) and each plugin's settings, per
 * scope, and the EFFECTIVE SET a machine's runner gets in `config.pluginSet`.
 *
 * Three things live here because every side needs the same answer:
 *
 *   - the shape of an item's definition per kind (`validateItemDefinition`);
 *   - the resolver: which items and settings apply to a pane on a machine,
 *     in a project (`effectivePlugin`), and the wire set built from it
 *     (`buildPluginSet`);
 *   - the wire parser (`parsePluginSet`), mirrored in Rust
 *     (runner/crates/protocol/src/plugin_set.rs) on fixtures/plugin-set.json.
 *
 * SECRETS never appear here. A definition names a secret (`{"$secret":
 * "GH_TOKEN"}`); the runner asks for the value when it launches the agent.
 *
 * PRECEDENCE. Scopes are user → machine → project, narrowest last.
 *   1. An item is visible where it is defined: everywhere (user), on one
 *      machine, or in one project.
 *   2. Of two visible items of the same kind and name, the narrower
 *      definition is the item there; the other is shadowed.
 *   3. An item is off until it is enabled for a plugin. Of the enable /
 *      disable rows that apply, the narrowest decides. A row broader than
 *      the item's own scope is ignored.
 *   4. Settings merge per key, one level into sections, narrowest last.
 */

export type PluginItemKind = 'mcp' | 'skill' | 'subagent' | 'command' | 'instructions';
export const PLUGIN_ITEM_KINDS: readonly PluginItemKind[] = ['mcp', 'skill', 'subagent', 'command', 'instructions'];
/**
 * The kinds `config.pluginSet` carries: the user's items and curated add-ons
 * (src/addon.ts). An add-on is never created from the item form, so it is
 * not in PLUGIN_ITEM_KINDS.
 */
export type PluginSetKind = PluginItemKind | 'addon';
export const PLUGIN_SET_KINDS: readonly PluginSetKind[] = [...PLUGIN_ITEM_KINDS, 'addon'];
export type PluginScope = 'user' | 'machine' | 'project';
export const PLUGIN_SCOPES: readonly PluginScope[] = ['user', 'machine', 'project'];
const RANK: Record<PluginScope, number> = { user: 0, machine: 1, project: 2 };

/** Lower-case letters, digits and `-`: safe as a file name and a TOML key in every CLI. */
export const PLUGIN_ITEM_NAME = /^[a-z0-9][a-z0-9-]{0,39}$/;
export const PLUGIN_ITEM_ID = /^[A-Za-z0-9_-]{1,64}$/;
export const SECRET_NAME = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;
const ENV_NAME = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
const HEADER_NAME = /^[A-Za-z0-9!#$%&'*+.^_`|~-]{1,64}$/;
const PLUGIN = /^[a-z0-9-]{2,32}$/;

/** Markdown bodies (skill, sub-agent, command, instructions), in UTF-8 bytes. */
export const MAX_ITEM_BODY_BYTES = 64 * 1024;
/** One definition, serialised. */
export const MAX_ITEM_BYTES = 80 * 1024;
/** Items in one `config.pluginSet`. */
export const MAX_PLUGIN_SET_ITEMS = 200;
/** Projects in one `config.pluginSet`. */
export const MAX_PLUGIN_SET_PROJECTS = 200;
export const MAX_ARGV = 64;
export const MAX_ARG_BYTES = 4096;
export const MAX_ENV = 64;
export const MAX_TOOLS = 128;

export interface SecretRef { $secret: string }
export type TextOrSecret = string | SecretRef;
export interface McpStdioDefinition { transport: 'stdio'; command: string[]; env?: Record<string, TextOrSecret>; tools?: string[] }
export interface McpHttpDefinition { transport: 'http'; url: string; headers?: Record<string, TextOrSecret>; bearer?: SecretRef; tools?: string[] }
export type McpDefinition = McpStdioDefinition | McpHttpDefinition;
export interface SkillDefinition { description: string; body: string }
export interface SubagentDefinition { description: string; instructions: string; model?: string; tools?: string[] }
export interface CommandDefinition { description?: string; body: string }
export interface InstructionsDefinition { body: string }
export type PluginItemDefinition = McpDefinition | SkillDefinition | SubagentDefinition | CommandDefinition | InstructionsDefinition;

export interface ItemIssue { path: string; code: string; message: string }
export type ItemResult = { ok: true; value: PluginItemDefinition } | { ok: false; issues: ItemIssue[] };

const enc = new TextEncoder();
const bytes = (s: string) => enc.encode(s).length;
const isObj = (v: unknown): v is Record<string, unknown> => !!v && typeof v === 'object' && !Array.isArray(v);
// eslint-disable-next-line no-control-regex
const oneLine = (s: string) => !/[\x00-\x1f\x7f]/.test(s);
// eslint-disable-next-line no-control-regex
const cleanText = (s: string) => !/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/.test(s);

class Check {
  issues: ItemIssue[] = [];
  constructor(private o: Record<string, unknown>, allowed: readonly string[]) {
    for (const k of Object.keys(o)) if (!allowed.includes(k)) this.add(k, 'unknown', `"${k.slice(0, 40)}" is not part of this item.`);
  }
  add(path: string, code: string, message: string) { this.issues.push({ path, code, message }); }
  line(key: string, max: number, required: boolean): string | undefined {
    const v = this.o[key];
    if (v === undefined && !required) return undefined;
    if (typeof v !== 'string' || !v.trim() || bytes(v) > max || !oneLine(v)) { this.add(key, 'invalid', `${key} is one line of text, up to ${max} bytes.`); return undefined; }
    return v;
  }
  body(key: string): string | undefined {
    const v = this.o[key];
    if (typeof v !== 'string' || !v.trim() || !cleanText(v)) { this.add(key, 'invalid', `${key} is text (Markdown).`); return undefined; }
    if (bytes(v) > MAX_ITEM_BODY_BYTES) { this.add(key, 'too_large', `${key} is larger than 64 KiB.`); return undefined; }
    return v;
  }
  tools(): string[] | undefined {
    const v = this.o.tools;
    if (v === undefined) return undefined;
    if (!Array.isArray(v) || v.length > MAX_TOOLS || !v.every((t) => typeof t === 'string' && /^[A-Za-z0-9_.:*-]{1,128}$/.test(t)) || new Set(v).size !== v.length) {
      this.add('tools', 'invalid', `tools is a list of up to ${MAX_TOOLS} different tool names.`);
      return undefined;
    }
    return v as string[];
  }
  secret(path: string, v: unknown): SecretRef | undefined {
    if (isObj(v) && Object.keys(v).length === 1 && typeof v.$secret === 'string' && SECRET_NAME.test(v.$secret)) return { $secret: v.$secret };
    this.add(path, 'secret', 'Name a secret, like {"$secret": "GH_TOKEN"}.');
    return undefined;
  }
  /** {NAME: text | {$secret}}: plain text is one line, at most 4 KiB. */
  map(key: string, name: RegExp): Record<string, TextOrSecret> | undefined {
    const v = this.o[key];
    if (v === undefined) return undefined;
    if (!isObj(v) || Object.keys(v).length > MAX_ENV) { this.add(key, 'invalid', `${key} is up to ${MAX_ENV} named values.`); return undefined; }
    const out: Record<string, TextOrSecret> = {};
    for (const [k, x] of Object.entries(v)) {
      const at = `${key}.${k.slice(0, 64)}`;
      if (!name.test(k)) { this.add(at, 'name', 'This name is not allowed.'); continue; }
      if (typeof x === 'string') {
        if (bytes(x) > MAX_ARG_BYTES || !oneLine(x)) this.add(at, 'invalid', 'A value is one line of text, up to 4 KiB.');
        else out[k] = x;
      } else {
        const s = this.secret(at, x);
        if (s) out[k] = s;
      }
    }
    return out;
  }
}

function mcp(o: Record<string, unknown>): { c: Check; value: McpDefinition } {
  if (o.transport === 'http') {
    const c = new Check(o, ['transport', 'url', 'headers', 'bearer', 'tools']);
    let url = '';
    if (typeof o.url === 'string' && o.url.length <= 2048 && /^https?:\/\/[^\s/]/.test(o.url) && oneLine(o.url) && !/\s/.test(o.url) && URL.canParse(o.url) && !new URL(o.url).username && !new URL(o.url).password) url = o.url;
    else c.add('url', 'invalid', 'url is an http or https address without a user name or password in it.');
    const headers = c.map('headers', HEADER_NAME);
    const bearer = o.bearer === undefined ? undefined : c.secret('bearer', o.bearer);
    const tools = c.tools();
    return { c, value: { transport: 'http', url, ...(headers ? { headers } : {}), ...(bearer ? { bearer } : {}), ...(tools ? { tools } : {}) } };
  }
  const c = new Check(o, ['transport', 'command', 'env', 'tools']);
  if (o.transport !== 'stdio') c.add('transport', 'invalid', 'transport is "stdio" (a local command) or "http" (a remote address).');
  let command: string[] = [];
  // An argv list, never a shell string: each element reaches the program as is.
  if (Array.isArray(o.command) && o.command.length >= 1 && o.command.length <= MAX_ARGV && o.command.every((a) => typeof a === 'string' && bytes(a) <= MAX_ARG_BYTES && oneLine(a)) && String(o.command[0]).trim()) command = o.command as string[];
  else c.add('command', 'invalid', `command is a list of 1 to ${MAX_ARGV} arguments (the program first), each one line.`);
  const env = c.map('env', ENV_NAME);
  const tools = c.tools();
  return { c, value: { transport: 'stdio', command, ...(env ? { env } : {}), ...(tools ? { tools } : {}) } };
}

/** Check a definition for its kind; unknown keys are refused, so a typo never silently does nothing. */
export function validateItemDefinition(kind: PluginItemKind, v: unknown): ItemResult {
  if (!isObj(v)) return { ok: false, issues: [{ path: '', code: 'invalid', message: 'A definition is an object.' }] };
  let c: Check;
  let value: PluginItemDefinition;
  if (kind === 'mcp') ({ c, value } = mcp(v));
  else if (kind === 'skill') {
    c = new Check(v, ['description', 'body']);
    value = { description: c.line('description', 1024, true) ?? '', body: c.body('body') ?? '' };
  } else if (kind === 'subagent') {
    c = new Check(v, ['description', 'instructions', 'model', 'tools']);
    const model = c.line('model', 64, false);
    const tools = c.tools();
    value = { description: c.line('description', 1024, true) ?? '', instructions: c.body('instructions') ?? '', ...(model ? { model } : {}), ...(tools ? { tools } : {}) };
  } else if (kind === 'command') {
    c = new Check(v, ['description', 'body']);
    const description = c.line('description', 1024, false);
    value = { ...(description ? { description } : {}), body: c.body('body') ?? '' };
  } else if (kind === 'instructions') {
    c = new Check(v, ['body']);
    value = { body: c.body('body') ?? '' };
  } else return { ok: false, issues: [{ path: 'kind', code: 'invalid', message: 'Unknown kind of item.' }] };
  if (!c.issues.length && bytes(JSON.stringify(value)) > MAX_ITEM_BYTES) c.add('', 'too_large', 'This item is larger than 80 KiB.');
  return c.issues.length ? { ok: false, issues: c.issues } : { ok: true, value };
}

/** The secrets a definition names (sorted, each once): for the audit row and the secret's "used by". */
export function definitionSecrets(def: unknown): string[] {
  const out = new Set<string>();
  const walk = (v: unknown, depth: number) => {
    if (depth > 4 || !v || typeof v !== 'object') return;
    if (isObj(v) && typeof v.$secret === 'string' && Object.keys(v).length === 1) { out.add(v.$secret); return; }
    for (const x of Object.values(v)) walk(x, depth + 1);
  };
  walk(def, 0);
  return [...out].sort();
}

// ── the resolver ─────────────────────────────────────────────────────────

export interface ScopeAt { scope: PluginScope; key: string }
export interface PluginItemRow extends ScopeAt { id: string; kind: PluginItemKind; name: string; version: number; definition: unknown }
export interface PluginItemStateRow extends ScopeAt { itemId: string; plugin: string; enabled: boolean }
export interface PluginSettingsRow extends ScopeAt { plugin: string; values: Record<string, unknown> }
export interface PluginRows { items: readonly PluginItemRow[]; states: readonly PluginItemStateRow[]; settings: readonly PluginSettingsRow[] }
/** A pane's place: its machine and, when its folder is in one, its project. */
export interface PluginTarget { machineId: string; projectKey?: string | null }

/** Why an item is, or is not, in the effective set. */
export interface ItemDecision {
  id: string; kind: PluginItemKind; name: string; version: number;
  definedAt: ScopeAt;
  enabled: boolean;
  /** The enable / disable row that decided; null = never enabled for this plugin here. */
  decidedAt: ScopeAt | null;
  /** The narrower item of the same kind and name that takes this one's place here. */
  shadowedBy: string | null;
}
export interface EffectivePlugin {
  plugin: string;
  /** Ids of the items the plugin gets, by kind, then name. */
  items: string[];
  values: Record<string, unknown>;
  /** Every item visible here, effective or not (the Plugins page explains with it). */
  decisions: ItemDecision[];
}

const applies = (s: ScopeAt, t: PluginTarget) => s.scope === 'user' || (s.scope === 'machine' ? s.key === t.machineId : !!t.projectKey && s.key === t.projectKey);
const at = (s: ScopeAt): ScopeAt => ({ scope: s.scope, key: s.scope === 'user' ? '' : s.key });
const order = (a: { kind: PluginItemKind; name: string; id: string }, b: { kind: PluginItemKind; name: string; id: string }) =>
  PLUGIN_ITEM_KINDS.indexOf(a.kind) - PLUGIN_ITEM_KINDS.indexOf(b.kind) || (a.name < b.name ? -1 : a.name > b.name ? 1 : a.id < b.id ? -1 : a.id > b.id ? 1 : 0);

/** Settings merge: per key, one level into sections (objects); lists and plain values replace. */
export function mergeSettings(base: Record<string, unknown>, over: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = { ...base };
  for (const [k, v] of Object.entries(over)) out[k] = isObj(v) && isObj(out[k]) ? { ...(out[k] as Record<string, unknown>), ...v } : v;
  return out;
}

/** What one plugin gets at a place (see PRECEDENCE above). */
export function effectivePlugin(rows: PluginRows, plugin: string, target: PluginTarget): EffectivePlugin {
  const visible = rows.items.filter((i) => applies(i, target));
  const winner = new Map<string, PluginItemRow>();
  for (const i of visible) {
    const k = `${i.kind}\n${i.name}`;
    const w = winner.get(k);
    if (!w || RANK[i.scope] > RANK[w.scope] || (RANK[i.scope] === RANK[w.scope] && i.id < w.id)) winner.set(k, i);
  }
  const decisions: ItemDecision[] = visible.map((i) => {
    const w = winner.get(`${i.kind}\n${i.name}`)!;
    let decided: PluginItemStateRow | null = null;
    for (const s of rows.states) {
      if (s.itemId !== i.id || s.plugin !== plugin || !applies(s, target) || RANK[s.scope] < RANK[i.scope]) continue;
      if (!decided || RANK[s.scope] > RANK[decided.scope]) decided = s;
    }
    return {
      id: i.id, kind: i.kind, name: i.name, version: i.version, definedAt: at(i),
      enabled: w.id === i.id && !!decided && decided.enabled,
      decidedAt: decided ? at(decided) : null,
      shadowedBy: w.id === i.id ? null : w.id,
    };
  }).sort(order);
  let values: Record<string, unknown> = {};
  const settings = rows.settings.filter((s) => s.plugin === plugin && applies(s, target)).sort((a, b) => RANK[a.scope] - RANK[b.scope]);
  for (const s of settings) values = mergeSettings(values, s.values);
  return { plugin, items: decisions.filter((d) => d.enabled).map((d) => d.id), values, decisions };
}

// ── the wire: config.pluginSet ───────────────────────────────────────────

export interface PluginSetItem { id: string; kind: PluginSetKind; name: string; version: number; definition: unknown }
export interface PluginSetEntry { items: string[]; values?: Record<string, unknown> }
/** Plugin id → what it gets. */
export type PluginSetScope = Record<string, PluginSetEntry>;
/**
 * `config.pluginSet` (web → runner): every item some plugin gets somewhere
 * on this machine, once; what each plugin gets in a folder that is in no
 * listed project (`machine`); and the same per project. A project that is
 * not listed gets `machine`.
 */
export interface PluginSet { v: 1; items: PluginSetItem[]; machine: PluginSetScope; projects: Record<string, PluginSetScope> }

const entry = (e: EffectivePlugin): PluginSetEntry => ({ items: e.items, ...(Object.keys(e.values).length ? { values: e.values } : {}) });

/** The set for one machine: `plugins` are the plugin ids web configures, `projectKeys` the user's projects. */
export function buildPluginSet(rows: PluginRows, opts: { machineId: string; plugins: readonly string[]; projectKeys: readonly string[] }): PluginSet {
  const scope = (target: PluginTarget): PluginSetScope => Object.fromEntries(opts.plugins.map((p) => [p, entry(effectivePlugin(rows, p, target))]));
  const machine = scope({ machineId: opts.machineId });
  const same = JSON.stringify(machine);
  const projects: Record<string, PluginSetScope> = {};
  for (const k of [...new Set(opts.projectKeys)].sort().slice(0, MAX_PLUGIN_SET_PROJECTS)) {
    const s = scope({ machineId: opts.machineId, projectKey: k });
    if (JSON.stringify(s) !== same) projects[k] = s;
  }
  const used = new Set<string>();
  for (const s of [machine, ...Object.values(projects)]) for (const e of Object.values(s)) for (const id of e.items) used.add(id);
  const items = rows.items.filter((i) => used.has(i.id)).map((i) => ({ id: i.id, kind: i.kind, name: i.name, version: i.version, definition: i.definition }))
    .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  return { v: 1, items, machine, projects };
}

function parseScope(v: unknown, known: Set<string>): PluginSetScope {
  const out: PluginSetScope = {};
  if (!isObj(v)) return out;
  for (const [plugin, e] of Object.entries(v)) {
    if (!PLUGIN.test(plugin) || !isObj(e) || !Array.isArray(e.items)) continue;
    const items = [...new Set(e.items.filter((i): i is string => typeof i === 'string' && known.has(i)))];
    out[plugin] = { items, ...(isObj(e.values) && Object.keys(e.values).length ? { values: e.values } : {}) };
  }
  return out;
}

/**
 * `config.pluginSet` → the valid part; undefined when absent, not an object
 * or of a version this side does not know. Junk rows are dropped, never the
 * whole set; an entry never names an item that is not in `items`.
 */
export function parsePluginSet(v: unknown): PluginSet | undefined {
  if (!isObj(v) || v.v !== 1) return undefined;
  const items: PluginSetItem[] = [];
  const known = new Set<string>();
  for (const x of Array.isArray(v.items) ? v.items : []) {
    if (items.length >= MAX_PLUGIN_SET_ITEMS) break;
    if (!isObj(x) || typeof x.id !== 'string' || !PLUGIN_ITEM_ID.test(x.id) || known.has(x.id)) continue;
    if (typeof x.name !== 'string' || !PLUGIN_ITEM_NAME.test(x.name) || !PLUGIN_SET_KINDS.includes(x.kind as PluginSetKind)) continue;
    if (!Number.isSafeInteger(x.version) || (x.version as number) < 1 || !isObj(x.definition)) continue;
    known.add(x.id);
    items.push({ id: x.id, kind: x.kind as PluginSetKind, name: x.name, version: x.version as number, definition: x.definition });
  }
  const projects: Record<string, PluginSetScope> = {};
  if (isObj(v.projects)) {
    for (const [k, s] of Object.entries(v.projects).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)).slice(0, MAX_PLUGIN_SET_PROJECTS)) {
      // eslint-disable-next-line no-control-regex
      if (!k || k.length > 512 || /[\x00-\x1f\x7f]/.test(k) || !isObj(s)) continue;
      projects[k] = parseScope(s, known);
    }
  }
  return { v: 1, items, machine: parseScope(v.machine, known), projects };
}

/** What `plugin` gets in `projectKey` (or outside every listed project) from a parsed set. */
export function pluginSetEntry(set: PluginSet, plugin: string, projectKey?: string | null): PluginSetEntry {
  const scope = (projectKey && Object.hasOwn(set.projects, projectKey) ? set.projects[projectKey] : set.machine) ?? {};
  return (Object.hasOwn(scope, plugin) ? scope[plugin] : undefined) ?? { items: [] };
}

// ── dangerous launch flags ───────────────────────────────────────────────

/**
 * The flags that only the plugin's dangerous switch may add (design §4.5).
 * ONE list for the browser form (which points at the switch) and for the
 * server (which refuses them in `values.args`, whoever calls).
 */
export const DANGEROUS_ARGS: Record<string, readonly string[]> = {
  claude: ['--dangerously-skip-permissions', '--allow-dangerously-skip-permissions'],
  codex: ['--dangerously-bypass-approvals-and-sandbox', '--yolo'],
  cursor: ['--force', '-f', '--yolo'],
};

const CODEX_DANGEROUS_CONFIG = /^\s*(approval_policy|sandbox_mode)\s*=\s*["']?\s*(never|danger-full-access)\b/i;

/**
 * The first token of `args` that does what a plugin's dangerous switch does,
 * or null. Each element is split on whitespace (a pasted line holds several
 * flags), long flags match in any case, `--flag=value` counts, and so do the
 * equivalent spellings the CLI honours: Claude's `--permission-mode
 * bypassPermissions`; Codex's `-a never`, `-s danger-full-access` and
 * `-c approval_policy=never` / `-c sandbox_mode=danger-full-access` (also
 * `--config=…`, `-cX=Y`, `-anever`); Cursor's `-f` inside combined short flags.
 */
export function dangerousArg(plugin: string, args: readonly unknown[]): string | null {
  const flags = DANGEROUS_ARGS[plugin];
  if (!flags) return null;
  const tokens = args.flatMap((a) => (typeof a === 'string' ? a.split(/\s+/).filter(Boolean) : []));
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i]!;
    const eq = t.indexOf('=');
    const head = eq < 0 ? t : t.slice(0, eq);
    const name = head.toLowerCase();
    const inline = eq < 0 ? null : t.slice(eq + 1);
    const value = () => (inline ?? tokens[i + 1] ?? '').replace(/^["']|["']$/g, '');
    // An attached short-option value: `-anever`, `-c=x`, `-cx=y`.
    const attached = (letter: string) => (t.startsWith(`-${letter}`) && !t.startsWith('--') && t.length > 2 ? t.slice(2).replace(/^=/, '').replace(/^["']|["']$/g, '') : null);
    if (name.startsWith('--') && flags.includes(name)) return t;
    if (plugin === 'claude' && name === '--permission-mode' && value().toLowerCase() === 'bypasspermissions') return t;
    if (plugin === 'cursor' && !t.startsWith('--') && /^-[A-Za-z]*f[A-Za-z]*$/.test(head)) return t;
    if (plugin === 'codex') {
      if ((name === '--config' || name === '-c') && CODEX_DANGEROUS_CONFIG.test(value())) return t;
      const c = attached('c');
      if (c !== null && CODEX_DANGEROUS_CONFIG.test(c)) return t;
      if ((name === '--ask-for-approval' || name === '-a') && value().toLowerCase() === 'never') return t;
      if (attached('a')?.toLowerCase() === 'never') return t;
      if ((name === '--sandbox' || name === '-s') && value().toLowerCase() === 'danger-full-access') return t;
      if (attached('s')?.toLowerCase() === 'danger-full-access') return t;
    }
  }
  return null;
}

export * from './mcp-pin.js';
