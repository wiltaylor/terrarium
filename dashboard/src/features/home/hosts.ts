// The Home host cards: state, units, and series from the host metrics
// (docs/api.md §17.1). Pure.

import type { Host, HostFilesystem, HostMetricsSample } from "../../api/types";

/** The minutes of history that a card shows. */
export const CARD_MINUTES = 15;
/** How many filesystems a card shows before "more". */
export const FS_SHOWN = 4;

/**
 * - `live`: online, with a fresh sample.
 * - `stale`: online, but no sample in the last 30 s.
 * - `offline`: the link is down (the last samples can still show).
 * - `waiting`: online with metrics, but no sample yet.
 * - `unsupported`: the host agent sends no metrics (an older host agent).
 */
export type HostState = "live" | "stale" | "offline" | "waiting" | "unsupported";

export function hostState(h: Pick<Host, "status" | "capabilities" | "metrics">): HostState {
  if (h.status !== "online") return "offline";
  if (!h.capabilities?.metrics) return "unsupported";
  if (!h.metrics) return "waiting";
  return h.metrics.stale ? "stale" : "live";
}

export const STATE_CHIP: Record<HostState, { label: string; cls: string }> = {
  live: { label: "online", cls: "ok" },
  stale: { label: "stale", cls: "warn" },
  offline: { label: "offline", cls: "bad" },
  waiting: { label: "online", cls: "ok" },
  unsupported: { label: "online", cls: "ok" },
};

/** Online hosts first, then by name. */
export function sortHosts<T extends Pick<Host, "name" | "status">>(hosts: T[]): T[] {
  return [...hosts].sort((a, b) => Number(b.status === "online") - Number(a.status === "online") || a.name.localeCompare(b.name));
}

const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

/** `512 B`, `1.2 MB`, `1.8 TB` (powers of 1024, as the rest of the dashboard). */
export function size(n: number): string {
  let v = Math.max(0, n);
  let i = 0;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i++;
  }
  if (i === 0) return `${Math.round(v)} B`;
  return `${v >= 100 ? Math.round(v) : v.toFixed(1)} ${UNITS[i]}`;
}

/** `1.2 MB/s`. */
export const rate = (bytesPerSec: number): string => `${size(bytesPerSec)}/s`;

/** `23%`; one decimal below 10. */
export function pct(n: number): string {
  const v = Math.max(0, Math.min(100, n));
  return `${v < 10 && v > 0 ? v.toFixed(1) : Math.round(v)}%`;
}

/** The meter level of a used share: warn at 80%, full at 90%. */
export function spaceLevel(usedPct: number): "ok" | "warn" | "full" {
  if (usedPct >= 90) return "full";
  if (usedPct >= 80) return "warn";
  return "ok";
}

/** The filesystems, the fullest first, and how many are hidden past `shown`. */
export function fullestFirst(fs: HostFilesystem[], shown = FS_SHOWN): { shown: HostFilesystem[]; all: HostFilesystem[]; hidden: number } {
  const all = [...fs].sort((a, b) => b.used_pct - a.used_pct || a.mount.localeCompare(b.mount));
  return { shown: all.slice(0, shown), all, hidden: Math.max(0, all.length - shown) };
}

/** One metric of a sample. `undefined` when the sample has no such part. */
export type MetricPick = (s: HostMetricsSample) => number | undefined;

export const METRICS = {
  cpu: (s) => s.cpu?.busy_pct,
  memory: (s) => s.memory?.used_pct,
  rx: (s) => s.net?.rx_bytes_per_sec,
  tx: (s) => s.net?.tx_bytes_per_sec,
  read: (s) => s.disk_io?.read_bytes_per_sec,
  write: (s) => s.disk_io?.write_bytes_per_sec,
} satisfies Record<string, MetricPick>;

export interface Point {
  at: number;
  value: number;
}

/** The points of one metric at or after `since`, oldest first. Samples without the part are left out. */
export function series(samples: HostMetricsSample[], pick: MetricPick, since: number): Point[] {
  const out: Point[] = [];
  for (const s of samples) {
    const at = Date.parse(s.at);
    const v = pick(s);
    if (at >= since && v !== undefined && Number.isFinite(v)) out.push({ at, value: v });
  }
  return out.sort((a, b) => a.at - b.at);
}

/**
 * The y position of each point, 0 to 100. A share uses its own scale
 * (`max` 100). A rate scales to its peak, but at least `floor`, so an idle
 * link stays a flat line at the bottom.
 */
export function scaled(points: Point[], max?: number, floor = 1024): number[] {
  const top = max ?? Math.max(floor, ...points.map((p) => p.value));
  return points.map((p) => (top > 0 ? Math.min(100, (p.value / top) * 100) : 0));
}

/** The scale top of a rate series, for the text that says what the line shows. */
export const peak = (points: Point[]): number => Math.max(0, ...points.map((p) => p.value));

/** The index of the point whose time is nearest to `at`. */
export function nearest(points: Point[], at: number): number {
  let best = -1;
  let dist = Infinity;
  points.forEach((p, i) => {
    const d = Math.abs(p.at - at);
    if (d < dist) {
      dist = d;
      best = i;
    }
  });
  return best;
}

/** `10:42:10` in local time. */
export const clock = (at: number): string => new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });

/**
 * SVG path `d` on a time axis from `since` to `until` in a w x h box. `ys`
 * are 0 to 100. A gap longer than `gapMs` (missed samples, a link that was
 * down) breaks the line, so it does not draw values that do not exist.
 */
export function timePath(points: Point[], ys: number[], since: number, until: number, w: number, h: number, gapMs = 35_000): string {
  const span = Math.max(1, until - since);
  let d = "";
  points.forEach((p, i) => {
    const x = ((p.at - since) / span) * w;
    const y = h - 1 - ((ys[i] ?? 0) / 100) * (h - 2);
    const move = i === 0 || p.at - points[i - 1]!.at > gapMs;
    const alone = move && (i === points.length - 1 || points[i + 1]!.at - p.at > gapMs);
    // A lone point is a short stroke: its round cap draws a dot.
    d += `${d ? " " : ""}${move ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}${alone ? " l0.1,0" : ""}`;
  });
  return d;
}

/** The x position (0 to w) of a time on the axis. */
export const timeX = (at: number, since: number, until: number, w: number): number => ((at - since) / Math.max(1, until - since)) * w;
