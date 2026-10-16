/**
 * Agent chat against fixtures/chat.json (the Rust runner reads the same file:
 * runner/crates/protocol/tests/fixtures.rs): CHAT frames byte for byte, one
 * event of every type, junk refused without throwing, and the chat control
 * frames on both sockets.
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import {
  CHAT_EVENT_TYPES, CHAT_MAX_SEND_BYTES, CHAT_MAX_TOOL_ATTACHMENTS, CHAT_MAX_TOOL_NAME, CHAT_MAX_TOOL_OUTPUT, CHAT_MAX_TOOL_SUBJECT,
  decodeChatFrame, encodeChatFrame, parseChatEvent, TOOL_STATUSES, TOOL_VARIANTS,
} from '../src/chat.js';
import { DataKind, decodeData, encodeData, parseClientFrame, parseRunnerFrame } from '../src/frames.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/chat.json', import.meta.url), 'utf8'));
const unhex = (h: string) => new Uint8Array(Buffer.from(h, 'hex'));

describe('chat events', () => {
  it('the fixture has one event of every type', () => {
    const types = new Set(fx.events.map((e: { type: string }) => e.type));
    expect([...types].sort()).toEqual([...CHAT_EVENT_TYPES].sort());
  });
  it.each(fx.events.map((e: { type: string; seq: number }) => [`${e.type} #${e.seq}`, e]))('%s parses', (_n, e) => {
    expect(parseChatEvent(JSON.stringify(e))).toEqual(e);
    expect(parseChatEvent(new TextEncoder().encode(JSON.stringify(e)))).toEqual(e);
  });
  it.each(fx.invalidEvents.map((c: { name: string; text: string }) => [c.name, c.text]))('%s is refused', (_n, t) => {
    expect(parseChatEvent(t as string)).toBeNull();
  });
  it.each(fx.invalidOptional.map((c: { name: string; text: string }) => [c.name, c.text]))('optional field: %s is refused', (_n, t) => {
    expect(parseChatEvent(t as string)).toBeNull();
  });
  it.each(fx.tolerated.map((c: { name: string; text: string }) => [c.name, c.text]))('%s is read past', (_n, t) => {
    expect(parseChatEvent(t as string)).toEqual(JSON.parse(t as string));
  });
  it('every optional tool and thought field appears in the fixture, and each event type also appears without them', () => {
    const optional: Record<string, string[]> = {
      'thought.done': ['durationMs', 'redacted'],
      'tool.started': ['name', 'server', 'subject', 'variant'],
      'tool.updated': ['output'],
      'tool.done': ['exitCode', 'durationMs', 'outputBytes', 'outputKept', 'attachments', 'resultCount'],
    };
    for (const [type, fields] of Object.entries(optional)) {
      const of = fx.events.filter((e: { type: string }) => e.type === type) as Record<string, unknown>[];
      for (const f of fields) expect(of.some((e) => f in e), `${type}.${f}`).toBe(true);
      expect(of.some((e) => fields.every((f) => !(f in e))), `${type} without them`).toBe(true);
    }
    const lines = fx.events.filter((e: { type: string; diff?: unknown }) => e.type === 'tool.updated' && e.diff).map((e: { diff: { line?: number } }) => e.diff.line);
    expect(lines).toContain(40);
    expect(lines).toContain(undefined);
    const statuses = new Set(fx.events.filter((e: { type: string }) => e.type === 'tool.done').map((e: { status: string }) => e.status));
    expect([...statuses].sort()).toEqual([...TOOL_STATUSES].sort());
    const kept = new Set(fx.events.map((e: { outputKept?: string }) => e.outputKept).filter(Boolean));
    expect([...kept].sort()).toEqual(['head', 'tail']);
    for (const v of fx.events.map((e: { variant?: string }) => e.variant).filter(Boolean)) expect(TOOL_VARIANTS).toContain(v);
    // No event carries an attachment's bytes.
    for (const e of fx.events as { attachments?: Record<string, unknown>[] }[]) for (const a of e.attachments ?? []) expect(Object.keys(a).sort()).toEqual(['bytes', 'mime']);
  });
  it('never throws on junk', () => {
    for (const j of ['', 'null', '[]', '"x"', '{"seq":"1"}', '\u0000', '{'.repeat(10_000)]) expect(parseChatEvent(j)).toBeNull();
    expect(parseChatEvent(new Uint8Array([0xff, 0xfe]))).toBeNull();
    expect(parseChatEvent('x'.repeat(400 * 1024))).toBeNull();
  });
});

describe('CHAT frames', () => {
  it.each(fx.frames.map((c: { name: string }) => [c.name, c]))('%s decodes and re-encodes byte-exactly', (_n, c: any) => {
    const f = decodeChatFrame(unhex(c.hex))!;
    expect(f).not.toBeNull();
    expect({ sessionId: f.sessionId, seq: String(f.seq), req: f.req, gap: f.gap, ephemeral: f.ephemeral })
      .toEqual({ sessionId: c.sessionId, seq: c.seq, req: c.req, gap: c.gap, ephemeral: c.ephemeral });
    const raw = decodeData(unhex(c.hex))!;
    expect(raw.kind).toBe(DataKind.CHAT);
    expect(Buffer.from(raw.payload).toString('hex')).toBe(c.payloadHex);
    expect(Buffer.from(encodeData(raw)).toString('hex')).toBe(c.hex);
    const payload = Buffer.from(c.payloadHex, 'hex').toString();
    expect(Buffer.from(encodeChatFrame({ sessionId: c.sessionId, seq: Number(c.seq), req: c.req, gap: c.gap, ephemeral: c.ephemeral, event: payload })).toString('hex')).toBe(c.hex);
  });
  it.each(fx.invalidFrames.map((c: { name: string; hex: string }) => [c.name, c.hex]))('%s is refused', (_n, h) => {
    expect(decodeChatFrame(unhex(h as string))).toBeNull();
  });
  it('the ephemeral flag belongs to CHAT frames only; a pane frame is not a chat frame', () => {
    expect(() => encodeData({ kind: DataKind.OUTPUT, gap: false, ephemeral: true, req: 0, paneId: 'w1:p1', offset: 0n, payload: new Uint8Array() })).toThrow();
    const pane = encodeData({ kind: DataKind.OUTPUT, gap: false, req: 0, paneId: 'c-abcdefgh', offset: 1n, payload: new Uint8Array([1]) });
    expect(decodeChatFrame(pane)).toBeNull();
    expect(decodeData(pane)!.ephemeral).toBeUndefined();
  });
  it('a frame whose payload is not an event still routes (event null)', () => {
    const f = decodeChatFrame(encodeChatFrame({ sessionId: 'c-abcdefgh', seq: 4, event: 'garbage' }))!;
    expect(f.seq).toBe(4);
    expect(f.event).toBeNull();
  });
  it('refuses a bad session id when encoding', () => {
    expect(() => encodeChatFrame({ sessionId: 'w1:p1', seq: 1, event: {} })).toThrow();
  });
  it('limits match the runner', () => {
    expect(CHAT_MAX_SEND_BYTES).toBe(65536);
    expect([CHAT_MAX_TOOL_OUTPUT, CHAT_MAX_TOOL_NAME, CHAT_MAX_TOOL_SUBJECT, CHAT_MAX_TOOL_ATTACHMENTS]).toEqual([16384, 64, 300, 20]);
  });
});

describe('chat control frames', () => {
  it.each(fx.clientFrames.map((c: { text: string; ok: boolean }) => [c.text, c.ok]))('client %s → %s', (t, ok) => {
    expect(parseClientFrame(t as string).ok).toBe(ok);
  });
  it.each(fx.runnerFrames.map((c: { name: string; text: string }) => [c.name, c.text]))('runner %s parses', (_n, t) => {
    const p = parseRunnerFrame(t as string);
    expect(p.ok).toBe(true);
    if (p.ok) expect(p.frame).toEqual(JSON.parse(t as string));
  });
  it('runner junk is refused', () => {
    for (const t of [
      { t: 'chat_attached', req: -1, sessionId: 'c-abcdefgh', ok: true },
      { t: 'chat_attached', req: 1, sessionId: '../x', ok: true },
      { t: 'chat_attached', req: 1, sessionId: 'c-abcdefgh', ok: 'yes' },
    ]) expect(parseRunnerFrame(JSON.stringify(t)).ok).toBe(false);
    // An odd code is dropped, the frame kept.
    const p = parseRunnerFrame(JSON.stringify({ t: 'chat_attached', req: 1, sessionId: 'c-abcdefgh', ok: false, code: 'NOT A CODE' }));
    expect(p.ok && p.frame).toEqual({ t: 'chat_attached', req: 1, sessionId: 'c-abcdefgh', ok: false, lastSeq: 0 });
  });
});
