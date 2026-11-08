// The project dashboard (docs/api.md §30) without Solid: the state of a
// block (its status, stale, pending, off), value text, the chart range and
// bucket, the y scale and ticks, the table columns, the layout checks that
// the daemon makes, and the block moves of the edit mode.

import type { DashboardBlock, DashboardBlockKind, DashboardBlockSize, DashboardRange, ItemOwner, JobStats, Monitor, MonitorKind, MonitorPoint, MonitorRow, MonitorStatus } from "../../api/types";

/** The state that a block shows: a result status, or why it has none. */
export type BlockState = MonitorStatus | "stale" | "pending" | "off";

/**
 * The label and the glyph of each state. The glyph and the label go with
 * the colour, so the state never reads by colour alone.
 */
export const STATE_META: Record<BlockState, { label: string; glyph: string; cls: string }> = {
  ok: { label: "OK", glyph: "✓", cls: "ok" },
  warn: { label: "Warning", glyph: "▲", cls: "warn" },
  crit: { label: "Critical", glyph: "✕", cls: "crit" },
  unknown: { label: "Unknown", glyph: "?", cls: "unknown" },
  stale: { label: "Stale", glyph: "⏱", cls: "unknown" },
  pending: { label: "No result yet", glyph: "…", cls: "unknown" },
  off: { label: "Off", glyph: "–", cls: "unknown" },
};

/** A block is stale after this many periods with no result. */
export const STALE_PERIODS = 3;

/** True when the last result is older than three periods (and the timeout) of the monitor. */
export function isStale(m: Pick<Monitor, "period_secs" | "timeout_secs" | "last">, now: number): boolean {
  if (!m.last) return false;
  const at = Date.parse(m.last.at);
  if (!Number.isFinite(at)) return false;
  return now - at > (STALE_PERIODS * m.period_secs + m.timeout_secs) * 1000;
}

/** The state of a monitor now. */
export function blockState(m: Pick<Monitor, "enabled" | "period_secs" | "timeout_secs" | "last">, now: number): BlockState {
  if (!m.enabled) return "off";
  if (!m.last) return "pending";
  if (isStale(m, now)) return "stale";
  return m.last.status;
}

/** The unit of the last result: the JSON `unit`, else the monitor's. */
export const unitOf = (m: Pick<Monitor, "unit" | "last">): string | undefined => m.last?.json?.unit ?? m.unit ?? undefined;

/**
 * A value as text. `decimals` fixes the decimals; else they follow the size (0 from 100, 1 from 10, else 2) with
 * no trailing zeros. 10 000 and up is compact (12.9K, 4.2M). `%` goes with no space; another unit after a space.
 */
export function valueText(v: number | undefined | null, decimals?: number, unit?: string | null): string {
  if (v === undefined || v === null || !Number.isFinite(v)) return "–";
  const abs = Math.abs(v);
  let text: string;
  if (abs >= 10_000) {
    text = new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: decimals ?? 1 }).format(v);
  } else {
    const d = decimals ?? (abs >= 100 ? 0 : abs >= 10 ? 1 : 2);
    text = new Intl.NumberFormat("en", { minimumFractionDigits: decimals ?? 0, maximumFractionDigits: d }).format(v);
  }
  if (!unit) return text;
  return unit === "%" ? `${text}%` : `${text} ${unit}`;
}

export const RANGES: readonly { id: DashboardRange; label: string; ms: number; bucket: number }[] = [
  { id: "1h", label: "1 hour", ms: 3_600_000, bucket: 60 },
  { id: "6h", label: "6 hours", ms: 6 * 3_600_000, bucket: 300 },
  { id: "24h", label: "24 hours", ms: 24 * 3_600_000, bucket: 600 },
  { id: "7d", label: "7 days", ms: 7 * 86_400_000, bucket: 3_600 },
];

/** The range of a chart block (default 24 h). */
export const rangeOf = (b: Pick<DashboardBlock, "options">) => RANGES.find((r) => r.id === (b.options.range ?? "24h")) ?? RANGES[2]!;

/** The sparkline of a stat block covers the last 24 hours. */
export const SPARK_RANGE = RANGES[2]!;

/** The block kinds that fit a monitor kind, the first being the default. */
export function kindsFor(kind: MonitorKind): DashboardBlockKind[] {
  switch (kind) {
    case "number":
      return ["stat", "chart", "status"];
    case "json":
      return ["stat", "table", "chart", "status", "text"];
    case "status":
      return ["status", "text"];
    case "text":
      return ["text", "status"];
  }
}

export const BLOCK_KINDS: readonly { id: DashboardBlockKind; label: string }[] = [
  { id: "stat", label: "Stat (big value)" },
  { id: "status", label: "Status" },
  { id: "chart", label: "Chart" },
  { id: "text", label: "Text" },
  { id: "table", label: "Table" },
  { id: "note", label: "Note (Markdown)" },
];

export const SIZES: readonly { id: DashboardBlockSize; label: string; span: number }[] = [
  { id: "s", label: "Small (1 column)", span: 1 },
  { id: "m", label: "Medium (2 columns)", span: 2 },
  { id: "l", label: "Large (full width)", span: 4 },
];

/** The ID of a new block: `b1`, `b2`, ... not in use. */
export function newBlockId(blocks: readonly Pick<DashboardBlock, "id">[]): string {
  const used = new Set(blocks.map((b) => b.id));
  for (let i = blocks.length + 1; ; i++) if (!used.has(`b${i}`)) return `b${i}`;
}

/** A new block for a monitor, with the default kind and size. */
export function blockFor(m: Pick<Monitor, "id" | "kind">, blocks: readonly Pick<DashboardBlock, "id">[]): DashboardBlock {
  const kind = kindsFor(m.kind)[0]!;
  return { id: newBlockId(blocks), kind, monitor: m.id, size: kind === "table" || kind === "text" ? "m" : "s", options: {} };
}

/** A new note block. */
export const noteBlock = (blocks: readonly Pick<DashboardBlock, "id">[], text = "## Section"): DashboardBlock => ({ id: newBlockId(blocks), kind: "note", size: "l", text, options: {} });

/** The checks of the daemon (`monitors::layout::check`), so the editor says what is wrong before a save. */
export function layoutProblems(blocks: readonly DashboardBlock[], monitors: readonly Pick<Monitor, "id" | "kind" | "name">[]): string[] {
  const out: string[] = [];
  if (blocks.length > 100) out.push(`${blocks.length} blocks; a dashboard has at most 100`);
  const ids = new Set<string>();
  blocks.forEach((b, i) => {
    const at = `Block ${i + 1}`;
    if (ids.has(b.id)) out.push(`${at}: the ID ${b.id} is used twice`);
    ids.add(b.id);
    if ((b.title ?? "").length > 120) out.push(`${at}: the title is longer than 120 characters`);
    if (b.kind === "note") {
      if (!(b.text ?? "").trim()) out.push(`${at}: a note needs text`);
      else if ((b.text ?? "").length > 4000) out.push(`${at}: the note is longer than 4000 characters`);
      return;
    }
    const m = monitors.find((x) => x.id === b.monitor);
    if (!m) {
      out.push(`${at}: pick a monitor`);
      return;
    }
    if (!kindsFor(m.kind).includes(b.kind)) out.push(`${at}: a ${b.kind} block does not fit the ${m.kind} monitor ${m.name}`);
  });
  return out;
}

/** Move the block at `from` to `to`. */
export function moveBlock<T>(blocks: readonly T[], from: number, to: number): T[] {
  const out = [...blocks];
  if (from < 0 || from >= out.length) return out;
  const clamped = Math.max(0, Math.min(out.length - 1, to));
  const [b] = out.splice(from, 1);
  out.splice(clamped, 0, b!);
  return out;
}

/** The columns of a table: the keys in order of first use, at most 20. */
export function columnsOf(rows: readonly MonitorRow[]): string[] {
  const seen: string[] = [];
  for (const r of rows) for (const k of Object.keys(r)) if (!seen.includes(k) && seen.length < 20) seen.push(k);
  return seen;
}

/** A cell as text. */
export const cellText = (v: unknown): string => (v === null || v === undefined ? "" : typeof v === "number" && !Number.isInteger(v) ? valueText(v) : String(v));

/** The columns whose values are all numbers: they align right. */
export const numericColumns = (rows: readonly MonitorRow[], cols: readonly string[]): Set<string> =>
  new Set(cols.filter((c) => rows.some((r) => typeof r[c] === "number") && rows.every((r) => r[c] === null || r[c] === undefined || typeof r[c] === "number")));

/** Clean ticks over [lo, hi]: steps of 1, 2, 2.5, or 5 times a power of ten. */
export function niceTicks(lo: number, hi: number, count = 4): number[] {
  if (!Number.isFinite(lo) || !Number.isFinite(hi)) return [];
  if (hi === lo) {
    const pad = Math.abs(lo) * 0.1 || 1;
    lo -= pad;
    hi += pad;
  }
  const raw = (hi - lo) / Math.max(1, count - 1);
  const pow = 10 ** Math.floor(Math.log10(raw));
  const step = ([1, 2, 2.5, 5, 10].find((s) => s * pow >= raw) ?? 10) * pow;
  const start = Math.floor(lo / step) * step;
  const ticks: number[] = [];
  for (let t = start; t <= hi + step * 1e-9; t += step) ticks.push(Number(t.toFixed(10)));
  if (ticks.at(-1)! < hi) ticks.push(Number((ticks.at(-1)! + step).toFixed(10)));
  return ticks;
}

/**
 * The y scale of a chart: the values of the points, and the thresholds when they are near the data (so a warn line
 * shows where it matters). Zero is in the scale when the data is all on one side and near it.
 */
export function yScale(points: readonly Pick<MonitorPoint, "value" | "min" | "max">[], thresholds: readonly (number | null | undefined)[] = []): { lo: number; hi: number; ticks: number[] } {
  const vals = points.flatMap((p) => [p.min ?? p.value, p.max ?? p.value]).filter((v): v is number => v !== undefined && Number.isFinite(v));
  if (!vals.length) return { lo: 0, hi: 1, ticks: [0, 1] };
  let lo = Math.min(...vals);
  let hi = Math.max(...vals);
  const span = hi - lo || Math.abs(hi) || 1;
  for (const t of thresholds) {
    if (t === null || t === undefined || t < lo - span || t > hi + span) continue;
    lo = Math.min(lo, t);
    hi = Math.max(hi, t);
  }
  if (lo > 0 && lo < hi * 0.5) lo = 0;
  const ticks = niceTicks(lo, hi);
  return { lo: ticks[0] ?? lo, hi: ticks.at(-1) ?? hi, ticks };
}

/** The y position (0 at the top, h at the bottom) of a value. */
export const yOf = (v: number, lo: number, hi: number, h: number): number => h - ((v - lo) / Math.max(1e-12, hi - lo)) * h;

/** The x position (0 to w) of a time. */
export const xOf = (at: number, since: number, until: number, w: number): number => ((at - since) / Math.max(1, until - since)) * w;

/**
 * The path of a line through the points with a value. A gap longer than `gapMs` (no result: the target was down)
 * breaks the line, so it does not draw values that do not exist.
 */
export function linePath(points: readonly { at: number; value: number }[], since: number, until: number, lo: number, hi: number, w: number, h: number, gapMs: number): string {
  let d = "";
  points.forEach((p, i) => {
    const x = xOf(p.at, since, until, w).toFixed(1);
    const y = yOf(p.value, lo, hi, h).toFixed(1);
    const move = i === 0 || p.at - points[i - 1]!.at > gapMs;
    const alone = move && (i === points.length - 1 || points[i + 1]!.at - p.at > gapMs);
    d += `${d ? " " : ""}${move ? "M" : "L"}${x},${y}${alone ? " l0.1,0" : ""}`;
  });
  return d;
}

/** The points of a series with a value, as times in ms. */
export function numericPoints(points: readonly Pick<MonitorPoint, "at" | "value" | "status">[]): { at: number; value: number; status: MonitorStatus }[] {
  return points.flatMap((p) => {
    const at = Date.parse(p.at);
    return p.value !== undefined && Number.isFinite(p.value) && Number.isFinite(at) ? [{ at, value: p.value, status: p.status }] : [];
  });
}

/** The index of the point nearest to `at`, or -1. */
export function nearestIndex(points: readonly { at: number }[], at: number): number {
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

/** A time label for an axis: the clock within a day, else the date. */
export function timeLabel(at: number, spanMs: number): string {
  const d = new Date(at);
  if (spanMs <= 36 * 3_600_000) return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}

/** The schedule of a monitor in words: "every 30s", or the cron expression with its zone when not UTC. */
export function scheduleText(s: Monitor["schedule"]): string {
  return s.kind === "interval" ? `every ${s.every}` : `cron ${s.expr}${s.tz && s.tz !== "UTC" ? ` (${s.tz})` : ""}`;
}

/** Count the monitors by state, for the summary line of the page. */
export function stateCounts(monitors: readonly Monitor[], now: number): Record<BlockState, number> {
  const out: Record<BlockState, number> = { ok: 0, warn: 0, crit: 0, unknown: 0, stale: 0, pending: 0, off: 0 };
  for (const m of monitors) out[blockState(m, now)]++;
  return out;
}

/** The owner of a monitor or a schedule (docs/api.md §30.7): `owned by the project`, `owned by the system`, or
 * `runs as <actor>`. A row with no owner is actor-owned. */
export const ownerText = (m: { owner?: ItemOwner; run_as: string }): string =>
  m.owner === "project" ? "owned by the project" : m.owner === "system" ? "owned by the system" : `runs as ${m.run_as}`;

/** The windows of the job-queue strip (TER-178): durations back from now, as the stats endpoint takes them. `7d` is the
 * daemon's default window. */
export const JOB_WINDOWS = [
  { id: "24h", label: "24 hours" },
  { id: "7d", label: "7 days" },
  { id: "30d", label: "30 days" },
] as const;

/** A pass rate (0 to 1, `null` with no ended attempt) as a whole percent. */
export const passRateText = (rate: number | null | undefined): string =>
  rate === null || rate === undefined || !Number.isFinite(rate) ? "–" : `${Math.round(rate * 100)}%`;

/** The quiet counts beside the rate of the job-queue strip: only the parts that are not zero, in this order. */
export function jobStatNotes(s: JobStats | null | undefined): string[] {
  if (!s) return [];
  const n = (v: number, what: string) => `${v} ${what}${v === 1 ? "" : "s"}`;
  const out: string[] = [];
  if (s.reruns.ended > 0) out.push(`${s.reruns.passed} of ${s.reruns.ended} rerun${s.reruns.ended === 1 ? "" : "s"} passed`);
  if (s.conflicts > 0) out.push(n(s.conflicts, "conflict"));
  if (s.errors > 0) out.push(n(s.errors, "error"));
  if (s.open > 0) out.push(`${s.open} open`);
  return out;
}
