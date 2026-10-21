/**
 * Plugins on the wire (docs/protocol.md "Plugins"; docs/design/plugins.md §6,
 * §10). Everything here is ADDITIVE to protocol v1:
 *
 *   hello.runner.plugins[]  each plugin the runner holds, its state and its
 *                           agent's login health. Sent beside the legacy
 *                           `claudeVersion` / `claudeAuth` (dual-write): web
 *                           reads `plugins[]` first and falls back to them.
 *   config.plugins[]        what web wants per plugin; `config.claude.args`
 *                           stays and means plugins[claude].settings.launch.args.
 *   ui                      runner → gateway → clients: the surfaces a plugin
 *                           declares (banner, toast, badge, quick_action,
 *                           status_line), each with an optional `when`.
 *   plugin_action           client → gateway: run one action the plugin's
 *                           manifest declares (the runner looks it up; a
 *                           client cannot invent argv). Answered with `res`.
 *
 * A runner whose hello carries `plugins` understands `plugin_action` and sends
 * `ui`; older runners never do. Parsers never throw: junk is dropped.
 */
import { evalWhen, parseWhen, renderText } from './when.js';

export type PluginState = 'enabled' | 'disabled' | 'incompatible' | 'failed';
export const PLUGIN_STATES: readonly PluginState[] = ['enabled', 'disabled', 'incompatible', 'failed'];
/** Agent login health, generalised from ClaudeAuth (plugins.md §7.1). */
export type AgentAuthState = 'ok' | 'expiring' | 'missing' | 'expired' | 'failed' | 'unknown';
export const AGENT_AUTH_STATES: readonly AgentAuthState[] = ['ok', 'expiring', 'missing', 'expired', 'failed', 'unknown'];
export interface AgentAuth { state: AgentAuthState; reason: string; since: string; expiresAt?: string }
export interface PluginInfo {
  id: string;
  version: string;
  contract: string;
  state: PluginState;
  /** The agent label this plugin owns (`claude`), as in Agent.kind. */
  agent?: string;
  agentVersion?: string;
  /** Absent: the plugin does not track a login, or tracking is off. */
  auth?: AgentAuth;
  /** Why the state is not `enabled` (e.g. "needs contract 2"). */
  reason?: string;
}

/** The plugin whose `hello.claudeVersion` / `claudeAuth` the legacy fields describe. */
export const LEGACY_PLUGIN = 'claude';
export const PLUGIN_ID = /^[a-z0-9-]{2,32}$/;
export const MAX_PLUGINS = 32;

const s = (v: unknown, max: number): string => (typeof v === 'string' ? v.slice(0, max) : '');

export function parseAgentAuth(v: unknown): AgentAuth | undefined {
  if (!v || typeof v !== 'object' || Array.isArray(v)) return undefined;
  const o = v as Record<string, unknown>;
  if (!AGENT_AUTH_STATES.includes(o.state as AgentAuthState)) return undefined;
  return {
    state: o.state as AgentAuthState, reason: s(o.reason, 256), since: s(o.since, 64),
    ...(typeof o.expiresAt === 'string' && o.expiresAt ? { expiresAt: o.expiresAt.slice(0, 64) } : {}),
  };
}

/** `hello.runner.plugins` → valid rows (junk rows and fields dropped); undefined when absent or not a list. */
export function parsePluginInfos(v: unknown): PluginInfo[] | undefined {
  if (!Array.isArray(v)) return undefined;
  const out: PluginInfo[] = [];
  for (const x of v.slice(0, MAX_PLUGINS)) {
    if (!x || typeof x !== 'object' || Array.isArray(x)) continue;
    const o = x as Record<string, unknown>;
    if (typeof o.id !== 'string' || !PLUGIN_ID.test(o.id) || !PLUGIN_STATES.includes(o.state as PluginState)) continue;
    if (out.some((p) => p.id === o.id)) continue;
    const auth = parseAgentAuth(o.auth);
    out.push({
      id: o.id, version: s(o.version, 64), contract: s(o.contract, 64), state: o.state as PluginState,
      ...(typeof o.agent === 'string' && o.agent ? { agent: o.agent.slice(0, 64) } : {}),
      ...(typeof o.agentVersion === 'string' && o.agentVersion ? { agentVersion: o.agentVersion.slice(0, 64) } : {}),
      ...(auth ? { auth } : {}),
      ...(typeof o.reason === 'string' && o.reason ? { reason: o.reason.slice(0, 256) } : {}),
    });
  }
  return out;
}

/** What a hello says about one plugin's login: `plugins[]` first, then the legacy fields (for `claude`). */
export function helloAgentAuth(
  hello: { plugins?: unknown; claudeAuth?: unknown; claudeVersion?: unknown } | null | undefined,
  plugin: string,
): { reported: boolean; auth: AgentAuth | null } {
  const plugins = parsePluginInfos(hello?.plugins);
  if (plugins) {
    const p = plugins.find((x) => x.id === plugin);
    return { reported: true, auth: p?.auth ?? null };
  }
  if (plugin !== LEGACY_PLUGIN || !hello) return { reported: false, auth: null };
  const c = parseAgentAuth(hello.claudeAuth);
  if (c && c.state !== 'expiring' && c.state !== 'unknown') return { reported: true, auth: c };
  // A runner that reports claudeVersion = null has no Claude Code: "no login" is a fact.
  return { reported: 'claudeVersion' in hello && hello.claudeVersion === null, auth: null };
}

// ── config ───────────────────────────────────────────────────────────────

/** One plugin's desired state in `config.plugins` (web → runner). */
export interface ConfigPlugin {
  id: string;
  /** Not sent today (built-ins only): the runner keeps its own version. */
  version?: string;
  digest?: string;
  settings?: {
    launch?: { args: string[] };
    /** The generated tmuxedo skill (plugins.md "The tmuxedo skill"): absent = on. */
    skill?: { enabled: boolean };
    /** Answer this plugin's on-screen questions by typing into the pane (D-12, Phase 13): absent = the plugin's default. */
    keystroke?: { enabled: boolean };
    /**
     * AI usage (docs/protocol.md "AI usage"): read this agent's plan numbers. For Claude that means the
     * runner sets Claude's status-line command to its own (which runs the person's). Absent = off.
     */
    usage?: {
      enabled: boolean;
      /** The needs-you alerts on (USAGE_ALERT_LEVELS; 100 = the limit, which offers to resume at the reset). Absent = all. */
      alerts?: number[];
    };
  };
}

/** Usage alert levels in percent (docs/protocol.md "AI usage"); 100 is the limit. */
export const USAGE_ALERT_LEVELS = [80, 95, 100] as const;

/** `settings.usage.alerts` → the known levels it holds, in order, once each; undefined (all on) when not a list. */
export function parseAlertLevels(v: unknown): number[] | undefined {
  if (!Array.isArray(v)) return undefined;
  return USAGE_ALERT_LEVELS.filter((l) => v.includes(l));
}

/** Built-in plugins web configures besides Claude (PLAN Phase 16). */
export const CONFIG_PLUGINS = ['claude', 'codex', 'cursor'] as const;
/** Plugins whose prompts can be answered by keystroke (`permissions.mode = "keystroke"`). */
export const KEYSTROKE_PLUGINS = ['cursor'] as const;
/** Plugins that can report plan usage (a `[usage]` source in their manifest). Cursor has none. */
export const USAGE_PLUGINS = ['claude', 'codex'] as const;

/**
 * `config.plugins` for a machine from the launch args web already sends as
 * `config.claude.args` (dual-write: both go out; a new runner reads
 * `plugins`, an old one `claude`).
 */
export function configPlugins(claudeArgs: readonly string[], opts: { skill?: boolean; keystroke?: boolean; keystrokePlugins?: readonly string[]; /** AI usage for the plugins that can report it (USAGE_PLUGINS); not given = not sent, which a runner reads as off. */ usage?: boolean; /** The usage alert levels on; not given = all. */ usageAlerts?: readonly number[] } = {}): ConfigPlugin[] {
  const skill = { enabled: opts.skill ?? true };
  const alerts = opts.usageAlerts !== undefined ? { alerts: parseAlertLevels([...opts.usageAlerts]) } : {};
  const usage = opts.usage !== undefined ? { usage: { enabled: opts.usage, ...alerts } } : {};
  return [
    {
      id: LEGACY_PLUGIN,
      settings: { launch: { args: [...claudeArgs] }, skill, ...(opts.keystroke !== undefined ? { keystroke: { enabled: opts.keystroke } } : {}), ...usage },
    },
    { id: 'codex', settings: { skill: { ...skill }, ...usage } },
    // Cursor's on-screen dialogs are answered by keys only when the person chose it (off by default).
    { id: 'cursor', settings: { skill: { ...skill }, keystroke: { enabled: (opts.keystrokePlugins ?? []).includes('cursor') } } },
  ];
}

/** `config.plugins` → valid rows; undefined when absent. Unknown fields are ignored. */
export function parseConfigPlugins(v: unknown): ConfigPlugin[] | undefined {
  if (!Array.isArray(v)) return undefined;
  const out: ConfigPlugin[] = [];
  for (const x of v.slice(0, MAX_PLUGINS)) {
    if (!x || typeof x !== 'object' || Array.isArray(x)) continue;
    const o = x as Record<string, unknown>;
    if (typeof o.id !== 'string' || !PLUGIN_ID.test(o.id)) continue;
    const st = (o.settings && typeof o.settings === 'object' ? o.settings : {}) as Record<string, unknown>;
    const launch = st.launch as Record<string, unknown> | undefined;
    const skill = st.skill as Record<string, unknown> | undefined;
    const keys = st.keystroke as Record<string, unknown> | undefined;
    const settings: NonNullable<ConfigPlugin['settings']> = {};
    if (launch && Array.isArray(launch.args) && launch.args.every((a) => typeof a === 'string')) settings.launch = { args: launch.args as string[] };
    if (skill && typeof skill.enabled === 'boolean') settings.skill = { enabled: skill.enabled };
    if (keys && typeof keys.enabled === 'boolean') settings.keystroke = { enabled: keys.enabled };
    const usage = st.usage as Record<string, unknown> | undefined;
    if (usage && typeof usage.enabled === 'boolean') {
      const alerts = parseAlertLevels(usage.alerts);
      settings.usage = { enabled: usage.enabled, ...(alerts ? { alerts } : {}) };
    }
    out.push({ id: o.id, ...(typeof o.version === 'string' ? { version: o.version } : {}), ...(typeof o.digest === 'string' ? { digest: o.digest } : {}), ...(Object.keys(settings).length ? { settings } : {}) });
  }
  return out;
}

// ── ui ───────────────────────────────────────────────────────────────────

export type UiTone = 'info' | 'success' | 'warning' | 'danger';
export const UI_TONES: readonly UiTone[] = ['info', 'success', 'warning', 'danger'];
export type UiKind = 'banner' | 'toast' | 'badge' | 'quick_action' | 'status_line';
export const UI_KINDS: readonly UiKind[] = ['banner', 'toast', 'badge', 'quick_action', 'status_line'];
/**
 * An action a surface offers: a manifest action id (run on the runner through
 * `plugin_action`), or a client-side one: `dismiss` (hide until the condition
 * changes) or `settings.open` (go to the plugin's settings).
 */
export interface UiAction { label: string; action: string }
export const CLIENT_ACTIONS = ['dismiss', 'settings.open'] as const;
/** `machine`: once for the machine. `agent`: on each pane whose agent is this plugin's. */
export type UiTarget = 'machine' | 'agent';
interface UiBase { key: string; when?: string; target?: UiTarget }
export type UiItem =
  | (UiBase & { kind: 'banner'; tone: UiTone; text: string; actions?: UiAction[]; dismissible?: boolean })
  | (UiBase & { kind: 'toast'; tone: UiTone; text: string; action?: UiAction })
  | (UiBase & { kind: 'badge'; tone: UiTone; text: string })
  | (UiBase & { kind: 'quick_action'; label: string; action: string })
  | (UiBase & { kind: 'status_line'; text: string });
export interface UiFrame { t: 'ui'; pluginId: string; scope: 'machine' | 'pane'; paneId?: string; items: UiItem[] }

/** Limits (plugins.md §9.3 `ui.surfaces`). */
export const UI_MAX_ITEMS = 20;
export const UI_MAX_TEXT = 280;
export const UI_MAX_LABEL = 40;
export const UI_MAX_BANNER_ACTIONS = 3;
export const UI_KEY = /^[a-z0-9][a-z0-9-]{0,39}$/;
/** A manifest action id, or a client action. */
export const UI_ACTION_ID = /^[a-z0-9][a-z0-9._-]{0,63}$/;

function action(v: unknown): UiAction | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  if (typeof o.label !== 'string' || !o.label || o.label.length > UI_MAX_LABEL || typeof o.action !== 'string' || !UI_ACTION_ID.test(o.action)) return null;
  return { label: o.label, action: o.action };
}

/** One item, or null when it breaks a rule (a bad item is dropped, its frame kept). */
export function parseUiItem(v: unknown): UiItem | null {
  if (!v || typeof v !== 'object' || Array.isArray(v)) return null;
  const o = v as Record<string, unknown>;
  if (typeof o.key !== 'string' || !UI_KEY.test(o.key) || !UI_KINDS.includes(o.kind as UiKind)) return null;
  if (o.when !== undefined && (typeof o.when !== 'string' || (o.when !== '' && !parseWhen(o.when).ok))) return null;
  if (o.target !== undefined && o.target !== 'machine' && o.target !== 'agent') return null;
  const base: UiBase = { key: o.key, ...(typeof o.when === 'string' && o.when ? { when: o.when } : {}), ...(o.target === 'agent' ? { target: 'agent' as const } : {}) };
  const text = typeof o.text === 'string' && o.text.length > 0 && o.text.length <= UI_MAX_TEXT ? o.text : null;
  const tone = UI_TONES.includes(o.tone as UiTone) ? (o.tone as UiTone) : 'info';
  switch (o.kind) {
    case 'banner': {
      if (!text || (o.actions !== undefined && !Array.isArray(o.actions))) return null;
      const acts = ((o.actions as unknown[] | undefined) ?? []).map(action);
      if (acts.length > UI_MAX_BANNER_ACTIONS || acts.some((a) => !a)) return null;
      return { ...base, kind: 'banner', tone, text, ...(acts.length ? { actions: acts as UiAction[] } : {}), ...(o.dismissible === true ? { dismissible: true } : {}) };
    }
    case 'toast': {
      if (!text) return null;
      const a = o.action === undefined ? undefined : action(o.action);
      if (a === null) return null;
      return { ...base, kind: 'toast', tone, text, ...(a ? { action: a } : {}) };
    }
    case 'badge':
      if (!text || text.length > UI_MAX_LABEL) return null;
      return { ...base, kind: 'badge', tone, text };
    case 'quick_action':
      if (typeof o.label !== 'string' || !o.label || o.label.length > UI_MAX_LABEL || typeof o.action !== 'string' || !UI_ACTION_ID.test(o.action)) return null;
      return { ...base, kind: 'quick_action', label: o.label, action: o.action };
    case 'status_line':
      if (!text) return null;
      return { ...base, kind: 'status_line', text };
    default:
      return null;
  }
}

/** A runner `ui` frame (items checked one by one); null when the frame itself is junk. */
export function parseUiFrame(o: Record<string, unknown>): UiFrame | null {
  if (typeof o.pluginId !== 'string' || !PLUGIN_ID.test(o.pluginId)) return null;
  if (o.scope !== 'machine' && o.scope !== 'pane') return null;
  if (o.scope === 'pane' && (typeof o.paneId !== 'string' || !o.paneId || o.paneId.length > 255)) return null;
  if (!Array.isArray(o.items) || o.items.length > UI_MAX_ITEMS) return null;
  const items: UiItem[] = [];
  for (const x of o.items) {
    const it = parseUiItem(x);
    if (it && !items.some((i) => i.key === it.key)) items.push(it);
  }
  return { t: 'ui', pluginId: o.pluginId, scope: o.scope, ...(o.scope === 'pane' ? { paneId: o.paneId as string } : {}), items };
}

/** The key a `ui` frame replaces: one set per (plugin, scope, pane). */
export function uiSlot(f: Pick<UiFrame, 'pluginId' | 'scope' | 'paneId'>): string {
  return f.scope === 'pane' ? `${f.pluginId}/pane/${f.paneId}` : `${f.pluginId}/machine`;
}

/** The context a surface's `when` and text see (plugins.md §6.1). */
export interface UiContext {
  auth?: { state: string; reason?: string } | null;
  agent?: { status: string; kind: string } | null;
  machine?: { kind?: string; online?: boolean; name?: string };
  settings?: Record<string, unknown>;
  plugin?: { id: string };
}

/** A surface to draw: the item with its text filled in, for one place. */
export type ShownItem = UiItem & { pluginId: string; paneId?: string };

/**
 * Which items show, given each plugin's declared items and the client's own
 * context. `machine` items are evaluated once; `agent` items once per pane
 * whose agent kind is the plugin's label (`agents` maps paneId → agent).
 * `dismissed` holds `pluginId/key[/paneId]` the person dismissed.
 */
export function visibleItems(
  sets: ReadonlyArray<{ pluginId: string; agentLabel?: string; items: readonly UiItem[]; paneId?: string }>,
  base: { auth?: Record<string, UiContext['auth']>; machine?: UiContext['machine']; agents?: Record<string, { status: string; kind: string } | null>; settings?: Record<string, Record<string, unknown>> },
  dismissed: ReadonlySet<string> = new Set(),
): ShownItem[] {
  const out: ShownItem[] = [];
  for (const set of sets) {
    const label = set.agentLabel ?? set.pluginId;
    const common = { auth: base.auth?.[set.pluginId] ?? null, machine: base.machine ?? {}, settings: base.settings?.[set.pluginId] ?? {}, plugin: { id: set.pluginId } };
    for (const it of set.items) {
      const fill = (ctx: UiContext): UiItem => {
        switch (it.kind) {
          case 'banner': case 'toast': case 'badge': case 'status_line': return { ...it, text: renderText(it.text, ctx) };
          case 'quick_action': return { ...it, label: renderText(it.label, ctx) };
        }
      };
      if (set.paneId || it.target !== 'agent') {
        const agent = set.paneId ? base.agents?.[set.paneId] ?? null : null;
        const ctx: UiContext = { ...common, agent };
        if (dismissed.has(`${set.pluginId}/${it.key}${set.paneId ? `/${set.paneId}` : ''}`) || !evalWhen(it.when, ctx)) continue;
        out.push({ ...fill(ctx), pluginId: set.pluginId, ...(set.paneId ? { paneId: set.paneId } : {}) });
        continue;
      }
      for (const [paneId, agent] of Object.entries(base.agents ?? {})) {
        if (!agent || agent.kind !== label) continue;
        const ctx: UiContext = { ...common, agent };
        if (dismissed.has(`${set.pluginId}/${it.key}/${paneId}`) || !evalWhen(it.when, ctx)) continue;
        out.push({ ...fill(ctx), pluginId: set.pluginId, paneId });
      }
    }
  }
  return out;
}

// ── plugin_action ────────────────────────────────────────────────────────

export interface PluginActionFrame { t: 'plugin_action'; id: string; pluginId: string; actionId: string; paneId?: string }

/** The runner API call a `plugin_action` becomes (runner-api.schema.json `plugin.action.invoke`). */
export function pluginActionRequest(f: Omit<PluginActionFrame, 't' | 'id'>): { method: 'plugin.action.invoke'; params: Record<string, unknown> } {
  return {
    method: 'plugin.action.invoke',
    params: { plugin_id: f.pluginId, action_id: f.actionId, context: f.paneId ? { focused_pane_id: f.paneId } : {} },
  };
}
