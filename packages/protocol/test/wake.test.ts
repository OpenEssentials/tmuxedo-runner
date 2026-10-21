import { describe, expect, it } from 'vitest';
import { broadcastOf, parseKeepAwake, parseNetwork, sameSubnet, wakeTargets, WAKE_MAX_INTERFACES, WOL_METHODS } from '../src/wake.js';

const iface = (mac: string, ipv4: string, prefix = 24) => ({ mac, ipv4, prefix });

describe('wake', () => {
  it('computes broadcasts and subnets', () => {
    expect(broadcastOf('192.0.2.20', 24)).toBe('192.0.2.255');
    expect(broadcastOf('198.51.100.3', 16)).toBe('198.51.255.255');
    expect(broadcastOf('203.0.113.9', 20)).toBe('203.0.127.255');
    expect(broadcastOf('192.0.2.1', 32)).toBeNull();
    expect(broadcastOf('300.0.0.1', 24)).toBeNull();
    expect(sameSubnet(iface('02:00:00:00:00:01', '192.0.2.20'), iface('02:00:00:00:00:02', '192.0.2.99'))).toBe(true);
    expect(sameSubnet(iface('02:00:00:00:00:01', '192.0.2.20'), iface('02:00:00:00:00:02', '198.51.100.20'))).toBe(false);
    expect(sameSubnet(iface('02:00:00:00:00:01', '192.0.2.20', 24), iface('02:00:00:00:00:02', '192.0.2.21', 16))).toBe(false);
  });

  it('parses networks, dropping junk', () => {
    const many = Array.from({ length: 12 }, (_, i) => iface('02:00:00:00:00:0' + (i % 10), `10.0.${i}.1`));
    expect(parseNetwork({ interfaces: many })!.interfaces).toHaveLength(WAKE_MAX_INTERFACES);
    expect(parseNetwork({ interfaces: [iface('02:AB:00:00:00:01', '192.0.2.2'), { mac: 'x', ipv4: '1.2.3.4', prefix: 24 }, iface('02:00:00:00:00:01', '1.2.3', 24), { mac: '02:00:00:00:00:01', ipv4: '1.2.3.4', prefix: 40 }, null] }))
      .toEqual({ interfaces: [iface('02:ab:00:00:00:01', '192.0.2.2')] });
    expect(parseNetwork('x')).toBeUndefined();
    expect(parseKeepAwake({ enabled: true, available: false })).toEqual({ enabled: true, available: false, held: false });
    expect(parseKeepAwake({ enabled: true, available: true, held: true, reason: 'battery' })).toEqual({ enabled: true, available: true, held: true, reason: 'battery' });
    expect(parseKeepAwake({ enabled: 'yes' })).toBeUndefined();
  });

  it('wakes through the relay interfaces on a shared subnet only', () => {
    const target = { interfaces: [iface('02:00:00:00:00:01', '192.0.2.20'), iface('02:00:00:00:00:02', '203.0.113.5', 8)] };
    const relay = { interfaces: [iface('02:00:00:00:00:09', '192.0.2.30'), iface('02:00:00:00:00:0a', '198.51.100.2', 16)] };
    expect(wakeTargets(target, relay)).toEqual([{ mac: '02:00:00:00:00:01', broadcast: '192.0.2.255' }]);
    expect(wakeTargets(target, { interfaces: [iface('02:00:00:00:00:09', '198.51.100.30')] })).toEqual([]);
    expect(WOL_METHODS).toEqual(['wol.send']);
  });
});
