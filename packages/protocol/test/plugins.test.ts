/**
 * Plugins on the wire against fixtures/plugins.json (the Rust side checks the
 * same file): hello dual-write, config dual-write, ui frames, plugin_action.
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { parseClientFrame, parseRunnerFrame } from '../src/frames.js';
import {
  configPlugins, helloAgentAuth, USAGE_ALERT_LEVELS, parseConfigPlugins, parsePluginInfos, parseUiFrame, pluginActionRequest,
  uiSlot, visibleItems, type UiItem,
} from '../src/plugins.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/plugins.json', import.meta.url), 'utf8'));
type Hello = { t: 'hello'; v: 1; runner: Record<string, unknown> };
const hellos = fx.hello as Record<string, Hello>;

describe('hello: plugins[] beside the legacy Claude fields', () => {
  it('every fixture hello parses, and plugins[] survives the parser unchanged', () => {
    for (const name of ['legacy', 'legacy_no_claude', 'dual', 'plugins_only', 'disabled']) {
      const r = parseRunnerFrame(JSON.stringify(hellos[name]));
      expect(r.ok, name).toBe(true);
      const p = hellos[name]!.runner.plugins;
      if (p) expect(parsePluginInfos(p), name).toEqual(p);
      else expect(parsePluginInfos(p), name).toBeUndefined();
    }
  });

  it('the dual hello keeps the legacy fields byte-for-byte (an old web reads them)', () => {
    const { plugins, ...legacyView } = hellos.dual!.runner;
    expect(plugins).toBeDefined();
    const old = { ...hellos.legacy!.runner };
    for (const k of ['version', 'holderVersion', 'hostname']) delete (old as Record<string, unknown>)[k], delete (legacyView as Record<string, unknown>)[k];
    expect(legacyView).toEqual(old);
  });

  it('junk rows and fields are dropped, never thrown', () => {
    expect(parsePluginInfos(fx.hello.junk_plugins.in)).toEqual(fx.hello.junk_plugins.out);
    for (const v of [undefined, null, 'x', {}, 3]) expect(parsePluginInfos(v)).toBeUndefined();
    expect(parsePluginInfos(Array.from({ length: 40 }, (_, i) => ({ id: `p${i}x`, state: 'enabled' })))).toHaveLength(32);
  });

  it.each(fx.auth_of as Array<{ hello: string; plugin: string; reported: boolean; state: string | null }>)(
    'login of $plugin from the $hello hello: $state', ({ hello, plugin, reported, state }) => {
      const a = helloAgentAuth(hellos[hello]!.runner, plugin);
      expect(a.reported).toBe(reported);
      expect(a.auth?.state ?? null).toBe(state);
    });

  it('a hello with neither field says nothing about Claude', () => {
    expect(helloAgentAuth({}, 'claude')).toEqual({ reported: false, auth: null });
    expect(helloAgentAuth(null, 'claude')).toEqual({ reported: false, auth: null });
  });
});

describe('config: plugins[] beside claude.args', () => {
  it('web sends both: the fixture is what configPlugins builds', () => {
    expect(configPlugins(['--model', 'opus'])).toEqual(fx.config.dual.plugins);
    expect(configPlugins([], { skill: false })).toEqual(fx.config.skill_off.plugins);
    expect(configPlugins([], { keystrokePlugins: ['cursor'] })).toEqual(fx.config.keystroke_on.plugins);
    expect(configPlugins([], { keystrokePlugins: ['codex'] })).toEqual(configPlugins([]));
    expect(configPlugins([], { usage: true })).toEqual(fx.config.usage_on.plugins);
    expect(configPlugins([], { usage: false })).toEqual(fx.config.usage_off.plugins);
  });

  it('usage alert levels: absent is all on, only known levels count, in order (as the Rust side)', () => {
    expect(USAGE_ALERT_LEVELS).toEqual([80, 95, 100]);
    for (const c of fx.usage_alerts as { alerts: unknown; want: number[] | null }[]) {
      const p = parseConfigPlugins([{ id: 'claude', settings: { usage: { enabled: true, alerts: c.alerts } } }])!;
      expect(p[0].settings?.usage?.alerts, JSON.stringify(c)).toEqual(c.want ?? undefined);
    }
    const sent = configPlugins([], { usage: true, usageAlerts: [95, 100] });
    expect(sent.find((p) => p.id === 'claude')?.settings?.usage).toEqual({ enabled: true, alerts: [95, 100] });
    expect(sent.find((p) => p.id === 'codex')?.settings?.usage).toEqual({ enabled: true, alerts: [95, 100] });
    expect(JSON.stringify(configPlugins([], { usage: true }))).not.toContain('alerts');
  });

  it('AI usage: on only when the config says so, and never for a plugin with no source', () => {
    for (const [name, want] of Object.entries(fx.config.effective_usage as Record<string, boolean>)) {
      const list = parseConfigPlugins(fx.config[name].plugins) ?? [];
      for (const id of ['claude', 'codex']) expect(list.find((p) => p.id === id)?.settings?.usage?.enabled ?? false, `${name} ${id}`).toBe(want);
      expect(list.find((p) => p.id === 'cursor')?.settings?.usage, name).toBeUndefined();
    }
    expect(JSON.stringify(configPlugins([]))).not.toContain('usage');
  });

  it('keystroke answers: what a runner reads for cursor', () => {
    for (const [name, want] of Object.entries(fx.config.effective_keystroke as Record<string, boolean | null>)) {
      const c = parseConfigPlugins(fx.config[name].plugins)?.find((p) => p.id === 'cursor');
      expect(c?.settings?.keystroke?.enabled ?? null, name).toBe(want);
    }
  });

  it('every fixture parses as written', () => {
    for (const name of ['dual', 'plugins_only', 'skill_off', 'plugins_win', 'keystroke_on', 'usage_on', 'usage_off']) expect(parseConfigPlugins(fx.config[name].plugins), name).toEqual(fx.config[name].plugins);
    expect(parseConfigPlugins(fx.config.legacy.plugins)).toBeUndefined();
  });

  it('junk is dropped', () => {
    expect(parseConfigPlugins([{ id: 'X' }, { id: 'ok', settings: { launch: { args: [1] }, skill: { enabled: 'yes' } } }, 'x'])).toEqual([{ id: 'ok' }]);
  });
});

describe('ui frames', () => {
  it('valid frames parse to themselves (runner side and gateway side)', () => {
    for (const f of [fx.ui.claude, fx.ui.codex, fx.ui.cursor, ...fx.ui.valid]) {
      expect(parseRunnerFrame(JSON.stringify(f)).ok).toBe(true);
      expect(parseUiFrame(f)).toEqual(f);
    }
  });

  it('a bad item is dropped and the frame kept', () => {
    expect(parseUiFrame(fx.ui.dropped_items.frame)!.items).toEqual(fx.ui.dropped_items.keep);
  });

  it('bad frames are refused', () => {
    for (const f of fx.ui.invalid) expect(parseUiFrame(f), JSON.stringify(f)).toBeNull();
    expect(parseUiFrame({ t: 'ui', pluginId: 'demo', scope: 'machine', items: Array.from({ length: 21 }, (_, i) => ({ key: `k${i}`, kind: 'status_line', text: 'x' })) })).toBeNull();
    expect(parseRunnerFrame(JSON.stringify({ t: 'ui', pluginId: 'demo', scope: 'x', items: [] })).ok).toBe(false);
  });

  it('one set per plugin, scope and pane', () => {
    expect(uiSlot({ pluginId: 'demo', scope: 'machine' })).toBe('demo/machine');
    expect(uiSlot({ pluginId: 'demo', scope: 'pane', paneId: 'w1:p1' })).toBe('demo/pane/w1:p1');
  });
});

describe('visibleItems: what a client draws', () => {
  const items = fx.ui.valid[0].items as UiItem[];
  const base = {
    auth: { demo: { state: 'expired' } },
    machine: { kind: 'byo', online: true, name: 'laptop' },
    agents: { 'w1:p1': { status: 'working', kind: 'demo' }, 'w1:p2': { status: 'idle', kind: 'demo' }, 'w1:p3': { status: 'working', kind: 'claude' }, 'w1:p4': null },
  };

  it('machine items once, agent items per matching pane, text filled', () => {
    const shown = visibleItems([{ pluginId: 'demo', items }], base);
    expect(shown.map((s) => `${s.key}${s.paneId ? `@${s.paneId}` : ''}`)).toEqual(['b1', 't1', 'g1@w1:p1', 'g1@w1:p2', 'q1', 's1@w1:p1']);
    expect(shown.find((s) => s.key === 'b1')).toMatchObject({ text: 'Login needed on laptop' });
    expect(shown.find((s) => s.key === 's1')).toMatchObject({ text: 'demo is working' });
  });

  it('a signed-in agent hides the login banner; dismissing hides it too', () => {
    expect(visibleItems([{ pluginId: 'demo', items }], { ...base, auth: { demo: { state: 'ok' } } }).some((s) => s.key === 'b1')).toBe(false);
    expect(visibleItems([{ pluginId: 'demo', items }], base, new Set(['demo/b1'])).some((s) => s.key === 'b1')).toBe(false);
    expect(visibleItems([{ pluginId: 'demo', items }], base, new Set(['demo/g1/w1:p1'])).filter((s) => s.key === 'g1').map((s) => s.paneId)).toEqual(['w1:p2']);
  });

  it('a plugin that owns another label matches by it', () => {
    const shown = visibleItems([{ pluginId: 'demo', agentLabel: 'claude', items }], base);
    expect(shown.filter((s) => s.paneId).map((s) => s.paneId)).toEqual(['w1:p3', 'w1:p3']);
  });

  it('the Claude quick action shows only while a login is needed', () => {
    const c = fx.ui.claude.items as UiItem[];
    expect(visibleItems([{ pluginId: 'claude', items: c }], { auth: { claude: { state: 'expired' } } })).toHaveLength(1);
    expect(visibleItems([{ pluginId: 'claude', items: c }], { auth: { claude: { state: 'ok' } } })).toHaveLength(0);
    expect(visibleItems([{ pluginId: 'claude', items: c }], {})).toHaveLength(0);
  });
});

describe('plugin_action', () => {
  it('valid frames parse and become the runner call', () => {
    for (const { frame, request } of fx.plugin_action.valid) {
      const r = parseClientFrame(JSON.stringify(frame));
      expect(r).toEqual({ ok: true, frame });
      expect(pluginActionRequest(frame)).toEqual(request);
    }
  });
  it('bad frames are refused', () => {
    for (const f of fx.plugin_action.invalid) expect(parseClientFrame(JSON.stringify(f)).ok, JSON.stringify(f)).toBe(false);
  });
});
