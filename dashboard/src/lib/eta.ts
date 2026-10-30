// The ETA of a card (docs/api.md §8): the parse of an ETA value, and the
// countdown chip of the board. Pure functions, no Solid imports.

import type { Card } from "../api/types";

/** The largest ETA, in days from now (terrarium_core::eta::MAX_DAYS). */
export const MAX_ETA_DAYS = 30;
const DAY = 86_400_000;
const HOUR = 3_600_000;
const MIN = 60_000;

/** A duration of one or more `<number><unit>` parts (`45m`, `2h30m`, `1d`), in ms. `null` for a bad value or zero. */
export function parseDuration(s: string): number | null {
  const v = s.trim().toLowerCase();
  if (!/^(\d+[dhms])+$/.test(v)) return null;
  let ms = 0;
  for (const [, n, u] of v.matchAll(/(\d+)([dhms])/g)) ms += Number(n) * { d: DAY, h: HOUR, m: MIN, s: 1000 }[u as "d" | "h" | "m" | "s"];
  return ms > 0 ? ms : null;
}

/** The daemon's rule for an ETA value: a duration from now or an RFC 3339 time, in the future, at most 30 days away. */
export function resolveEta(v: string, now = Date.now()): { at: number } | { error: string } {
  const t = v.trim();
  const d = parseDuration(t);
  const at = d !== null ? now + d : /^\d{4}-\d{2}-\d{2}T/.test(t) ? Date.parse(t) : NaN;
  if (Number.isNaN(at)) return { error: `bad ETA \`${t}\`: use a duration such as 45m, 2h30m, or 1d, or an RFC 3339 time` };
  if (at <= now) return { error: `the ETA \`${t}\` is in the past; give a time in the future` };
  if (at - now > MAX_ETA_DAYS * DAY) return { error: `the ETA \`${t}\` is more than ${MAX_ETA_DAYS} days away; split the work into smaller cards` };
  return { at };
}

const pad = (n: number) => String(n).padStart(2, "0");

/** A span for the chip: `45s`, `34m 05s` under an hour, `1h 12m`, `2d 3h`. `seconds: false` drops the seconds under an hour. */
export function span(ms: number, seconds = true): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return seconds ? `${m}m ${pad(s % 60)}s` : `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${pad(m % 60)}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** True when the card is done or archived: its ETA shows no countdown. */
export const etaStopped = (c: Pick<Card, "column" | "archived">) => c.column === "done" || c.archived;

export interface Countdown {
  /** `1h 12m left`, `overdue 12m`. */
  label: string;
  overdue: boolean;
  /** Elapsed of the whole estimate, 0 to 1; `null` without `eta_set_at`. */
  progress: number | null;
  /** The absolute ETA and who set it, for the title and the aria-label. */
  title: string;
}

/** The countdown chip of a card, or `null` for a card with no ETA or a done or archived card. */
export function countdown(c: Card, now = Date.now()): Countdown | null {
  if (!c.eta_at || etaStopped(c)) return null;
  const at = Date.parse(c.eta_at);
  if (Number.isNaN(at)) return null;
  const left = at - now;
  const overdue = left < 0;
  const start = c.eta_set_at ? Date.parse(c.eta_set_at) : NaN;
  const total = at - start;
  const progress = Number.isNaN(start) || total <= 0 ? null : Math.min(1, Math.max(0, (now - start) / total));
  const by = c.eta_set_by_name ?? c.eta_set_by;
  return {
    label: overdue ? `overdue ${span(-left, false)}` : `${span(left)} left`,
    overdue,
    progress,
    title: `ETA ${new Date(at).toLocaleString()}${by ? `, set by ${by}` : ""}${overdue ? " (overdue)" : ""}`,
  };
}

/** The ETA line of a done card: `done in 1h 30m (estimated 2h 00m)`. `null` without the times. */
export function doneSummary(c: Card): string | null {
  if (!c.eta_at || !c.eta_set_at || !c.done_at) return null;
  const start = Date.parse(c.eta_set_at);
  const took = Date.parse(c.done_at) - start;
  const est = Date.parse(c.eta_at) - start;
  if (Number.isNaN(took) || Number.isNaN(est)) return null;
  return `done in ${span(took, false)} (estimated ${span(est, false)})`;
}

/**
 * The delay of the board's one shared timer: 1 s while a countdown is under one hour (the chip shows seconds), else
 * 30 s, or less when a card comes under one hour before then. `null` when no card counts down: no timer.
 */
export function tickInterval(cards: Card[], now = Date.now()): number | null {
  let next: number | null = null;
  for (const c of cards) {
    if (!c.eta_at || etaStopped(c)) continue;
    const left = Date.parse(c.eta_at) - now;
    if (Number.isNaN(left)) continue;
    if (left >= 0 && left < HOUR) return 1000;
    // A card that comes under one hour before the next tick: wake at that time.
    const wake = left >= HOUR ? Math.max(1000, left - HOUR + 1) : 30_000;
    next = Math.min(next ?? 30_000, wake, 30_000);
  }
  return next;
}
