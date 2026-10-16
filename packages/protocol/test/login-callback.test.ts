import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { parseClientFrame } from '../src/frames.js';
import {
  detectLoginCallback, LOGIN_CALLBACK_REFUSALS, loginCallbackHost, matchLoginCallback, parseLoginCallback,
} from '../src/login-callback.js';

// The same file the runner checks (login_relay.rs golden_fixtures, protocol tests/fixtures.rs).
const fx = JSON.parse(readFileSync(new URL('../fixtures/login-callback.json', import.meta.url), 'utf8'));

describe('login callback fixtures', () => {
  it.each(fx.detect.map((c: any) => [c.name, c]))('detect %s', (_n, c: any) => {
    expect(detectLoginCallback(c.url)).toEqual(c.callback);
  });
  it.each(fx.match.map((c: any) => [c.name, c]))('match %s', (_n, c: any) => {
    expect(matchLoginCallback(c.callback, c.pasted)).toBe(c.outcome);
  });
  it.each(fx.clientFrames.map((c: any) => [c.name, c]))('client frame %s', (_n, c: any) => {
    const p = parseClientFrame(c.text);
    expect(p.ok).toBe(c.ok);
    if (p.ok) expect(p.frame).toEqual(JSON.parse(c.text));
  });
  it('lists every refusal code', () => {
    expect([...LOGIN_CALLBACK_REFUSALS].sort()).toEqual([...fx.refusals].sort());
    for (const r of fx.results) if (r.res.error) expect(LOGIN_CALLBACK_REFUSALS).toContain(r.res.error.code);
  });
  it('every detected callback survives parseLoginCallback; junk is dropped', () => {
    for (const c of fx.detect) if (c.callback) expect(parseLoginCallback(c.callback)).toEqual(c.callback);
    expect(parseLoginCallback({ host: 'evil.com', port: 1, path: '/', state: 's' })).toBeNull();
    expect(parseLoginCallback({ host: 'localhost', port: 0, path: '/', state: 's' })).toBeNull();
    expect(parseLoginCallback({ host: 'localhost', port: 80, path: 'x', state: 's' })).toBeNull();
    expect(parseLoginCallback({ host: 'localhost', port: 80, path: '/', state: '' })).toBeNull();
    expect(parseLoginCallback('x')).toBeNull();
  });
  it('the audit host never carries the port, path or query', () => {
    expect(loginCallbackHost('http://localhost:54545/callback?code=SECRET&state=s')).toBe('localhost');
    expect(loginCallbackHost('nope')).toBe('?');
  });
});
