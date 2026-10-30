// Mock host metrics (docs/api.md §17.1): a sample every 10 s for each host
// agent with the `metrics` capability, made from the time, so the values
// change from poll to poll and the history agrees with the latest sample.

import type { Host, HostFilesystem, HostMetricsSample } from "../types";

const STEP = 10_000;
const GB = 1024 ** 3;

interface Profile {
  cores: number;
  memGb: number;
  /** The mean CPU busy share and memory share. */
  cpu: number;
  mem: number;
  /** Peak network and disk rates, bytes per second. */
  net: number;
  disk: number;
  fs: Omit<HostFilesystem, "used_bytes" | "avail_bytes" | "used_pct">[];
  /** The used share of each filesystem. */
  used: number[];
}

const PROFILES: Record<string, Profile> = {
  lab1: {
    cores: 16,
    memGb: 94,
    cpu: 22,
    mem: 38,
    net: 4 * 1024 ** 2,
    disk: 60 * 1024 ** 2,
    fs: [
      { mount: "/", fstype: "btrfs", device: "/dev/mapper/luks-97cd", total_bytes: 1862 * GB },
      { mount: "/boot", fstype: "vfat", device: "/dev/nvme0n1p1", total_bytes: 1 * GB },
      { mount: "/srv/labs", fstype: "ext4", device: "/dev/sda1", total_bytes: 3726 * GB },
      { mount: "/var/backups", fstype: "xfs", device: "/dev/sdb1", total_bytes: 931 * GB },
    ],
    used: [0.61, 0.32, 0.84, 0.93],
  },
  build1: {
    cores: 8,
    memGb: 31,
    cpu: 64,
    mem: 71,
    net: 900 * 1024,
    disk: 140 * 1024 ** 2,
    fs: [{ mount: "/", fstype: "ext4", device: "/dev/vda1", total_bytes: 240 * GB }],
    used: [0.82],
  },
};

/** A small repeatable wobble in -1..1 for a host, a metric, and a time. */
function wobble(seed: string, at: number, period: number): number {
  let h = 0;
  for (const c of seed) h = (h * 31 + c.charCodeAt(0)) | 0;
  const slow = Math.sin(at / period + (h % 100));
  const fast = Math.sin(at / (period / 7) + (h % 37)) * 0.35;
  return Math.max(-1, Math.min(1, slow * 0.65 + fast));
}

const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

/** The sample of `host` at the 10 s step at or before `at`. */
export function mockSample(host: string, at: number): HostMetricsSample | null {
  const p = PROFILES[host];
  if (!p) return null;
  const t = Math.floor(at / STEP) * STEP;
  const cpu = clamp(p.cpu + wobble(`${host}cpu`, t, 90_000) * p.cpu * 0.8, 0.5, 100);
  const mem = clamp(p.mem + wobble(`${host}mem`, t, 400_000) * 6, 1, 99);
  const burst = (k: string, peak: number) => Math.max(0, (wobble(`${host}${k}`, t, 120_000) + 0.4) / 1.4) ** 3 * peak;
  const memTotal = p.memGb * GB;
  return {
    at: new Date(t).toISOString(),
    interval_secs: 10,
    cpu: { busy_pct: cpu, cores: p.cores, load1: (cpu / 100) * p.cores * 1.1 },
    memory: { total_bytes: memTotal, used_bytes: Math.round((memTotal * mem) / 100), used_pct: mem, swap_total_bytes: 8 * GB, swap_used_bytes: Math.round(0.4 * GB) },
    net: { rx_bytes_per_sec: burst("rx", p.net), tx_bytes_per_sec: burst("tx", p.net / 4) },
    disk_io: { read_bytes_per_sec: burst("rd", p.disk / 3), write_bytes_per_sec: burst("wr", p.disk) },
    filesystems: p.fs.map((f, i) => {
      const used = Math.round(f.total_bytes * clamp(p.used[i]! + wobble(`${host}fs${i}`, t, 600_000) * 0.003, 0, 1));
      const avail = f.total_bytes - used;
      return { ...f, used_bytes: used, avail_bytes: avail, used_pct: (used / (used + avail)) * 100 };
    }),
  };
}

/** The samples of the last `minutes`, oldest first. */
export function mockHistory(host: string, minutes: number, now = Date.now()): HostMetricsSample[] {
  const out: HostMetricsSample[] = [];
  const end = Math.floor(now / STEP) * STEP;
  for (let t = end - minutes * 60_000 + STEP; t <= end; t += STEP) {
    const s = mockSample(host, t);
    if (s) out.push(s);
  }
  return out;
}

/** A host row with its latest sample, as `GET /api/hosts` gives it. */
export function withMetrics(h: Host, now = Date.now()): Host {
  if (!h.capabilities?.metrics) return { ...h, metrics: null };
  const s = h.status === "online" ? mockSample(h.name, now) : null;
  return { ...h, metrics: s ? { ...s, stale: false } : null };
}
