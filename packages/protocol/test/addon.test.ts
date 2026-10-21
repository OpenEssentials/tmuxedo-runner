/**
 * Add-on definitions against fixtures/addons.json (the Rust side checks the
 * same cases: runner/crates/protocol/tests/addon.rs) and the wire: an add-on
 * travels in config.pluginSet but is never a kind the item form offers.
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { addonFilePath, reservedEnvName, validateAddonDefinition, MAX_ADDON_BYTES } from '../src/addon.js';
import { parsePluginSet, PLUGIN_ITEM_KINDS, PLUGIN_SET_KINDS } from '../src/plugin-set.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/addons.json', import.meta.url), 'utf8'));
type Case = { name: string; definition: unknown; path?: string };

describe('add-on definitions', () => {
  it('accepts the full example and returns exactly it', () => {
    expect(validateAddonDefinition(fx.full)).toEqual({ ok: true, value: fx.full });
  });
  it.each((fx.good as Case[]).map((c) => [c.name, c] as const))('accepts: %s', (_n, c) => {
    expect(validateAddonDefinition(c.definition)).toEqual({ ok: true, value: c.definition });
  });
  it.each((fx.bad as Case[]).map((c) => [c.name, c] as const))('refuses: %s', (_n, c) => {
    const r = validateAddonDefinition(c.definition);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.issues.map((i) => i.path)).toContain(c.path);
  });
  it('refuses an add-on larger than the cap', () => {
    const files = Object.fromEntries(Array.from({ length: 5 }, (_, i) => [`f${i}`, { body: 'x'.repeat(60 * 1024) }]));
    const r = validateAddonDefinition({ files, agents: { claude: { env: { A: '1' } } } });
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.issues[0]).toMatchObject({ path: '', code: 'too_large' });
    expect(MAX_ADDON_BYTES).toBe(262144);
  });
  it('file paths and reserved names', () => {
    expect(['bin/hook', 'a.b/c-d_e', 'x'].every(addonFilePath)).toBe(true);
    expect(['', '/x', '../x', 'a/../b', 'a/./b', '.hidden', 'a//b', 'a/b/c/d/e'].some(addonFilePath)).toBe(false);
    expect(['PATH', 'path', 'TMUXEDO_X', 'AIMUX_X', 'LD_PRELOAD', 'DYLD_X', 'BASH_ENV'].every(reservedEnvName)).toBe(true);
    expect(reservedEnvName('SERVICE_TOKEN')).toBe(false);
  });
});

describe('the wire', () => {
  it('carries an add-on but the item form never offers one', () => {
    expect(PLUGIN_SET_KINDS).toContain('addon');
    expect(PLUGIN_ITEM_KINDS as readonly string[]).not.toContain('addon');
    const set = parsePluginSet({ v: 1, items: [{ id: 'a1', kind: 'addon', name: 'telemetry', version: 1, definition: fx.full }], machine: { claude: { items: ['a1'] } }, projects: {} });
    expect(set?.items.map((i) => i.kind)).toEqual(['addon']);
    expect(set?.machine.claude?.items).toEqual(['a1']);
  });
});
