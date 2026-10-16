import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import {
  chunkCount, chunkLength, decodeFileFrame, diskLevel, encodeFileFrame, freeSpaceFloor, insertPathText, isFileFrame,
  parseDisk, parseRange, sanitizeFileName, uploadStamp, validUploadShape, CHUNK_MIN, CHUNK_MAX, MAX_CHUNKS, FILE_NAME_MAX,
} from '../src/files.js';
import { mintFileTicket, presentedFileTicket, verifyFileTicket } from '../src/file-ticket.js';
import { mintTicket, verifyTicket } from '../src/ticket.js';
import { decodeData, parseRunnerFrame } from '../src/frames.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/files.json', import.meta.url), 'utf8'));
const SECRET = 's'.repeat(40);

describe('FILE frames', () => {
  it('golden frames decode and re-encode exactly', () => {
    for (const c of fx.frames) {
      const buf = Buffer.from(c.hex, 'hex');
      const f = decodeFileFrame(buf);
      expect(f, c.name).not.toBeNull();
      expect(f!.id).toBe(c.id);
      expect(f!.req).toBe(c.req);
      expect(f!.offset.toString()).toBe(c.offset);
      expect(Buffer.from(f!.payload).toString('hex')).toBe(c.payloadHex);
      expect(Buffer.from(encodeFileFrame(f!)).toString('hex')).toBe(c.hex);
      expect(isFileFrame(buf)).toBe(true);
      // A pane-frame decoder never mistakes one for pane bytes.
      expect(decodeData(buf)).toBeNull();
    }
  });
  it('junk is refused, never thrown', () => {
    for (const c of fx.invalidFrames) expect(decodeFileFrame(Buffer.from(c.hex, 'hex')), c.name).toBeNull();
    expect(decodeFileFrame(new Uint8Array())).toBeNull();
    expect(decodeFileFrame(new Uint8Array(1024 * 1024 + 1).fill(5))).toBeNull();
  });
  it('encoding refuses bad ids, reqs, offsets and oversize frames', () => {
    const ok = { id: 'abcdefgh', req: 1, offset: 0n, payload: new Uint8Array() };
    expect(() => encodeFileFrame({ ...ok, id: 'short' })).toThrow();
    expect(() => encodeFileFrame({ ...ok, req: -1 })).toThrow();
    expect(() => encodeFileFrame({ ...ok, offset: -1n })).toThrow();
    expect(() => encodeFileFrame({ ...ok, payload: new Uint8Array(1024 * 1024) })).toThrow();
  });
});

describe('chunks, names, ranges, disk', () => {
  it('chunk arithmetic matches the fixture', () => {
    for (const c of fx.chunks) {
      expect(chunkCount(c.size, c.chunkSize)).toBe(c.count);
      expect(chunkLength(c.size, c.chunkSize, 0)).toBe(c.first);
      expect(chunkLength(c.size, c.chunkSize, c.count - 1)).toBe(c.last);
      expect(chunkLength(c.size, c.chunkSize, c.count)).toBe(c.beyond);
    }
    expect(chunkLength(10, CHUNK_MIN, 0.5)).toBe(-1);
  });
  it('upload shapes', () => {
    expect(validUploadShape(0, CHUNK_MIN)).toBe(true);
    expect(validUploadShape(1, CHUNK_MIN - 1)).toBe(false);
    expect(validUploadShape(1, CHUNK_MAX + 1)).toBe(false);
    expect(validUploadShape(-1, CHUNK_MIN)).toBe(false);
    expect(validUploadShape(1.5, CHUNK_MIN)).toBe(false);
    expect(validUploadShape(CHUNK_MIN * (MAX_CHUNKS + 1), CHUNK_MIN)).toBe(false);
    expect(validUploadShape(11 * 1024 ** 3, CHUNK_MAX)).toBe(false);
    expect(validUploadShape('1', CHUNK_MIN)).toBe(false);
  });
  it('file names follow the shared rules', () => {
    for (const c of fx.names) expect(sanitizeFileName(c.in), JSON.stringify(c.in)).toBe(c.out);
    for (const c of fx.names) if (c.out) expect([...c.out].length).toBeLessThanOrEqual(FILE_NAME_MAX);
    expect(sanitizeFileName(42)).toBeNull();
  });
  it('ranges', () => {
    for (const c of fx.ranges) expect(parseRange(c.header, c.size), String(c.header)).toEqual(c.result);
  });
  it('disk levels and the floor', () => {
    for (const c of fx.disk) {
      expect(diskLevel(c.freeBytes, c.totalBytes)).toBe(c.level);
      expect(freeSpaceFloor(c.totalBytes)).toBe(c.floor);
    }
    expect(parseDisk({ freeBytes: 1, totalBytes: 2, filesBytes: 0, level: 'ok' })).toEqual({ freeBytes: 1, totalBytes: 2, filesBytes: 0, level: 'ok' });
    expect(parseDisk({ freeBytes: -1, totalBytes: 2, filesBytes: 0, level: 'ok' })).toBeNull();
    expect(parseDisk(null)).toBeNull();
  });
  it('stamps', () => {
    expect(uploadStamp(new Date(fx.stamp.at))).toBe(fx.stamp.stamp);
  });
  it('insert path text per agent', () => {
    expect(insertPathText('/h/x/inbox/a.png', 'claude')).toBe('@/h/x/inbox/a.png ');
    expect(insertPathText('/h/x/inbox/a.png', 'codex')).toBe('/h/x/inbox/a.png ');
    expect(insertPathText('/h/my files/a.png', null)).toBe("'/h/my files/a.png' ");
    expect(insertPathText("/h/it's/a", 'cursor')).toBe(`@'/h/it'\\''s/a' `);
  });
});

describe('control frames', () => {
  it('runner frames from the fixture parse; junk does not', () => {
    for (const f of fx.control.runnerToGateway) {
      const p = parseRunnerFrame(JSON.stringify(f));
      expect(p.ok, JSON.stringify(f)).toBe(true);
      if (p.ok) expect(p.frame).toEqual(f);
    }
    for (const f of fx.control.invalidRunnerToGateway) expect(parseRunnerFrame(JSON.stringify(f)).ok, JSON.stringify(f)).toBe(false);
  });
});

describe('file tickets', () => {
  const now = new Date('2026-10-08T10:00:00Z');
  const up = { op: 'upload' as const, sub: 'u1', sid: 's1', mid: 'm1', uid: 'up_0123456789', size: 10, cs: CHUNK_MIN };
  const down = { op: 'download' as const, sub: 'u1', sid: 's1', mid: 'm1', project: 'github.com_org_repo', dir: 'outbox' as const, name: 'report.pdf' };
  it('round trip, both kinds', () => {
    for (const c of [up, down]) {
      const t = mintFileTicket({ ...c, now }, SECRET);
      expect(t.startsWith('f1.')).toBe(true);
      const v = verifyFileTicket(t, SECRET, now);
      expect(v.ok).toBe(true);
      if (v.ok) expect(v.claims).toMatchObject(c);
    }
  });
  it('refusals', () => {
    const t = mintFileTicket({ ...up, now }, SECRET);
    expect(verifyFileTicket(t, 'x'.repeat(40), now)).toEqual({ ok: false, reason: 'bad_signature' });
    expect(verifyFileTicket(t, SECRET, new Date(now.getTime() + 16 * 60_000))).toEqual({ ok: false, reason: 'expired' });
    expect(verifyFileTicket(t, SECRET, new Date(now.getTime() - 60_000))).toEqual({ ok: false, reason: 'not_yet_valid' });
    expect(verifyFileTicket('junk', SECRET, now)).toEqual({ ok: false, reason: 'malformed' });
    expect(verifyFileTicket(undefined, SECRET, now)).toEqual({ ok: false, reason: 'malformed' });
    const [, body, mac] = t.split('.');
    expect(verifyFileTicket(`t1.${body}.${mac}`, SECRET, now).ok).toBe(false);
  });
  it('an attach ticket is never a file ticket, nor the other way round', () => {
    const attach = mintTicket({ sub: 'u1', sid: 's1', mid: 'm1', scope: 'control', now }, SECRET);
    expect(verifyFileTicket(attach, SECRET, now).ok).toBe(false);
    const file = mintFileTicket({ ...up, now }, SECRET);
    expect(verifyTicket(file, SECRET, now).ok).toBe(false);
  });
  it('bad claims cannot be minted', () => {
    expect(() => mintFileTicket({ ...up, uid: 'bad id', now }, SECRET)).toThrow();
    expect(() => mintFileTicket({ ...up, cs: 1, now }, SECRET)).toThrow();
    expect(() => mintFileTicket({ ...down, name: '../x', now }, SECRET)).toThrow();
    expect(() => mintFileTicket({ ...down, dir: 'etc' as 'inbox', now }, SECRET)).toThrow();
    expect(() => mintFileTicket({ ...down, project: '..', now }, SECRET)).toThrow();
    expect(() => mintFileTicket({ ...down, project: 'a/b', now }, SECRET)).toThrow();
    expect(() => mintFileTicket({ ...up, now }, 'short')).toThrow();
  });
  it('presented in a header or a link', () => {
    expect(presentedFileTicket('Ticket f1.a.b', undefined)).toBe('f1.a.b');
    expect(presentedFileTicket(undefined, '/gw/files/download?t=f1.a.b')).toBe('f1.a.b');
    expect(presentedFileTicket('Bearer x', '/gw/files/x')).toBeNull();
  });
});
