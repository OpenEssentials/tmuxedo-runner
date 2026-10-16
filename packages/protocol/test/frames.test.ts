import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { AUTH_EVENT_TYPES, parseClaudeAuth, decodeData, encodeData, parseClientFrame, parseRunnerFrame, setReq, DataKind, SUBPROTOCOL, LEGACY_SUBPROTOCOL, SUBPROTOCOLS } from '../src/frames.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/data-frames.json', import.meta.url), 'utf8'));
const unhex = (h: string) => new Uint8Array(Buffer.from(h, 'hex'));

describe('data frames match the golden fixtures', () => {
  it.each(fx.valid.map((c: any) => [c.name, c]))('%s decodes and re-encodes byte-exactly', (_n, c: any) => {
    const f = decodeData(unhex(c.hex))!;
    expect(f).not.toBeNull();
    expect({ kind: f.kind, gap: f.gap, req: f.req, paneId: f.paneId, offset: f.offset.toString(), payloadHex: Buffer.from(f.payload).toString('hex') })
      .toEqual({ kind: c.kind, gap: c.gap, req: c.req, paneId: c.paneId, offset: c.offset, payloadHex: c.payloadHex });
    expect(Buffer.from(encodeData(f)).toString('hex')).toBe(c.hex);
  });
  it.each(fx.invalid.map((c: any) => [c.name, c.hex]))('%s is rejected without throwing', (_n, h) => {
    expect(decodeData(unhex(h as string))).toBeNull();
  });
  it('setReq rewrites only the req field', () => {
    const b = encodeData({ kind: DataKind.OUTPUT, gap: false, req: 0, paneId: 'w1:p1', offset: 5n, payload: new Uint8Array([1, 2]) });
    setReq(b, 42);
    expect(decodeData(b)).toMatchObject({ req: 42, paneId: 'w1:p1', offset: 5n });
  });
  it('refuses oversize pane ids and frames', () => {
    expect(() => encodeData({ kind: 1, gap: false, req: 0, paneId: 'x'.repeat(256), offset: 0n, payload: new Uint8Array() })).toThrow();
    expect(() => encodeData({ kind: 1, gap: false, req: 0, paneId: 'x', offset: 0n, payload: new Uint8Array(1024 * 1024) })).toThrow();
  });
});

describe('client frames', () => {
  it.each([
    ['{"t":"attach","paneId":"w1:p1","from":10}', true],
    ['{"t":"attach","paneId":"w1:p1","from":-1}', false],
    ['{"t":"resize","paneId":"w1:p1","cols":120,"rows":40}', true],
    ['{"t":"resize","paneId":"w1:p1","cols":0,"rows":40}', false],
    ['{"t":"req","id":"1","method":"pane.split","params":{}}', true],
    ['{"t":"req","id":"1","method":"rm -rf","params":{}}', false],
    ['{"t":"req","id":"1","method":"ping","params":{}}', true],
    ['{"t":"req","id":"1","method":"agent.view.set","params":{}}', true],
    ['{"t":"req","id":"1","method":"pane.","params":{}}', false],
    ['{"t":"decision","paneId":"w1:p1","promptId":"pr1","behavior":"allow"}', true],
    ['{"t":"decision","paneId":"w1:p1","promptId":"pr1","behavior":"maybe"}', false],
    ['{"t":"ack","paneId":"w1:p1","offset":4096}', true],
    ['{"t":"nope"}', false], ['[1,2]', false], ['not json', false],
  ])('%s → ok=%s', (text, ok) => {
    expect(parseClientFrame(text as string).ok).toBe(ok);
  });
});

describe('runner frames', () => {
  it('requires protocol v1 in hello', () => {
    const r = { version: '0.1.0', holderVersion: '0.1.0', os: 'linux', arch: 'x86_64', hostname: 'h' };
    expect(parseRunnerFrame(JSON.stringify({ t: 'hello', v: 1, runner: r })).ok).toBe(true);
    expect(parseRunnerFrame(JSON.stringify({ t: 'hello', v: 2, runner: r }))).toEqual({ ok: false, reason: 'unsupported_version' });
  });
  it.each([
    ['{"t":"topology","rev":3,"workspaces":[],"tabs":[],"panes":[]}', true],
    ['{"t":"event","seq":1,"paneId":"w1:p1","type":"blocked","at":"2026-10-03T00:00:00Z","data":{}}', true],
    ['{"t":"event","seq":-1,"paneId":"w1:p1","type":"blocked","at":"x","data":{}}', false],
    ['{"t":"status","paneId":"w1:p1","agent":null}', true],
    // Machine-level events (the Claude credentials check) carry paneId "".
    ['{"t":"event","seq":2,"paneId":"","type":"auth_expired","at":"2026-10-03T00:00:00Z","data":{"state":"expired","reason":"x","source":"credentials"}}', true],
    ['{"t":"event","seq":2,"type":"auth_expired","at":"2026-10-03T00:00:00Z","data":{}}', false],
    ['{"t":"topology","rev":4,"workspaces":[{"id":"w1","name":"a","cwd":"/","position":0,"activeTab":"w1:t1"}],"tabs":[],"panes":[],"focusedWorkspace":"w1"}', true],
  ])('%s → ok=%s', (text, ok) => {
    expect(parseRunnerFrame(text as string).ok).toBe(ok);
  });
  it('runner text frames may be up to 16 MiB (a big res); client text frames stay at 1 MiB', () => {
    const big = JSON.stringify({ t: 'res', id: 'g1', result: { text: 'x'.repeat(2 * 1024 * 1024) } });
    expect(parseRunnerFrame(big).ok).toBe(true);
    expect(parseRunnerFrame(JSON.stringify({ t: 'res', id: 'g1', result: 'x'.repeat(16 * 1024 * 1024) })).ok).toBe(false);
    expect(parseClientFrame(JSON.stringify({ t: 'req', id: 'r', method: 'pane.list', params: { pad: 'x'.repeat(2 * 1024 * 1024) } })).ok).toBe(false);
  });
  it('hello.runner.claudeAuth: kept when valid, dropped (hello still accepted) when junk', () => {
    const r = { version: '0.1.0', holderVersion: '0.1.0', os: 'linux', arch: 'x86_64', hostname: 'h' };
    const auth = { state: 'expired', reason: 'refresh token rejected', since: '2026-10-03T00:00:00Z' };
    const ok1 = parseRunnerFrame(JSON.stringify({ t: 'hello', v: 1, runner: { ...r, claudeAuth: auth } }));
    expect(ok1.ok && ok1.frame.t === 'hello' && ok1.frame.runner.claudeAuth).toEqual(auth);
    const junk = parseRunnerFrame(JSON.stringify({ t: 'hello', v: 1, runner: { ...r, claudeAuth: { state: 'weird' } } }));
    expect(junk.ok && junk.frame.t === 'hello' && 'claudeAuth' in junk.frame.runner).toBe(false);
    expect(parseClaudeAuth({ state: 'ok' })).toEqual({ state: 'ok', reason: '', since: '' });
    expect(parseClaudeAuth(null)).toBeUndefined();
    expect(AUTH_EVENT_TYPES).toEqual(['auth_ok', 'auth_missing', 'auth_expired', 'auth_failed']);
  });
});

describe('subprotocol names', () => {
  it('emits tmuxedo.v1 and still lists the legacy aimux.v1', () => {
    expect(SUBPROTOCOL).toBe('tmuxedo.v1');
    expect(LEGACY_SUBPROTOCOL).toBe('aimux.v1');
    expect(SUBPROTOCOLS).toEqual(['tmuxedo.v1', 'aimux.v1']);
  });
});
