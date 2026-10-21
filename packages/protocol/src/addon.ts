/**
 * Add-ons (docs/design/plugins.md §8.1): a curated, data-only bundle that
 * wires a service into the agents tmuxedo launches: environment variables,
 * hooks, a chained status line, Codex config keys and a few packaged files.
 * An add-on is an item of kind "addon" in `config.pluginSet`; it is never
 * user-uploaded, so it is not one of PLUGIN_ITEM_KINDS.
 *
 * An add-on never runs an installer and never writes the agent's own
 * configuration. The runner generates everything per launch (plugin_gen.rs)
 * and leaves it out from the next launch once the add-on is off.
 *
 * The Rust side is runner/crates/protocol/src/addon.rs; both check
 * fixtures/addons.json.
 */

export const ADDON_KIND = 'addon';
export const ADDON_AGENTS = ['claude', 'codex', 'cursor'] as const;
export type AddonAgent = (typeof ADDON_AGENTS)[number];
/** One add-on, serialised. */
export const MAX_ADDON_BYTES = 256 * 1024;
/**
 * Every add-on of one plugin set together, serialised: web keeps the ones on
 * for a machine within it (in name order) and the runner drops what is past
 * it, so add-ons never crowd the person's own items out of the set.
 */
export const MAX_ADDONS_TOTAL_BYTES = 384 * 1024;
export const MAX_ADDON_FILES = 16;
export const MAX_ADDON_FILE_BYTES = 64 * 1024;
export const MAX_ADDON_ORIGINS = 16;
export const MAX_ADDON_PROBES = 8;
export const MAX_ADDON_ENV = 64;
export const MAX_ADDON_EVENTS = 16;
export const MAX_ADDON_HOOKS = 8;
export const MAX_ADDON_CONFIG = 64;
const MAX_LINE = 4096;

/** What each agent may take from an add-on. */
export const ADDON_AGENT_KEYS: Record<AddonAgent, readonly string[]> = {
  claude: ['env', 'hooks', 'statusline'],
  codex: ['env', 'config', 'hooks'],
  cursor: ['hooks'],
};

export interface AddonSecretRef { $secret: string }
export type AddonEnvValue = string | AddonSecretRef;
export interface AddonFile { body: string; exec?: boolean }
export type AddonProbe = { file: string } | { mentions: string };
export interface AddonAgentPart {
  env?: Record<string, AddonEnvValue>;
  hooks?: Record<string, string[]>;
  statusline?: string;
  config?: Record<string, unknown>;
}
export interface AddonDefinition {
  description?: string;
  /** The service's name, for "Already set up on this machine by <service>'s installer". */
  service?: string;
  /** Origins (`https://host[:port]`) the add-on's hooks send to, shown before it is turned on. */
  net?: string[];
  /** Signs that the service's own installer already set it up: the add-on is skipped. */
  probe?: AddonProbe[];
  files?: Record<string, AddonFile>;
  agents: Partial<Record<AddonAgent, AddonAgentPart>>;
}
export interface AddonIssue { path: string; code: string; message: string }
export type AddonResult = { ok: true; value: AddonDefinition } | { ok: false; issues: AddonIssue[] };

const SECRET_NAME = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;
const ENV_NAME = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
/** Names the shell, the loader or tmuxedo itself owns. */
export function reservedEnvName(name: string): boolean {
  const u = name.toUpperCase();
  return ['PATH', 'HOME', 'SHELL', 'USER', 'IFS', 'ENV', 'BASH_ENV'].includes(u) || /^(TMUXEDO_|AIMUX_|LD_|DYLD_)/.test(u);
}
const ORIGIN = /^https?:\/\/[a-z0-9](?:[a-z0-9.-]{0,252})(?::[0-9]{1,5})?$/;
const SEGMENT = /^[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}$/;
const EVENT = /^[A-Za-z][A-Za-z0-9]{0,39}$/;
const FILE_REF = /\{file:([^}]*)\}/g;
/** Codex keys an add-on may not set: hooks go in `hooks`, and the dangerous switch is the plugin's. */
const CODEX_REFUSED = ['hooks', 'approval_policy', 'sandbox_mode'];

const enc = new TextEncoder();
const bytes = (s: string) => enc.encode(s).length;
const isObj = (v: unknown): v is Record<string, unknown> => !!v && typeof v === 'object' && !Array.isArray(v);
// eslint-disable-next-line no-control-regex
const oneLine = (s: string) => !/[\x00-\x1f\x7f]/.test(s);
// eslint-disable-next-line no-control-regex
const cleanText = (s: string) => !/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/.test(s);
const line = (v: unknown, max = MAX_LINE): v is string => typeof v === 'string' && !!v.trim() && bytes(v) <= max && oneLine(v);

/** A relative path of up to four plain segments: never absolute, never `.` or `..`. */
export function addonFilePath(p: string): boolean {
  const segs = p.split('/');
  return p.length <= 128 && segs.length <= 4 && segs.every((s) => SEGMENT.test(s));
}

/** A probe's path under the home folder: dot-folders allowed, never `.` or `..`. */
export function addonProbePath(p: string): boolean {
  const segs = p.split('/');
  return p.length <= 256 && segs.length <= 6 && segs.every((s) => /^[A-Za-z0-9_.-]{1,64}$/.test(s) && s !== '.' && s !== '..');
}

function depth(v: unknown, d: number): number {
  if (Array.isArray(v)) return Math.max(d, ...v.map((x) => depth(x, d + 1)));
  if (isObj(v)) return Math.max(d, ...Object.values(v).map((x) => depth(x, d + 1)));
  return d;
}

/** Check an add-on; unknown keys are refused, so a typo never silently does nothing. */
export function validateAddonDefinition(v: unknown): AddonResult {
  const issues: AddonIssue[] = [];
  const add = (path: string, code: string, message: string) => issues.push({ path, code, message });
  if (!isObj(v)) return { ok: false, issues: [{ path: '', code: 'invalid', message: 'An add-on is an object.' }] };
  const unknown = (o: Record<string, unknown>, allowed: readonly string[], prefix: string) => {
    for (const k of Object.keys(o)) if (!allowed.includes(k)) add(prefix + k, 'unknown', `"${k.slice(0, 40)}" is not part of an add-on.`);
  };
  unknown(v, ['description', 'service', 'net', 'probe', 'files', 'agents'], '');
  if (v.description !== undefined && !line(v.description, 1024)) add('description', 'invalid', 'description is one line of text, up to 1024 bytes.');
  if (v.service !== undefined && !line(v.service, 64)) add('service', 'invalid', 'service is one line of text, up to 64 bytes.');

  if (v.net !== undefined) {
    if (!Array.isArray(v.net) || v.net.length > MAX_ADDON_ORIGINS) add('net', 'invalid', `net is a list of up to ${MAX_ADDON_ORIGINS} origins.`);
    else v.net.forEach((o, i) => { if (typeof o !== 'string' || !ORIGIN.test(o)) add(`net.${i}`, 'origin', 'An origin is http(s)://host[:port], lower case, without a path.'); });
  }

  if (v.probe !== undefined) {
    if (!Array.isArray(v.probe) || v.probe.length > MAX_ADDON_PROBES) add('probe', 'invalid', `probe is a list of up to ${MAX_ADDON_PROBES} checks.`);
    else v.probe.forEach((p, i) => {
      const ok = isObj(p) && Object.keys(p).length === 1 && (
        (typeof p.file === 'string' && p.file.startsWith('~/') && addonProbePath(p.file.slice(2)))
        || line(p.mentions, 256));
      if (!ok) add(`probe.${i}`, 'invalid', 'A probe is {"file": "~/path"} or {"mentions": "text"}.');
    });
  }

  const files = new Set<string>();
  if (v.files !== undefined) {
    if (!isObj(v.files) || Object.keys(v.files).length > MAX_ADDON_FILES) add('files', 'invalid', `files is up to ${MAX_ADDON_FILES} named files.`);
    else for (const [p, f] of Object.entries(v.files)) {
      const at = `files.${p.slice(0, 128)}`;
      if (!addonFilePath(p)) { add(at, 'path', 'A file path is relative, in plain segments, without "." or "..".'); continue; }
      if (!isObj(f) || Object.keys(f).some((k) => k !== 'body' && k !== 'exec') || typeof f.body !== 'string' || !cleanText(f.body) || bytes(f.body) > MAX_ADDON_FILE_BYTES || (f.exec !== undefined && typeof f.exec !== 'boolean')) {
        add(at, 'invalid', 'A file is {"body": text up to 64 KiB, "exec": true|false}.');
        continue;
      }
      files.add(p);
    }
  }
  const refs = (at: string, s: string) => {
    for (const m of s.matchAll(FILE_REF)) if (!files.has(m[1]!)) { add(at, 'file', `{file:${m[1]!.slice(0, 64)}} is not one of the add-on's files.`); return; }
  };

  if (!isObj(v.agents) || !Object.keys(v.agents).length) add('agents', 'invalid', 'agents names what each agent gets (claude, codex, cursor).');
  else for (const [agent, part] of Object.entries(v.agents)) {
    const at = `agents.${agent.slice(0, 40)}`;
    const keys = ADDON_AGENT_KEYS[agent as AddonAgent];
    if (!keys) { add(at, 'unknown', 'tmuxedo has no such agent.'); continue; }
    if (!isObj(part) || !Object.keys(part).length) { add(at, 'invalid', 'Name what this agent gets.'); continue; }
    unknown(part, keys, `${at}.`);
    if (part.env !== undefined) {
      if (!isObj(part.env) || Object.keys(part.env).length > MAX_ADDON_ENV) add(`${at}.env`, 'invalid', `env is up to ${MAX_ADDON_ENV} variables.`);
      else for (const [k, x] of Object.entries(part.env)) {
        const p = `${at}.env.${k.slice(0, 128)}`;
        if (!ENV_NAME.test(k) || reservedEnvName(k)) add(p, 'name', 'This variable name is not allowed.');
        else if (typeof x === 'string') { if (bytes(x) > MAX_LINE || !oneLine(x)) add(p, 'invalid', 'A value is one line of text, up to 4 KiB.'); else refs(p, x); }
        else if (!(isObj(x) && Object.keys(x).length === 1 && typeof x.$secret === 'string' && SECRET_NAME.test(x.$secret))) add(p, 'secret', 'Name a secret, like {"$secret": "SERVICE_TOKEN"}.');
      }
    }
    if (part.hooks !== undefined) {
      if (!isObj(part.hooks) || Object.keys(part.hooks).length > MAX_ADDON_EVENTS) add(`${at}.hooks`, 'invalid', `hooks is up to ${MAX_ADDON_EVENTS} events.`);
      else for (const [e, cmds] of Object.entries(part.hooks)) {
        const p = `${at}.hooks.${e.slice(0, 40)}`;
        if (!EVENT.test(e)) add(p, 'name', 'An event is a name like SessionStart.');
        else if (!Array.isArray(cmds) || !cmds.length || cmds.length > MAX_ADDON_HOOKS || !cmds.every((c) => line(c))) add(p, 'invalid', `An event runs 1 to ${MAX_ADDON_HOOKS} commands, each one line.`);
        else for (const c of cmds as string[]) refs(p, c);
      }
    }
    if (part.statusline !== undefined) {
      if (!line(part.statusline)) add(`${at}.statusline`, 'invalid', 'statusline is one command line.');
      else refs(`${at}.statusline`, part.statusline);
    }
    if (part.config !== undefined) {
      if (!isObj(part.config) || Object.keys(part.config).length > MAX_ADDON_CONFIG) add(`${at}.config`, 'invalid', `config is up to ${MAX_ADDON_CONFIG} keys.`);
      else for (const [k, x] of Object.entries(part.config)) {
        const p = `${at}.config.${k.slice(0, 128)}`;
        if (!line(k, 128)) add(p, 'name', 'A key is one line, up to 128 bytes.');
        else if (CODEX_REFUSED.includes(k.split('.')[0]!.trim())) add(p, 'refused', 'An add-on may not set this key.');
        else if (depth(x, 0) > 4 || bytes(JSON.stringify(x)) > MAX_LINE) add(p, 'invalid', 'A value is up to 4 KiB and four levels deep.');
      }
    }
  }
  if (!issues.length && bytes(JSON.stringify(v)) > MAX_ADDON_BYTES) add('', 'too_large', 'This add-on is larger than 256 KiB.');
  return issues.length ? { ok: false, issues } : { ok: true, value: v as unknown as AddonDefinition };
}
