/**
 * Roles: reusable agent definitions (docs/design/agent-team-builder.md §2.1).
 * A role can start a pane of its own or be offered to another agent as a
 * helper. This file is the role schema: what a role document may hold, the
 * one normal form of it, and the text its content hash is taken over.
 *
 *   parseRole(input)      strict: every problem is reported with the path of
 *                         the field, nothing is silently dropped or repaired
 *   canonicalRole(role)   the canonical text: JSON, keys sorted, no spaces,
 *                         defaults written out (`cli`, `access`), empty
 *                         optional parts left out. The role's hash is the
 *                         SHA-256 of this text (hex), taken by the caller.
 *
 * Pure (no node:crypto), so every client can import it. The Rust twin is
 * runner/crates/protocol/src/roles.rs; both are checked against
 * fixtures/roles.json, which also pins each hash.
 *
 * Everything in a role is text a person (or a repository) wrote: clients
 * render it as text only.
 */

export const ROLE_SCHEMA = 1;
/** A role id: unique inside its scope. */
export const ROLE_ID = /^[a-z0-9-]{2,32}$/;
/** `cli` of a role that runs on whichever CLI hosts it. */
export const ROLE_ANY_CLI = 'any';
/** Access presets, least first. */
export const ROLE_ACCESS = ['read-only', 'edit', 'edit+shell', 'full'] as const;
export type RoleAccess = (typeof ROLE_ACCESS)[number];
/** The avatar palette (fixed, so every client can draw it with checked contrast). */
export const ROLE_COLORS = ['slate', 'red', 'orange', 'amber', 'green', 'teal', 'blue', 'purple'] as const;
export type RoleColor = (typeof ROLE_COLORS)[number];

export const ROLE_NAME_MAX = 60;
export const ROLE_PURPOSE_MAX = 200;
/** Instructions, in UTF-8 bytes. */
export const ROLE_INSTRUCTIONS_MAX_BYTES = 16 * 1024;
export const ROLE_GLYPH_MAX = 2;
export const ROLE_MODEL_MAX = 80;
export const ROLE_MODELS_MAX = 16;
export const ROLE_TOOL_MAX = 200;
export const ROLE_TOOLS_MAX = 64;
/** `mcp`, `skills` and `secrets`: entries each. */
export const ROLE_LIST_MAX = 32;
export const ROLE_DELEGATES_MAX = 16;
export const ROLE_MAX_SUBAGENTS = 16;
export const ROLE_MAX_DEPTH = 4;

const CLI_ID = /^[a-z0-9-]{2,32}$/;
const ITEM_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;
const SECRET = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;

export type RoleAvatar = { glyph: string; color: RoleColor };
export type RoleLimits = { max_subagents?: number; max_depth?: number };

/** A role in normal form: every field present, defaults filled. */
export type AgentRole = {
  schema: 1;
  id: string;
  name: string;
  avatar?: RoleAvatar;
  /** One line; becomes the helper's description, which the parent agent reads. */
  purpose: string;
  instructions: string;
  /** A plugin id, or `any`. */
  cli: string;
  /** Model per plugin id. */
  model: Record<string, string>;
  access: RoleAccess;
  tools: { allow: string[]; deny: string[] };
  mcp: string[];
  skills: string[];
  /** Secret names only, never values. */
  secrets: string[];
  /** Ids of the roles it may call as helpers. */
  delegates: string[];
  limits: RoleLimits;
};

export type RoleProblem = { path: string; message: string };
export type RoleParse = { ok: true; role: AgentRole } | { ok: false; problems: RoleProblem[] };

const KEYS = ['schema', 'id', 'name', 'avatar', 'purpose', 'instructions', 'cli', 'model', 'access', 'tools', 'mcp', 'skills', 'secrets', 'delegates', 'limits'];

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}
function chars(s: string): number {
  let n = 0;
  for (const _ of s) n++;
  return n;
}
/** A C0 or C1 control character other than the ones in `allowed`. */
function hasControl(s: string, allowed = ''): boolean {
  for (const ch of s) {
    const c = ch.codePointAt(0)!;
    if ((c < 0x20 || (c >= 0x7f && c <= 0x9f)) && !allowed.includes(ch)) return true;
  }
  return false;
}
const edgeSpace = (s: string) => s.startsWith(' ') || s.endsWith(' ');

/** Check a role document. Unknown fields, wrong types and broken limits are all problems. */
export function parseRole(input: unknown): RoleParse {
  const p: RoleProblem[] = [];
  const bad = (path: string, message: string) => { p.push({ path, message }); };
  if (!isObj(input)) return { ok: false, problems: [{ path: '', message: 'A role is a table of fields.' }] };
  const unknown = (o: Record<string, unknown>, known: string[], at: string) => {
    for (const k of Object.keys(o).sort()) if (!known.includes(k)) bad(at + k, 'Unknown field.');
  };
  unknown(input, KEYS, '');

  if (input.schema !== undefined && input.schema !== ROLE_SCHEMA) bad('schema', `Only schema ${ROLE_SCHEMA} is understood.`);

  const id = typeof input.id === 'string' ? input.id : '';
  if (!ROLE_ID.test(id)) bad('id', 'An id is 2 to 32 of a-z, 0-9 and "-".');

  const name = typeof input.name === 'string' ? input.name : '';
  if (name === '' || chars(name) > ROLE_NAME_MAX || hasControl(name) || edgeSpace(name)) bad('name', `A name is 1 to ${ROLE_NAME_MAX} characters on one line.`);

  let avatar: RoleAvatar | undefined;
  if (input.avatar !== undefined) {
    const a = input.avatar;
    if (!isObj(a)) bad('avatar', 'An avatar is a glyph and a colour.');
    else {
      unknown(a, ['glyph', 'color'], 'avatar.');
      const glyph = typeof a.glyph === 'string' ? a.glyph : '';
      const color = typeof a.color === 'string' ? a.color : '';
      const glyphOk = glyph !== '' && chars(glyph) <= ROLE_GLYPH_MAX && !hasControl(glyph) && !glyph.includes(' ');
      const colorOk = (ROLE_COLORS as readonly string[]).includes(color);
      if (!glyphOk) bad('avatar.glyph', `A glyph is 1 or ${ROLE_GLYPH_MAX} characters.`);
      if (!colorOk) bad('avatar.color', `A colour is one of: ${ROLE_COLORS.join(', ')}.`);
      if (glyphOk && colorOk) avatar = { glyph, color: color as RoleColor };
    }
  }

  let purpose = '';
  if (input.purpose !== undefined) {
    if (typeof input.purpose !== 'string' || chars(input.purpose) > ROLE_PURPOSE_MAX || hasControl(input.purpose)) bad('purpose', `A purpose is one line of at most ${ROLE_PURPOSE_MAX} characters.`);
    else purpose = input.purpose;
  }

  let instructions = '';
  if (input.instructions !== undefined) {
    if (typeof input.instructions !== 'string' || new TextEncoder().encode(input.instructions).length > ROLE_INSTRUCTIONS_MAX_BYTES || hasControl(input.instructions, '\n\t')) bad('instructions', 'Instructions are plain text of at most 16 KiB.');
    else instructions = input.instructions;
  }

  let cli = ROLE_ANY_CLI;
  if (input.cli !== undefined) {
    if (typeof input.cli !== 'string' || !CLI_ID.test(input.cli)) bad('cli', 'cli is "any" or the id of an agent plugin.');
    else cli = input.cli;
  }

  const model: Record<string, string> = {};
  if (input.model !== undefined) {
    const m = input.model;
    if (!isObj(m) || Object.keys(m).length > ROLE_MODELS_MAX) bad('model', `model names a model per agent plugin (at most ${ROLE_MODELS_MAX}).`);
    else {
      for (const k of Object.keys(m).sort()) {
        const v = m[k];
        if (!CLI_ID.test(k) || k === ROLE_ANY_CLI) bad(`model.${k}`, 'A model is set per agent plugin id.');
        else if (typeof v !== 'string' || v === '' || chars(v) > ROLE_MODEL_MAX || hasControl(v) || edgeSpace(v)) bad(`model.${k}`, `A model name is 1 to ${ROLE_MODEL_MAX} characters.`);
        else model[k] = v;
      }
    }
  }

  let access: RoleAccess = 'read-only';
  if (input.access !== undefined) {
    if (typeof input.access !== 'string' || !(ROLE_ACCESS as readonly string[]).includes(input.access)) bad('access', `access is one of: ${ROLE_ACCESS.join(', ')}.`);
    else access = input.access as RoleAccess;
  }

  const list = (v: unknown, path: string, max: number, okItem: (s: string) => boolean, what: string): string[] => {
    if (v === undefined) return [];
    if (!Array.isArray(v) || v.length > max) { bad(path, `${what} (at most ${max}).`); return []; }
    const out: string[] = [];
    v.forEach((item, i) => {
      if (typeof item !== 'string' || !okItem(item)) bad(`${path}.${i}`, `${what}.`);
      else if (out.includes(item)) bad(`${path}.${i}`, 'Listed twice.');
      else out.push(item);
    });
    return out;
  };

  const tools = { allow: [] as string[], deny: [] as string[] };
  if (input.tools !== undefined) {
    const t = input.tools;
    if (!isObj(t)) bad('tools', 'tools is an allow list and a deny list.');
    else {
      unknown(t, ['allow', 'deny'], 'tools.');
      const okTool = (s: string) => s !== '' && chars(s) <= ROLE_TOOL_MAX && !hasControl(s) && !edgeSpace(s);
      tools.allow = list(t.allow, 'tools.allow', ROLE_TOOLS_MAX, okTool, 'A list of tool rules');
      tools.deny = list(t.deny, 'tools.deny', ROLE_TOOLS_MAX, okTool, 'A list of tool rules');
    }
  }

  const mcp = list(input.mcp, 'mcp', ROLE_LIST_MAX, (s) => ITEM_NAME.test(s), 'A list of MCP server names');
  const skills = list(input.skills, 'skills', ROLE_LIST_MAX, (s) => ITEM_NAME.test(s), 'A list of skill names');
  const secrets = list(input.secrets, 'secrets', ROLE_LIST_MAX, (s) => SECRET.test(s), 'A list of secret names');
  const delegates = list(input.delegates, 'delegates', ROLE_DELEGATES_MAX, (s) => ROLE_ID.test(s) && s !== id, 'A list of other role ids');

  const limits: RoleLimits = {};
  if (input.limits !== undefined) {
    const l = input.limits;
    if (!isObj(l)) bad('limits', 'limits is max_subagents and max_depth.');
    else {
      unknown(l, ['max_subagents', 'max_depth'], 'limits.');
      const int = (v: unknown, max: number) => typeof v === 'number' && Number.isInteger(v) && v >= 0 && v <= max;
      if (l.max_subagents !== undefined) {
        if (!int(l.max_subagents, ROLE_MAX_SUBAGENTS)) bad('limits.max_subagents', `A whole number from 0 to ${ROLE_MAX_SUBAGENTS}.`);
        else limits.max_subagents = l.max_subagents as number;
      }
      if (l.max_depth !== undefined) {
        if (!int(l.max_depth, ROLE_MAX_DEPTH)) bad('limits.max_depth', `A whole number from 0 to ${ROLE_MAX_DEPTH}.`);
        else limits.max_depth = l.max_depth as number;
      }
    }
  }

  if (p.length) return { ok: false, problems: p };
  return { ok: true, role: { schema: 1, id, name, ...(avatar ? { avatar } : {}), purpose, instructions, cli, model, access, tools, mcp, skills, secrets, delegates, limits } };
}

function canon(v: unknown): string {
  if (Array.isArray(v)) return `[${v.map(canon).join(',')}]`;
  if (isObj(v)) return `{${Object.keys(v).sort().map((k) => `${JSON.stringify(k)}:${canon(v[k])}`).join(',')}}`;
  return JSON.stringify(v);
}

/**
 * The canonical text of a role (see the file header). Two documents that mean
 * the same role give the same text, whatever order their fields came in.
 */
export function canonicalRole(role: AgentRole): string {
  const o: Record<string, unknown> = { schema: ROLE_SCHEMA, id: role.id, name: role.name, cli: role.cli, access: role.access };
  if (role.avatar) o.avatar = { glyph: role.avatar.glyph, color: role.avatar.color };
  if (role.purpose) o.purpose = role.purpose;
  if (role.instructions) o.instructions = role.instructions;
  if (Object.keys(role.model).length) o.model = role.model;
  const tools: Record<string, string[]> = {};
  if (role.tools.allow.length) tools.allow = role.tools.allow;
  if (role.tools.deny.length) tools.deny = role.tools.deny;
  if (Object.keys(tools).length) o.tools = tools;
  for (const k of ['mcp', 'skills', 'secrets', 'delegates'] as const) if (role[k].length) o[k] = role[k];
  const limits: RoleLimits = {};
  if (role.limits.max_subagents !== undefined) limits.max_subagents = role.limits.max_subagents;
  if (role.limits.max_depth !== undefined) limits.max_depth = role.limits.max_depth;
  if (Object.keys(limits).length) o.limits = limits;
  return canon(o);
}

/** The role a canonical text holds (what a store keeps as its parsed form). */
export function roleFromCanonical(text: string): AgentRole | null {
  let v: unknown;
  try { v = JSON.parse(text); } catch { return null; }
  const r = parseRole(v);
  return r.ok ? r.role : null;
}

// ── config.roles: the roles effective on one machine (runner role_set.rs) ──

/** Roles in one `config.roles` (web's per-account limit). */
export const ROLE_SET_MAX_ROLES = 200;
/** Listed projects in one `config.roles`. */
export const ROLE_SET_MAX_PROJECTS = 200;
/** `config.roles` as JSON text: a runner refuses a larger one whole. */
export const ROLE_SET_MAX_BYTES = 512 * 1024;

/** One role as `config.roles` carries it: sent once, named by `ref` in the scopes. */
export interface RoleSetEntry {
  /** The library's id: a row id, or `builtin:<roleId>`. */
  ref: string;
  version: number;
  /** SHA-256 (hex) of canonicalRole(role). */
  hash: string;
  /** A built-in starter role: usable with `agent.start {role}`, never offered as a helper on its own. */
  builtin?: true;
  /** The role as its canonical text (canonicalRole) parsed: the same bytes both sides hash. Read it with parseRole. */
  role: Record<string, unknown>;
}

/**
 * `config.roles`: what each role id means on one machine, resolved by the
 * nearest-scope rule. `machine` holds the refs for a folder in no listed
 * project; a listed project holds its own (one ref per role id).
 */
export interface RoleSet {
  v: 1;
  roles: RoleSetEntry[];
  machine: string[];
  projects: Record<string, string[]>;
}
