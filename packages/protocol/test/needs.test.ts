/**
 * Needs you on the wire against fixtures/needs.json (the Rust side checks the
 * same file in runner/crates/protocol/tests/needs.rs).
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { parseRunnerFrame } from '../src/frames.js';
import {
  fileRequestOf, findResponse, fromChatItem, fromLegacyPrompt, hostAllowed, linkHost, needsCategory, needsFreshStepUp, notificationResponse,
  openUrlTrust, parseNeedsFrame, parseNeedsItem, parseRespondParams, unverifiedLinkText, type NeedsYou,
} from '../src/needs.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/needs.json', import.meta.url), 'utf8'));
const valid = fx.items.valid as Record<string, NeedsYou>;

describe('needs items', () => {
  it('every valid item parses to itself', () => {
    for (const [name, item] of Object.entries(valid)) expect(parseNeedsItem(item), name).toEqual(item);
  });

  it('every junk item is refused, never thrown', () => {
    for (const j of fx.items.junk as Array<{ why: string; item: unknown }>) expect(parseNeedsItem(j.item), j.why).toBeNull();
    for (const v of [undefined, null, 3, [], 'x']) expect(parseNeedsItem(v)).toBeNull();
  });

  it('a needs frame keeps only valid items of its own pane, once each', () => {
    const r = parseRunnerFrame(JSON.stringify(fx.frame.in));
    expect(r.ok).toBe(true);
    const f = parseNeedsFrame(fx.frame.in);
    expect(f?.items.map((i) => i.id)).toEqual(fx.frame.out_ids);
    expect(parseNeedsFrame({ paneId: 'x', items: Array.from({ length: 21 }, () => valid.permission_hook) })).toBeNull();
    expect(parseNeedsFrame({ paneId: 3, items: [] })).toBeNull();
    expect(parseRunnerFrame(JSON.stringify({ t: 'needs', paneId: 'w1:p1' })).ok).toBe(false);
  });

  it('a file request (tmuxedo files request) is answered by an upload and nothing else', () => {
    const f = valid.file_request;
    expect(fileRequestOf(f)).toEqual({ project: 'github.com_example_site', requestId: 'Rq7c1d2e3f4a' });
    expect(needsCategory(f)).toBe('NEEDS_OPEN');
    // No notification action answers it: an upload needs the app or the browser.
    for (const a of ['approve', 'deny', 'reply', 'open']) expect(notificationResponse(f, a, 'x'), a).toBeNull();
    expect(fileRequestOf(valid.permission_hook)).toBeNull();
    expect(fileRequestOf({ ...f, detail: { upload: { project: '', requestId: 'Rq7c1d2e3f4a' } } })).toBeNull();
    expect(parseRespondParams({ itemId: f.id, paneId: f.paneId, responseId: 'dismiss' }).ok).toBe(true);
  });

  it('long texts are cut, not refused: context and why', () => {
    const it = parseNeedsItem({ ...valid.permission_high, why: 'w'.repeat(500), context: ['c'.repeat(500), 3, 'ok'] });
    expect(it?.why).toHaveLength(200);
    expect(it?.context).toEqual(['c'.repeat(200), 'ok']);
  });
});

describe('needs.respond params', () => {
  it('valid params parse to themselves', () => {
    for (const p of fx.respond.valid) expect(parseRespondParams(p)).toEqual({ ok: true, params: p });
  });
  it('control characters, escapes and bad ids are refused (no keystroke injection)', () => {
    for (const c of fx.respond.invalid as Array<{ why: string; params: unknown }>) expect(parseRespondParams(c.params).ok, c.why).toBe(false);
  });
});

describe('push categories and notification actions', () => {
  it('each item maps to its fixed category; high risk never gets approve', () => {
    for (const c of fx.categories as Array<{ item: string; category: string }>) expect(needsCategory(valid[c.item]!), c.item).toBe(c.category);
  });
  it('a notification action answers only what its category allows', () => {
    for (const c of fx.notification_actions as Array<{ item: string; action: string; text?: string; out: unknown }>) {
      expect(notificationResponse(valid[c.item]!, c.action, c.text), `${c.item}/${c.action}`).toEqual(c.out);
    }
  });
  it('approving a high-risk item needs a fresh step-up; denying does not', () => {
    const hi = valid.permission_high!;
    expect(needsFreshStepUp(hi, findResponse(hi, 'approve')!)).toBe(true);
    expect(needsFreshStepUp(hi, findResponse(hi, 'deny')!)).toBe(false);
    expect(needsFreshStepUp(valid.permission_hook!, findResponse(valid.permission_hook!, 'approve')!)).toBe(false);
  });
});

describe('one list: older runners and chats', () => {
  it('a legacy held prompt becomes a hook permission item with the same id', () => {
    const it = fromLegacyPrompt({ paneId: 'w1:p1', promptId: 'abc', kind: 'claude', tool: 'Bash', input: { command: 'ls' }, title: 'Run ls', since: '2026-10-08T10:00:00.000Z' });
    expect(parseNeedsItem(it)).toEqual(it);
    expect(it).toMatchObject({ id: 'abc', answerVia: 'hook', kind: 'permission', risk: 'normal' });
    expect(needsCategory(it)).toBe('NEEDS_APPROVE_DENY');
  });
  it("a chat's item becomes a session item; high risk opens the app", () => {
    const n = fromChatItem('c-abcdefgh12', { id: 'n1', kind: 'permission', risk: 'high', title: 'git push', options: ['allow_once', 'deny'], why: 'Pushes to a remote' });
    expect(parseNeedsItem(n)).toEqual(n);
    expect(n.answerVia).toBe('session');
    expect(needsCategory(n)).toBe('NEEDS_OPEN');
    const q = fromChatItem('c-abcdefgh12', { id: 'n2', kind: 'question', risk: 'normal', title: 'Pick one', options: ['answers', 'deny'] });
    expect(needsCategory(q)).toBe('NEEDS_OPEN');
  });
});

describe('links', () => {
  it('host allowlists match exact hosts and *.subdomains only', () => {
    for (const c of fx.hosts.cases as Array<{ host: string; ok: boolean }>) expect(hostAllowed(c.host, fx.hosts.allow), c.host).toBe(c.ok);
  });
  it('open_url trust: verified, unverified (with the reason), legacy', () => {
    for (const c of fx.open_url as Array<{ data: unknown; trust: string; host: string; reason?: string }>) {
      expect(openUrlTrust(c.data)).toEqual({ trust: c.trust, host: c.host, ...(c.reason ? { reason: c.reason } : {}) });
    }
    expect(linkHost('javascript:alert(1)')).toBe('');
    expect(unverifiedLinkText('claude-login.example', 'pane 3')).toContain('Unverified link to claude-login.example');
  });
});
