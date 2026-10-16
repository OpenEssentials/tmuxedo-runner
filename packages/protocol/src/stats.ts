/**
 * Machine stats (docs/protocol.md "Machine stats"): CPU, memory and disk of a
 * machine, sent only while someone is looking at it. Numbers only: a sample
 * never names a process, a user or a path, and `parseStatsSample` rebuilds it
 * field by field, so nothing a runner adds beside them is passed on. A field
 * the machine cannot report is absent ("not available"), never a guess.
 *
 * The thresholds here are the ones every client draws with, so the bars, the
 * popover and a later phone card cannot disagree. Memory's "full" mark is the
 * runner's memory watchdog threshold (it stops the largest process there).
 * Shared fixtures: ../fixtures/stats.json (also checked by the runner).
 */

export interface StatsCpu {
  /** Share of what the machine may use (all cores, or the container's quota) since the previous sample; absent on the first. */
  pct?: number;
  /** Cores of the host. */
  cores?: number;
  /** The container's CPU quota in cores, when it has one. */
  quota?: number;
  /** Share of scheduler periods the container was held back in since the previous sample. */
  throttledPct?: number;
  /** One-minute load average (hosts only). */
  load1?: number;
}

export interface StatsMem {
  used: number;
  limit: number;
  /** `limit` is a container limit, not the size of the host's memory. */
  limited?: boolean;
  /** Percent of the last 10 s some process waited for memory (PSI `some avg10`). */
  pressure10?: number;
  swapUsed?: number;
}

/** `id` is a fixed word (`home`), never a path. */
export interface StatsDisk { id: string; free: number; total: number }

/** One sample; `at` is Unix milliseconds. */
export interface StatsSample { at: number; cpu?: StatsCpu; mem?: StatsMem; disks: StatsDisk[] }

/** One point of the short history: [at, cpu percent or null, memory percent or null]. */
export type StatsPoint = [at: number, cpu: number | null, mem: number | null];

/** What a client is sent: a sample, the machine, and (on the first frame after subscribing) the history so far. */
export interface StatsFrame extends StatsSample { t: 'stats'; machineId?: string; history?: StatsPoint[] }

/** Points kept: 10 minutes at one every 2 s. */
export const STATS_HISTORY_MAX = 300;
/** The history covers this long; older points are dropped. */
export const STATS_HISTORY_MS = 10 * 60 * 1000;
/** Most disks in one sample. */
export const STATS_MAX_DISKS = 4;

/** Memory: amber from here… */
export const STATS_MEM_HIGH = 80;
/** …and red here, where the runner's memory watchdog stops the largest process. */
export const STATS_MEM_FULL = 90;
/** Memory pressure (avg10) above this is amber whatever the percentage. */
export const STATS_PRESSURE_HIGH = 10;
/** CPU at or above this for `STATS_CPU_SUSTAIN_MS` is amber. */
export const STATS_CPU_HIGH = 85;
export const STATS_CPU_SUSTAIN_MS = 30_000;
/** Disk: amber below this share free, red below the next. */
export const STATS_DISK_LOW_FREE = 15;
export const STATS_DISK_FULL_FREE = 5;

const num = (v: unknown, min: number, max: number): v is number => typeof v === 'number' && Number.isFinite(v) && v >= min && v <= max;
const bytes = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
const obj = (v: unknown): v is Record<string, unknown> => !!v && typeof v === 'object' && !Array.isArray(v);

/**
 * A runner's sample, checked and rebuilt (unknown fields are dropped). Null
 * when any field present is not what it should be. Never throws.
 */
export function parseStatsSample(v: unknown): StatsSample | null {
  if (!obj(v) || !bytes(v.at)) return null;
  const out: StatsSample = { at: v.at, disks: [] };
  if (v.cpu !== undefined) {
    const c = v.cpu;
    if (!obj(c)) return null;
    const cpu: StatsCpu = {};
    if (c.pct !== undefined) { if (!num(c.pct, 0, 100)) return null; cpu.pct = c.pct; }
    if (c.cores !== undefined) { if (!num(c.cores, 1, 4096) || !Number.isInteger(c.cores)) return null; cpu.cores = c.cores; }
    if (c.quota !== undefined) { if (!num(c.quota, 0.001, 4096)) return null; cpu.quota = c.quota; }
    if (c.throttledPct !== undefined) { if (!num(c.throttledPct, 0, 100)) return null; cpu.throttledPct = c.throttledPct; }
    if (c.load1 !== undefined) { if (!num(c.load1, 0, 1e6)) return null; cpu.load1 = c.load1; }
    if (Object.keys(cpu).length) out.cpu = cpu;
  }
  if (v.mem !== undefined) {
    const m = v.mem;
    if (!obj(m) || !bytes(m.used) || !bytes(m.limit) || m.limit === 0) return null;
    const mem: StatsMem = { used: m.used, limit: m.limit };
    if (m.limited !== undefined) { if (typeof m.limited !== 'boolean') return null; if (m.limited) mem.limited = true; }
    if (m.pressure10 !== undefined) { if (!num(m.pressure10, 0, 100)) return null; mem.pressure10 = m.pressure10; }
    if (m.swapUsed !== undefined) { if (!bytes(m.swapUsed)) return null; mem.swapUsed = m.swapUsed; }
    out.mem = mem;
  }
  if (v.disks !== undefined) {
    if (!Array.isArray(v.disks) || v.disks.length > STATS_MAX_DISKS) return null;
    for (const d of v.disks as unknown[]) {
      if (!obj(d) || typeof d.id !== 'string' || !/^[a-z]{1,16}$/.test(d.id) || !bytes(d.free) || !bytes(d.total) || d.total === 0 || d.free > d.total) return null;
      out.disks.push({ id: d.id, free: d.free, total: d.total });
    }
  }
  return out;
}

/** What a client was sent, checked the same way (with the history, when there is one). Never throws. */
export function parseStatsFrame(v: unknown): StatsFrame | null {
  const s = parseStatsSample(v);
  if (!s || !obj(v)) return null;
  const out: StatsFrame = { t: 'stats', ...s };
  if (typeof v.machineId === 'string' && v.machineId.length <= 128) out.machineId = v.machineId;
  if (Array.isArray(v.history)) {
    const history: StatsPoint[] = [];
    for (const p of v.history.slice(-STATS_HISTORY_MAX) as unknown[]) {
      if (!Array.isArray(p) || !bytes(p[0])) continue;
      history.push([p[0], num(p[1], 0, 100) ? p[1] : null, num(p[2], 0, 100) ? p[2] : null]);
    }
    out.history = history;
  }
  return out;
}

const round1 = (v: number) => Math.round(v * 10) / 10;

/** Memory used as a percentage of its limit (0..100, one decimal); null when not reported. */
export function memPct(s: Pick<StatsSample, 'mem'>): number | null {
  return s.mem ? round1(Math.min(100, (s.mem.used / s.mem.limit) * 100)) : null;
}

/** The home disk (or the first one); null when none was reported. */
export function homeDisk(s: Pick<StatsSample, 'disks'>): StatsDisk | null {
  return s.disks.find((d) => d.id === 'home') ?? s.disks[0] ?? null;
}

/** Disk space used as a percentage (0..100, one decimal); null when not reported. */
export function diskUsedPct(s: Pick<StatsSample, 'disks'>): number | null {
  const d = homeDisk(s);
  return d ? round1(((d.total - d.free) / d.total) * 100) : null;
}

/** The history point of a sample. */
export function statsPoint(s: StatsSample): StatsPoint {
  return [s.at, s.cpu?.pct ?? null, memPct(s)];
}

/**
 * `history` with `p` added: ordered by time, nothing older than 10 minutes
 * before `p`, never more than STATS_HISTORY_MAX points. A point no newer than
 * the last one is ignored (a clock that went back, a frame sent twice).
 */
export function pushStatsPoint(history: readonly StatsPoint[], p: StatsPoint): StatsPoint[] {
  const last = history[history.length - 1];
  if (last && p[0] <= last[0]) return history as StatsPoint[];
  const from = p[0] - STATS_HISTORY_MS;
  const out = history.filter((h) => h[0] >= from);
  out.push(p);
  return out.length > STATS_HISTORY_MAX ? out.slice(out.length - STATS_HISTORY_MAX) : out;
}

/** `ok`: nothing to say. `high`: amber. `full`: red. `none`: the machine did not report it. */
export type StatsLevel = 'none' | 'ok' | 'high' | 'full';

export function statsMemLevel(s: Pick<StatsSample, 'mem'>): StatsLevel {
  const pct = memPct(s);
  if (pct === null) return 'none';
  if (pct >= STATS_MEM_FULL) return 'full';
  if (pct >= STATS_MEM_HIGH || (s.mem?.pressure10 ?? 0) > STATS_PRESSURE_HIGH) return 'high';
  return 'ok';
}

export function statsDiskLevel(s: Pick<StatsSample, 'disks'>): StatsLevel {
  const used = diskUsedPct(s);
  if (used === null) return 'none';
  const free = 100 - used;
  if (free < STATS_DISK_FULL_FREE) return 'full';
  if (free < STATS_DISK_LOW_FREE) return 'high';
  return 'ok';
}

/**
 * CPU is `high` only when it stayed at or above STATS_CPU_HIGH for the last
 * 30 s of `history` (a short burst is what a build looks like, not a problem).
 */
export function statsCpuLevel(s: Pick<StatsSample, 'cpu' | 'at'>, history: readonly StatsPoint[] = []): StatsLevel {
  const pct = s.cpu?.pct;
  if (pct === undefined) return 'none';
  if (pct < STATS_CPU_HIGH) return 'ok';
  const from = s.at - STATS_CPU_SUSTAIN_MS;
  const window = history.filter((h) => h[0] >= from && h[0] <= s.at);
  // The window must reach back the whole 30 s (give or take a slow sample), not start a moment ago.
  const covers = window.length >= 2 && window[0]![0] <= from + 10_000;
  return covers && window.every((h) => h[1] !== null && h[1] >= STATS_CPU_HIGH) ? 'high' : 'ok';
}

/** Bytes as people read them: `512 MiB`, `2.4 GiB`, `48 GiB`. */
export function formatBytes(n: number): string {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  let v = Math.max(0, n);
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v >= 10 || i === 0 ? Math.round(v) : round1(v)} ${units[i]}`;
}
