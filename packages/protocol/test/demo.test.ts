import { describe, expect, it } from 'vitest';
import { createHmac } from 'node:crypto';
import { DEMO_CLAIM_MAX, mintTicket, verifyTicket } from '../src/ticket.js';
import { DEMO_PANES, demoHello, demoTopology } from '../src/demo.js';
import { parseRunnerFrame } from '../src/frames.js';

const S = 'x'.repeat(48);
const now = new Date('2026-10-04T12:00:00Z');

describe('ticket demo claim', () => {
  it('round-trips and is absent by default', () => {
    const r = verifyTicket(mintTicket({ sub: 'u', sid: 's', mid: 'm', scope: 'observe', demo: ['m'], now }, S), S, now);
    expect(r.ok && r.claims.demo).toEqual(['m']);
    const plain = verifyTicket(mintTicket({ sub: 'u', sid: 's', scope: 'observe', now }, S), S, now);
    expect(plain.ok && 'demo' in plain.claims).toBe(false);
  });

  it('caps the list when minting', () => {
    const r = verifyTicket(mintTicket({ sub: 'u', sid: 's', scope: 'observe', demo: ['a', 'b', 'c', 'd', 'e', 'f'], now }, S), S, now);
    expect(r.ok && r.claims.demo).toHaveLength(DEMO_CLAIM_MAX);
  });

  it('refuses a signed but malformed demo claim', () => {
    const iat = Math.floor(now.getTime() / 1000);
    for (const demo of ['m', [1], [''], Array(DEMO_CLAIM_MAX + 1).fill('m')]) {
      const body = Buffer.from(JSON.stringify({ sub: 'u', sid: 's', scope: 'observe', demo, aud: 'tmuxedo-gw', iat, exp: iat + 60 })).toString('base64url');
      const mac = createHmac('sha256', S).update(`t1.${body}`).digest('base64url');
      expect(verifyTicket(`t1.${body}.${mac}`, S, now)).toEqual({ ok: false, reason: 'malformed' });
    }
  });
});

describe('demo topology', () => {
  it('is a valid runner topology with two tabs and a split', () => {
    const t = demoTopology(3, { kind: 'claude', status: 'idle', since: now.toISOString(), source: 'reported' });
    const p = parseRunnerFrame(JSON.stringify(t));
    expect(p.ok).toBe(true);
    expect(t.tabs).toHaveLength(2);
    expect(t.tabs[0]!.layout).toMatchObject({ dir: 'h', a: { pane: DEMO_PANES.shell }, b: { pane: DEMO_PANES.agent } });
    expect(t.panes.map((x) => x.id)).toEqual([DEMO_PANES.shell, DEMO_PANES.agent, DEMO_PANES.notes]);
  });

  it('hello parses and lists glance', () => {
    const p = parseRunnerFrame(JSON.stringify({ t: 'hello', v: 1, runner: demoHello(now.toISOString()) }));
    expect(p.ok && p.frame.t === 'hello' && p.frame.runner.features).toEqual(['glance']);
  });
});
