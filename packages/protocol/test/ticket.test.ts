import { describe, expect, it } from 'vitest';
import { createHmac } from 'node:crypto';
import { mintTicket, verifyTicket, ticketOwner, TICKET_AUDIENCE, TICKET_TTL_SECONDS } from '../src/ticket.js';

const SECRET = 'x'.repeat(48);
const OTHER = 'y'.repeat(48);
const now = new Date('2026-10-02T12:00:00Z');
const base = { sub: 'u1', sid: 's1', scope: 'control' as const, now };

describe('attach tickets', () => {
  it('round-trips its claims', () => {
    const r = verifyTicket(mintTicket({ ...base, mid: 'm1' }, SECRET), SECRET, now);
    expect(r).toEqual({ ok: true, claims: expect.objectContaining({ sub: 'u1', sid: 's1', mid: 'm1', scope: 'control' }) });
  });

  it('omits mid for list-only tickets', () => {
    const r = verifyTicket(mintTicket({ ...base, scope: 'observe' }, SECRET), SECRET, now);
    expect(r.ok).toBe(true);
    expect(r.ok && 'mid' in r.claims).toBe(false);
  });

  it('refuses a ticket signed with another secret', () => {
    expect(verifyTicket(mintTicket(base, OTHER), SECRET, now)).toEqual({ ok: false, reason: 'bad_signature' });
  });

  it('refuses tampered claims', () => {
    const [v, , mac] = mintTicket(base, SECRET).split('.');
    const forged = Buffer.from(JSON.stringify({ sub: 'admin', sid: 's1', scope: 'control', aud: 'tmuxedo-gw', iat: 0, exp: 9e9 })).toString('base64url');
    expect(verifyTicket(`${v}.${forged}.${mac}`, SECRET, now)).toEqual({ ok: false, reason: 'bad_signature' });
  });

  it('mints the tmuxedo-gw audience', () => {
    expect(TICKET_AUDIENCE).toBe('tmuxedo-gw');
    const r = verifyTicket(mintTicket(base, SECRET), SECRET, now);
    expect(r.ok && r.claims.aud).toBe('tmuxedo-gw');
  });

  // A ticket signed by a pre-rename web pod carries aud aimux-gw; a rolling
  // deploy must not lock those clients out.
  const signed = (aud: string) => {
    const body = Buffer.from(JSON.stringify({ sub: 'u1', sid: 's1', scope: 'control', aud, iat: Math.floor(now.getTime() / 1000), exp: Math.floor(now.getTime() / 1000) + 60 })).toString('base64url');
    return `t1.${body}.${createHmac('sha256', SECRET).update(`t1.${body}`).digest('base64url')}`;
  };

  it('still verifies a legacy aimux-gw ticket', () => {
    expect(verifyTicket(signed('aimux-gw'), SECRET, now)).toEqual({ ok: true, claims: expect.objectContaining({ sub: 'u1', aud: 'aimux-gw' }) });
  });

  it('refuses any other audience', () => {
    expect(verifyTicket(signed('other-gw'), SECRET, now)).toEqual({ ok: false, reason: 'wrong_audience' });
  });

  it('expires after its ttl plus skew, not before', () => {
    const t = mintTicket(base, SECRET);
    expect(verifyTicket(t, SECRET, new Date(now.getTime() + (TICKET_TTL_SECONDS + 4) * 1000)).ok).toBe(true);
    expect(verifyTicket(t, SECRET, new Date(now.getTime() + (TICKET_TTL_SECONDS + 5) * 1000))).toEqual({ ok: false, reason: 'expired' });
  });

  it('refuses a ticket from the future beyond skew', () => {
    const t = mintTicket({ ...base, now: new Date(now.getTime() + 60_000) }, SECRET);
    expect(verifyTicket(t, SECRET, now)).toEqual({ ok: false, reason: 'not_yet_valid' });
  });

  it.each([undefined, null, '', 'junk', 't1.a', 't2.a.b', 't1..b', 't1.a.'])('treats %p as malformed without throwing', (raw) => {
    expect(verifyTicket(raw as string, SECRET, now)).toMatchObject({ ok: false });
  });

  it('refuses to mint with a short secret', () => {
    expect(() => mintTicket(base, 'short')).toThrow(/at least 32 bytes/);
  });

  describe('share claim (a guest of one pane)', () => {
    const t = Math.floor(now.getTime() / 1000);
    const share = { id: 'sh1', pane: 'p1', owner: 'owner1', until: t + 3600 };
    const forge = (claims: Record<string, unknown>) => {
      const body = Buffer.from(JSON.stringify({ aud: TICKET_AUDIENCE, iat: t, exp: t + 60, sid: 's1', scope: 'observe', ...claims })).toString('base64url');
      return `t1.${body}.${createHmac('sha256', SECRET).update(`t1.${body}`).digest('base64url')}`;
    };

    it('round-trips, and the owner it acts for is the share owner', () => {
      const r = verifyTicket(mintTicket({ ...base, sub: 'guest1', scope: 'observe', mid: 'm1', share }, SECRET), SECRET, now);
      expect(r.ok && r.claims.share).toEqual(share);
      expect(r.ok && ticketOwner(r.claims)).toBe('owner1');
      expect(ticketOwner({ sub: 'u1' })).toBe('u1');
    });

    it('refuses to mint without a machine, with browse or demo, or with a bad claim', () => {
      expect(() => mintTicket({ ...base, share }, SECRET)).toThrow(/share ticket/);
      expect(() => mintTicket({ ...base, mid: 'm1', share, browse: true }, SECRET)).toThrow(/share ticket/);
      expect(() => mintTicket({ ...base, mid: 'm1', share, demo: ['m1'] }, SECRET)).toThrow(/share ticket/);
      expect(() => mintTicket({ ...base, mid: 'm1', share: { ...share, pane: '' } }, SECRET)).toThrow(/share ticket/);
    });

    it('is expired once the share has ended, however fresh the ticket', () => {
      const ended = mintTicket({ ...base, scope: 'observe', mid: 'm1', share: { ...share, until: t - 1 } }, SECRET);
      expect(verifyTicket(ended, SECRET, now)).toEqual({ ok: false, reason: 'expired' });
    });

    it('refuses a signed share claim that is malformed or combined with browse or demo', () => {
      expect(verifyTicket(forge({ sub: 'g', mid: 'm1', share: { ...share, until: 'soon' } }), SECRET, now)).toEqual({ ok: false, reason: 'malformed' });
      expect(verifyTicket(forge({ sub: 'g', share }), SECRET, now)).toEqual({ ok: false, reason: 'malformed' });
      expect(verifyTicket(forge({ sub: 'g', mid: 'm1', share, browse: true }), SECRET, now)).toEqual({ ok: false, reason: 'malformed' });
      expect(verifyTicket(forge({ sub: 'g', mid: 'm1', share, demo: ['m1'] }), SECRET, now)).toEqual({ ok: false, reason: 'malformed' });
      expect(verifyTicket(forge({ sub: 'g', mid: 'm1', share }), SECRET, now).ok).toBe(true);
    });
  });
});
