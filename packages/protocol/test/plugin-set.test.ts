/**
 * Plugin items and settings against fixtures/plugin-set.json (the Rust side
 * parses the same frames: runner/crates/protocol/tests/plugin_set.rs): item
 * definitions per kind, the scope resolver (table-driven) and the wire set.
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import {
  buildPluginSet, definitionSecrets, effectivePlugin, mergeSettings, parsePluginSet, pluginSetEntry, validateItemDefinition,
  MAX_ITEM_BODY_BYTES, MAX_PLUGIN_SET_ITEMS,
  type PluginItemKind, type PluginItemRow, type PluginItemStateRow, type PluginRows, type PluginSet, type PluginSettingsRow, type PluginTarget,
} from '../src/plugin-set.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/plugin-set.json', import.meta.url), 'utf8'));
const rows = fx.rows as PluginRows;
const API = 'github.com/acme/api';
const WEB = 'github.com/acme/web';

describe('item definitions', () => {
  const good: Array<[PluginItemKind, unknown]> = [
    ['mcp', { transport: 'stdio', command: ['npx', '-y', 'server'], env: { TOKEN: { $secret: 'GH_TOKEN' }, HOST: 'github.com' }, tools: ['create_issue', 'mcp__x__*'] }],
    ['mcp', { transport: 'stdio', command: ['server'] }],
    ['mcp', { transport: 'http', url: 'https://mcp.example.com/x', headers: { 'X-Team': 'core', Authorization: { $secret: 'T' } }, bearer: { $secret: 'T' } }],
    ['skill', { description: 'Draft notes.', body: '# Notes\n\n- one\n' }],
    ['subagent', { description: 'Reviews code.', instructions: 'Review.\n', model: 'sonnet', tools: ['Read'] }],
    ['command', { body: 'Ship it.\n' }],
    ['command', { description: 'Ship', body: 'Ship it.\n' }],
    ['instructions', { body: 'Prefer small commits.\n' }],
  ];
  it.each(good)('accepts a %s definition and returns exactly it', (kind, def) => {
    expect(validateItemDefinition(kind, def)).toEqual({ ok: true, value: def });
  });

  it('every fixture row is a valid definition', () => {
    for (const i of rows.items) expect(validateItemDefinition(i.kind, i.definition), i.id).toEqual({ ok: true, value: i.definition });
  });

  const bad: Array<[string, PluginItemKind, unknown, string]> = [
    ['not an object', 'mcp', 'npx server', ''],
    ['a list', 'skill', [], ''],
    ['no transport', 'mcp', { command: ['x'] }, 'transport'],
    ['a shell string instead of argv', 'mcp', { transport: 'stdio', command: 'npx server' }, 'command'],
    ['an empty argv', 'mcp', { transport: 'stdio', command: [] }, 'command'],
    ['an empty program', 'mcp', { transport: 'stdio', command: [' '] }, 'command'],
    ['a line break in an argument', 'mcp', { transport: 'stdio', command: ['x', 'a\nb'] }, 'command'],
    ['too many arguments', 'mcp', { transport: 'stdio', command: Array(65).fill('x') }, 'command'],
    ['an argument that is too long', 'mcp', { transport: 'stdio', command: ['x', 'y'.repeat(4097)] }, 'command'],
    ['a bad env name', 'mcp', { transport: 'stdio', command: ['x'], env: { 'A-B': 'c' } }, 'env.A-B'],
    ['a multi-line env value', 'mcp', { transport: 'stdio', command: ['x'], env: { A: 'b\nc' } }, 'env.A'],
    ['a secret reference with a bad name', 'mcp', { transport: 'stdio', command: ['x'], env: { A: { $secret: 'gh token' } } }, 'env.A'],
    ['a secret reference with more in it', 'mcp', { transport: 'stdio', command: ['x'], env: { A: { $secret: 'T', value: 'ghp_x' } } }, 'env.A'],
    ['an env value that is a number', 'mcp', { transport: 'stdio', command: ['x'], env: { A: 1 } }, 'env.A'],
    ['env as a list', 'mcp', { transport: 'stdio', command: ['x'], env: ['A=b'] }, 'env'],
    ['url on a local command', 'mcp', { transport: 'stdio', command: ['x'], url: 'https://x.example' }, 'url'],
    ['command on a remote server', 'mcp', { transport: 'http', url: 'https://x.example', command: ['x'] }, 'command'],
    ['a url that is not http', 'mcp', { transport: 'http', url: 'file:///etc/passwd' }, 'url'],
    ['a url with a password in it', 'mcp', { transport: 'http', url: 'https://user:pw@x.example/' }, 'url'],
    ['a url with a space', 'mcp', { transport: 'http', url: 'https://x.example/a b' }, 'url'],
    ['a bearer token typed in', 'mcp', { transport: 'http', url: 'https://x.example', bearer: 'ghp_abc' }, 'bearer'],
    ['a bad header name', 'mcp', { transport: 'http', url: 'https://x.example', headers: { 'X Team': 'a' } }, 'headers.X Team'],
    ['tools twice', 'mcp', { transport: 'stdio', command: ['x'], tools: ['a', 'a'] }, 'tools'],
    ['tools as text', 'mcp', { transport: 'stdio', command: ['x'], tools: 'a' }, 'tools'],
    ['an unknown key', 'mcp', { transport: 'stdio', command: ['x'], cwd: '/' }, 'cwd'],
    ['a skill without a description', 'skill', { body: 'x' }, 'description'],
    ['a skill without a body', 'skill', { description: 'x' }, 'body'],
    ['a blank body', 'instructions', { body: ' \n' }, 'body'],
    ['a body with an escape character', 'instructions', { body: 'a\u001b[0m' }, 'body'],
    ['a body over 64 KiB', 'instructions', { body: 'x'.repeat(MAX_ITEM_BODY_BYTES + 1) }, 'body'],
    ['a body over 64 KiB in bytes', 'command', { body: 'é'.repeat(MAX_ITEM_BODY_BYTES / 2 + 1) }, 'body'],
    ['a multi-line description', 'skill', { description: 'a\nb', body: 'x' }, 'description'],
    ['a sub-agent with a long model', 'subagent', { description: 'd', instructions: 'i', model: 'm'.repeat(65) }, 'model'],
    ['files on a skill (not in this version)', 'skill', { description: 'd', body: 'b', files: {} }, 'files'],
    ['an unknown kind', 'plugin' as PluginItemKind, { body: 'x' }, 'kind'],
  ];
  it.each(bad)('refuses %s', (_name, kind, def, path) => {
    const r = validateItemDefinition(kind, def);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.issues.map((i) => i.path)).toEqual([path]);
  });

  it('a body of exactly 64 KiB fits', () => {
    expect(validateItemDefinition('instructions', { body: 'x'.repeat(MAX_ITEM_BODY_BYTES) }).ok).toBe(true);
  });

  it('reports every problem, and never echoes a value', () => {
    const r = validateItemDefinition('mcp', { transport: 'stdio', command: 'x', env: { A: 5 }, extra: 'ghp_secretvalue' });
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.issues.map((i) => i.path).sort()).toEqual(['command', 'env.A', 'extra']);
      expect(JSON.stringify(r.issues)).not.toContain('ghp_secretvalue');
    }
  });

  it('lists the secrets a definition names', () => {
    expect(definitionSecrets(good[0]![1])).toEqual(['GH_TOKEN']);
    expect(definitionSecrets(good[2]![1])).toEqual(['T']);
    expect(definitionSecrets({ body: 'no {"$secret": "X"} in text' })).toEqual([]);
    expect(definitionSecrets(null)).toEqual([]);
  });
});

describe('the resolver', () => {
  const item = (id: string, name: string, scope: PluginItemRow['scope'] = 'user', key = '', kind: PluginItemKind = 'mcp'): PluginItemRow =>
    ({ id, kind, name, version: 1, definition: {}, scope, key });
  const st = (itemId: string, enabled: boolean, scope: PluginItemStateRow['scope'] = 'user', key = '', plugin = 'claude'): PluginItemStateRow =>
    ({ itemId, plugin, scope, key, enabled });
  const set = (values: Record<string, unknown>, scope: PluginSettingsRow['scope'] = 'user', key = '', plugin = 'claude'): PluginSettingsRow =>
    ({ plugin, scope, key, values });
  const M1: PluginTarget = { machineId: 'm1' };
  const M1P: PluginTarget = { machineId: 'm1', projectKey: 'p' };
  const M2P: PluginTarget = { machineId: 'm2', projectKey: 'p' };
  const M1Q: PluginTarget = { machineId: 'm1', projectKey: 'q' };

  const table: Array<{ name: string; items: PluginItemRow[]; states?: PluginItemStateRow[]; target: PluginTarget; plugin?: string; want: string[] }> = [
    { name: 'nothing is on until it is enabled', items: [item('a', 'x')], target: M1, want: [] },
    { name: 'enabled for everyone', items: [item('a', 'x')], states: [st('a', true)], target: M1P, want: ['a'] },
    { name: 'enabled for one plugin is not enabled for another', items: [item('a', 'x')], states: [st('a', true)], target: M1, plugin: 'codex', want: [] },
    { name: 'disabled for everyone', items: [item('a', 'x')], states: [st('a', false)], target: M1, want: [] },
    { name: 'user on, machine off: off on that machine', items: [item('a', 'x')], states: [st('a', true), st('a', false, 'machine', 'm1')], target: M1, want: [] },
    { name: 'user on, another machine off: still on here', items: [item('a', 'x')], states: [st('a', true), st('a', false, 'machine', 'm2')], target: M1, want: ['a'] },
    { name: 'user off, machine on: on on that machine', items: [item('a', 'x')], states: [st('a', false), st('a', true, 'machine', 'm1')], target: M1, want: ['a'] },
    { name: 'user off, machine on: off elsewhere', items: [item('a', 'x')], states: [st('a', false), st('a', true, 'machine', 'm1')], target: M2P, want: [] },
    { name: 'user on, project off: off in that project', items: [item('a', 'x')], states: [st('a', true), st('a', false, 'project', 'p')], target: M1P, want: [] },
    { name: 'user on, project off: on in another project', items: [item('a', 'x')], states: [st('a', true), st('a', false, 'project', 'p')], target: M1Q, want: ['a'] },
    { name: 'user on, project off: on outside projects', items: [item('a', 'x')], states: [st('a', true), st('a', false, 'project', 'p')], target: M1, want: ['a'] },
    { name: 'machine off, project on: the project wins', items: [item('a', 'x')], states: [st('a', true), st('a', false, 'machine', 'm1'), st('a', true, 'project', 'p')], target: M1P, want: ['a'] },
    { name: 'machine on, project off: the project wins', items: [item('a', 'x')], states: [st('a', true, 'machine', 'm1'), st('a', false, 'project', 'p')], target: M1P, want: [] },
    { name: 'the order of rows does not matter', items: [item('a', 'x')], states: [st('a', false, 'project', 'p'), st('a', true, 'machine', 'm1'), st('a', true)], target: M1P, want: [] },
    { name: 'a machine item is not visible on another machine', items: [item('a', 'x', 'machine', 'm2')], states: [st('a', true, 'machine', 'm2')], target: M1, want: [] },
    { name: 'a project item is not visible outside it', items: [item('a', 'x', 'project', 'p')], states: [st('a', true, 'project', 'p')], target: M1Q, want: [] },
    { name: 'a project item is visible on every machine', items: [item('a', 'x', 'project', 'p')], states: [st('a', true, 'project', 'p')], target: M2P, want: ['a'] },
    { name: 'a row broader than the item is ignored', items: [item('a', 'x', 'project', 'p')], states: [st('a', true), st('a', true, 'machine', 'm1')], target: M1P, want: [] },
    { name: 'a machine item can be turned off in a project', items: [item('a', 'x', 'machine', 'm1')], states: [st('a', true, 'machine', 'm1'), st('a', false, 'project', 'p')], target: M1P, want: [] },
    { name: 'same name: the project definition wins', items: [item('u', 'x'), item('p', 'x', 'project', 'p')], states: [st('u', true), st('p', true, 'project', 'p')], target: M1P, want: ['p'] },
    { name: 'same name: the machine definition beats the user one', items: [item('u', 'x'), item('m', 'x', 'machine', 'm1')], states: [st('u', true), st('m', true, 'machine', 'm1')], target: M1, want: ['m'] },
    { name: 'same name: project beats machine beats user', items: [item('u', 'x'), item('m', 'x', 'machine', 'm1'), item('p', 'x', 'project', 'p')], states: [st('u', true), st('m', true, 'machine', 'm1'), st('p', true, 'project', 'p')], target: M1P, want: ['p'] },
    { name: 'same name: a disabled narrower definition still shadows', items: [item('u', 'x'), item('p', 'x', 'project', 'p')], states: [st('u', true), st('p', false, 'project', 'p')], target: M1P, want: [] },
    { name: 'same name: a narrower definition never enabled still shadows', items: [item('u', 'x'), item('p', 'x', 'project', 'p')], states: [st('u', true)], target: M1P, want: [] },
    { name: 'same name: no shadow outside the project', items: [item('u', 'x'), item('p', 'x', 'project', 'p')], states: [st('u', true), st('p', false, 'project', 'p')], target: M1Q, want: ['u'] },
    { name: 'same name, another kind: both', items: [item('u', 'x'), item('s', 'x', 'user', '', 'skill')], states: [st('u', true), st('s', true)], target: M1, want: ['u', 's'] },
    { name: 'sorted by kind, then name', items: [item('i', 'b', 'user', '', 'instructions'), item('z', 'a'), item('y', 'b'), item('s', 'a', 'user', '', 'skill')], states: [st('i', true), st('z', true), st('y', true), st('s', true)], target: M1, want: ['z', 'y', 's', 'i'] },
    { name: 'a row for an item that is gone changes nothing', items: [item('a', 'x')], states: [st('gone', true), st('a', true)], target: M1, want: ['a'] },
  ];
  it.each(table)('$name', ({ items, states, target, plugin, want }) => {
    expect(effectivePlugin({ items, states: states ?? [], settings: [] }, plugin ?? 'claude', target).items).toEqual(want);
  });

  it('says why: where it was decided and what shadows it', () => {
    const r = effectivePlugin(rows, 'claude', { machineId: 'm1', projectKey: API });
    const by = Object.fromEntries(r.decisions.map((d) => [d.id, d]));
    expect(Object.keys(by).sort()).toEqual(['i-gh', 'i-gh-api', 'i-notes', 'i-pg', 'i-rule']);
    expect(by['i-gh']).toMatchObject({ enabled: false, shadowedBy: 'i-gh-api', decidedAt: { scope: 'user', key: '' }, definedAt: { scope: 'user', key: '' } });
    expect(by['i-gh-api']).toMatchObject({ enabled: true, shadowedBy: null, decidedAt: { scope: 'project', key: API }, definedAt: { scope: 'project', key: API } });
    expect(by['i-notes']).toMatchObject({ enabled: false, shadowedBy: null, decidedAt: { scope: 'project', key: API } });
    expect(by['i-pg']).toMatchObject({ enabled: true, decidedAt: { scope: 'machine', key: 'm1' } });
    expect(by['i-rule']).toMatchObject({ enabled: false, decidedAt: null });
  });

  const settings: Array<{ name: string; rows: PluginSettingsRow[]; target: PluginTarget; want: Record<string, unknown> }> = [
    { name: 'no settings', rows: [], target: M1, want: {} },
    { name: 'user settings everywhere', rows: [set({ a: 1 })], target: M2P, want: { a: 1 } },
    { name: 'a machine overrides one key', rows: [set({ a: 1, b: 2 }), set({ b: 3 }, 'machine', 'm1')], target: M1, want: { a: 1, b: 3 } },
    { name: 'another machine does not', rows: [set({ a: 1, b: 2 }), set({ b: 3 }, 'machine', 'm2')], target: M1, want: { a: 1, b: 2 } },
    { name: 'a project overrides the machine', rows: [set({ b: 4 }, 'project', 'p'), set({ a: 1, b: 2 }), set({ b: 3 }, 'machine', 'm1')], target: M1P, want: { a: 1, b: 4 } },
    { name: 'sections merge per key', rows: [set({ launch: { model: 'sonnet', args: ['-v'] } }), set({ launch: { model: 'opus' } }, 'machine', 'm1')], target: M1, want: { launch: { model: 'opus', args: ['-v'] } } },
    { name: 'lists replace, they do not append', rows: [set({ launch: { args: ['-v'] } }), set({ launch: { args: [] } }, 'project', 'p')], target: M1P, want: { launch: { args: [] } } },
    { name: 'a narrower false is a value', rows: [set({ extras: true }), set({ extras: false }, 'project', 'p')], target: M1P, want: { extras: false } },
    { name: 'another plugin\'s settings are not this one\'s', rows: [set({ a: 1 }, 'user', '', 'codex')], target: M1, want: {} },
  ];
  it.each(settings)('settings: $name', ({ rows: s, target, want }) => {
    expect(effectivePlugin({ items: [], states: [], settings: s }, 'claude', target).values).toEqual(want);
  });

  it('merging never changes its inputs', () => {
    const base = { launch: { model: 'a' } };
    const over = { launch: { args: ['x'] } };
    expect(mergeSettings(base, over)).toEqual({ launch: { model: 'a', args: ['x'] } });
    expect(base).toEqual({ launch: { model: 'a' } });
    expect(over).toEqual({ launch: { args: ['x'] } });
  });
});

describe('config.pluginSet', () => {
  const configs = fx.config as Record<string, { pluginSet?: unknown }>;
  const parsed = fx.parsed as Record<string, PluginSet | null>;

  it('the fixture rows build exactly the full frame\'s set', () => {
    expect(buildPluginSet(rows, fx.build)).toEqual(configs.full!.pluginSet);
  });

  it('a project that gets what the machine gets is not listed, and reads as the machine', () => {
    const s = buildPluginSet(rows, fx.build);
    expect(Object.keys(s.projects).sort()).toEqual([API, WEB]);
    expect(pluginSetEntry(s, 'claude', 'github.com/acme/untouched')).toEqual(s.machine.claude);
    expect(pluginSetEntry(s, 'claude')).toEqual(s.machine.claude);
    expect(pluginSetEntry(s, 'claude', API).items).toEqual(['i-gh-api', 'i-pg']);
    expect(pluginSetEntry(s, 'codex', WEB).items).toEqual(['i-rule']);
    expect(pluginSetEntry(s, 'unknown', API)).toEqual({ items: [] });
    expect(pluginSetEntry(s, 'claude', '__proto__')).toEqual(s.machine.claude);
  });

  it('carries each item once, and none that no plugin gets here', () => {
    const s = buildPluginSet(rows, fx.build);
    expect(s.items.map((i) => i.id)).toEqual(['i-gh', 'i-gh-api', 'i-notes', 'i-pg', 'i-rule']);
    expect(JSON.stringify(buildPluginSet(rows, { ...fx.build, machineId: 'm3' }))).not.toContain('pg-mcp');
  });

  it('never carries a secret value: only names', () => {
    const text = JSON.stringify(buildPluginSet(rows, fx.build));
    expect(text).toContain('{"$secret":"GH_TOKEN"}');
    for (const i of buildPluginSet(rows, fx.build).items) expect(validateItemDefinition(i.kind, i.definition).ok).toBe(true);
  });

  it.each(Object.keys(fx.parsed as object))('parses %s as the fixture says', (name) => {
    expect(parsePluginSet(configs[name]!.pluginSet) ?? null).toEqual(parsed[name]);
  });

  it('parsing is idempotent', () => {
    for (const name of ['full', 'junk', 'empty']) {
      const once = parsePluginSet(configs[name]!.pluginSet);
      expect(parsePluginSet(once)).toEqual(once);
    }
  });

  it('keeps at most 200 items', () => {
    const items = Array.from({ length: MAX_PLUGIN_SET_ITEMS + 5 }, (_, i) => ({ id: `i${i}`, kind: 'command', name: `c${i}`, version: 1, definition: { body: 'x' } }));
    expect(parsePluginSet({ v: 1, items, machine: {}, projects: {} })!.items).toHaveLength(MAX_PLUGIN_SET_ITEMS);
  });
});
