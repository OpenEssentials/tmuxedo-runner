import { generateKeyPairSync } from 'node:crypto';
import { describe, expect, it } from 'vitest';
import { open, publicKeyOf, QUEUED_MAX_SEALED_BYTES, QUEUED_MAX_TEXT_BYTES, queuedContext, seal, validQueuedId } from '../src/secrets.js';

describe('queued prompts (runner tmuxedo_protocol::queued)', () => {
  it('uses the runner’s context string and id rule', () => {
    expect(queuedContext('m1', 'q_abcdef12')).toBe('m1|queued:q_abcdef12');
    expect(validQueuedId('q_abcdef12')).toBe(true);
    expect(validQueuedId('short')).toBe(false);
    expect(validQueuedId('has space here')).toBe(false);
    expect(validQueuedId('a'.repeat(65))).toBe(false);
  });

  it('a sealed prompt opens only under its own id and machine, and fits the cap', () => {
    const pk = generateKeyPairSync('x25519').privateKey.export({ format: 'jwk' }).d!;
    const body = Buffer.from(JSON.stringify({ v: 1, text: 'x'.repeat(QUEUED_MAX_TEXT_BYTES), target: { chat: 'abcdefgh12' } }));
    const s = seal(publicKeyOf(pk), body, queuedContext('m1', 'q_abcdef12'));
    expect(s.length).toBeLessThanOrEqual(QUEUED_MAX_SEALED_BYTES);
    expect(open(pk, s, queuedContext('m1', 'q_abcdef12'))?.equals(body)).toBe(true);
    expect(open(pk, s, queuedContext('m1', 'q_abcdef13'))).toBeNull();
    expect(open(pk, s, queuedContext('m2', 'q_abcdef12'))).toBeNull();
  });
});
