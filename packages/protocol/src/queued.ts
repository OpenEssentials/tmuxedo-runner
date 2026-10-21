/**
 * Queued prompts' shared constants (docs/protocol.md "Queued prompts", runner
 * crate `tmuxedo_protocol::queued`). No imports: used by ./secrets (Node) and
 * ./seal (browser and app).
 *
 * A prompt for an offline machine is sealed to the runner's key under
 * `<machineId>|queued:<id>`; the plaintext is
 * `{v: 1, text, target: {chat} | {pane}, expiresAt?}` (ms since the epoch).
 */
export const QUEUED_FEATURE = 'queued_prompts';
export const QUEUED_METHOD = 'queued.deliver';
export const QUEUED_MAX_TEXT_BYTES = 64 * 1024;
export const QUEUED_MAX_SEALED_BYTES = 96 * 1024;
export const queuedContext = (machineId: string, id: string) => `${machineId}|queued:${id}`;
export const validQueuedId = (id: string) => /^[A-Za-z0-9_-]{8,64}$/.test(id);
