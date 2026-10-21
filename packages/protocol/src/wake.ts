/**
 * Keep-awake and Wake-on-LAN (docs/protocol.md "Keep-awake and Wake-on-LAN").
 * The runner reports `hello.runner.keepAwake` and `hello.runner.network`;
 * web wakes an offline machine by calling `wol.send` on another online
 * machine of the same owner on the same network.
 */

/** Gateway only: never from a client socket (the gateway answers `forbidden_method`). */
export const WOL_SEND_METHOD = 'wol.send';
export const WOL_METHODS: readonly string[] = [WOL_SEND_METHOD];
/** The most interfaces a hello reports. */
export const WAKE_MAX_INTERFACES = 8;
/** How long the web page waits for a woken machine. */
export const WAKE_WAIT_MS = 2 * 60 * 1000;

export interface KeepAwake { enabled: boolean; available: boolean; held: boolean; reason?: 'battery' }
export interface NetInterface { mac: string; ipv4: string; prefix: number }
export interface Network { interfaces: NetInterface[] }

const MAC_RE = /^[0-9a-f]{2}(:[0-9a-f]{2}){5}$/;
const IPV4_RE = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/;

/** Dotted IPv4 → unsigned 32-bit, or null. */
export function ipv4ToInt(s: string): number | null {
  const m = IPV4_RE.exec(s);
  if (!m) return null;
  const o = m.slice(1).map(Number);
  if (o.some((x) => x > 255)) return null;
  return ((o[0] << 24) | (o[1] << 16) | (o[2] << 8) | o[3]) >>> 0;
}

function intToIpv4(n: number): string {
  return [n >>> 24, (n >>> 16) & 255, (n >>> 8) & 255, n & 255].join('.');
}

function mask(prefix: number): number {
  return prefix === 0 ? 0 : (0xffffffff << (32 - prefix)) >>> 0;
}

/** The directed broadcast of `ipv4/prefix` (null for /31, /32 or junk). */
export function broadcastOf(ipv4: string, prefix: number): string | null {
  const ip = ipv4ToInt(ipv4);
  if (ip === null || !Number.isInteger(prefix) || prefix < 0 || prefix > 30) return null;
  return intToIpv4((ip | ~mask(prefix)) >>> 0);
}

/** Two interfaces on the same IPv4 subnet (same prefix and network). */
export function sameSubnet(a: NetInterface, b: NetInterface): boolean {
  if (a.prefix !== b.prefix) return false;
  const x = ipv4ToInt(a.ipv4);
  const y = ipv4ToInt(b.ipv4);
  if (x === null || y === null) return false;
  const m = mask(a.prefix);
  return ((x & m) >>> 0) === ((y & m) >>> 0);
}

/** A valid Network or undefined (junk dropped, never thrown). */
export function parseNetwork(v: unknown): Network | undefined {
  if (!v || typeof v !== 'object' || !Array.isArray((v as { interfaces?: unknown }).interfaces)) return undefined;
  const out: NetInterface[] = [];
  for (const i of (v as { interfaces: unknown[] }).interfaces) {
    if (out.length === WAKE_MAX_INTERFACES) break;
    if (!i || typeof i !== 'object') continue;
    const o = i as Record<string, unknown>;
    const mac = typeof o.mac === 'string' ? o.mac.toLowerCase() : '';
    const ipv4 = typeof o.ipv4 === 'string' ? o.ipv4 : '';
    const prefix = o.prefix;
    if (!MAC_RE.test(mac) || ipv4ToInt(ipv4) === null) continue;
    if (typeof prefix !== 'number' || !Number.isInteger(prefix) || prefix < 0 || prefix > 32) continue;
    out.push({ mac, ipv4, prefix });
  }
  return { interfaces: out };
}

/** A valid KeepAwake or undefined. */
export function parseKeepAwake(v: unknown): KeepAwake | undefined {
  if (!v || typeof v !== 'object') return undefined;
  const o = v as Record<string, unknown>;
  if (typeof o.enabled !== 'boolean' || typeof o.available !== 'boolean') return undefined;
  const k: KeepAwake = { enabled: o.enabled, available: o.available, held: o.held === true };
  if (o.reason === 'battery') k.reason = 'battery';
  return k;
}

/** One wake attempt through a relay: the relay interface's broadcast, the target's MAC. */
export interface WakeTarget { mac: string; broadcast: string }

/**
 * What a relay with `relay` interfaces sends to wake a machine with `target`
 * interfaces: one per target interface on a subnet the relay shares.
 */
export function wakeTargets(target: Network, relay: Network): WakeTarget[] {
  const out: WakeTarget[] = [];
  for (const t of target.interfaces) {
    for (const r of relay.interfaces) {
      if (!sameSubnet(t, r)) continue;
      const broadcast = broadcastOf(r.ipv4, r.prefix);
      if (broadcast && !out.some((o) => o.mac === t.mac && o.broadcast === broadcast)) out.push({ mac: t.mac, broadcast });
    }
  }
  return out;
}
