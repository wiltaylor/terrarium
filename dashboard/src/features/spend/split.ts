// Usage totals from `GET /api/usage` points (docs/api.md §16.1). Pure.

import type { UsagePoint } from "../../api/types";

export interface SpendRow {
  /** The group key: a subscription, project, model, or agent ULID. */
  key: string;
  /** What to show. For agents, the name when it is known. */
  name: string;
  input: number;
  output: number;
  /** Cache reads plus cache writes. */
  cache: number;
  messages: number;
  turns: number;
  /** input + output. */
  tokens: number;
}

/** Sum a grouped series by `key`, biggest first. `label` turns a key into a name. */
export function totalsByKey(points: UsagePoint[], label: (key: string) => string = (k) => k): SpendRow[] {
  const m = new Map<string, SpendRow>();
  for (const p of points) {
    const key = p.key ?? "";
    const r = m.get(key) ?? { key, name: key ? label(key) : "unknown", input: 0, output: 0, cache: 0, messages: 0, turns: 0, tokens: 0 };
    r.input += p.input_tokens;
    r.output += p.output_tokens;
    r.cache += p.cache_read + p.cache_write;
    r.messages += p.messages;
    // `turns` counts distinct turns per bucket; summing buckets can count a turn twice at a bucket edge.
    r.turns += p.turns;
    r.tokens += p.input_tokens + p.output_tokens;
    m.set(key, r);
  }
  return [...m.values()].sort((a, b) => b.tokens - a.tokens || a.name.localeCompare(b.name));
}

/**
 * The rows of a `group=card` series, biggest first, named `TER-42 Title`.
 * Points with no key (samples with no card) are left out.
 */
export function cardRows(points: UsagePoint[], limit = Infinity): SpendRow[] {
  const titles = new Map<string, string>();
  for (const p of points) if (p.key && p.title) titles.set(p.key, p.title);
  const label = (k: string) => (titles.has(k) ? `${k} ${titles.get(k)}` : k);
  return totalsByKey(
    points.filter((p) => p.key),
    label,
  ).slice(0, limit);
}

/** The project of a card ID, from the card prefixes (`{TER: "terrarium"}`). */
export function cardProject(id: string, prefixes: Record<string, string>): string | undefined {
  const i = id.lastIndexOf("-");
  return i > 0 ? prefixes[id.slice(0, i)] : undefined;
}

/** The bucket starts from `since` to `now`, oldest first. */
export function bucketStarts(since: number, now: number, bucket: "hour" | "day"): string[] {
  const step = bucket === "day" ? 86_400_000 : 3_600_000;
  const out: string[] = [];
  for (let t = Math.floor(since / step) * step; t <= now; t += step) out.push(new Date(t).toISOString().replace(/\.\d{3}Z$/, "Z"));
  return out;
}

/** Input + output tokens per bucket for one key (or every point when `key` is undefined), zeros filled. */
export function tokenSeries(points: UsagePoint[], buckets: string[], key?: string): number[] {
  const by = new Map<string, number>();
  for (const p of points) {
    if (key !== undefined && p.key !== key) continue;
    const b = normalizeBucket(p.bucket);
    by.set(b, (by.get(b) ?? 0) + p.input_tokens + p.output_tokens);
  }
  return buckets.map((b) => by.get(b) ?? 0);
}

/** `2026-10-24T06:00:00Z` and `2026-10-24T06:00:00.000Z` are the same bucket. */
export const normalizeBucket = (b: string): string => {
  const t = Date.parse(b);
  return Number.isNaN(t) ? b : new Date(t).toISOString().replace(/\.\d{3}Z$/, "Z");
};

/** Scale values to 0..100 for `sparkPath`. */
export function toPercent(values: number[]): number[] {
  const max = Math.max(0, ...values);
  return max > 0 ? values.map((v) => (v / max) * 100) : values.map(() => 0);
}
