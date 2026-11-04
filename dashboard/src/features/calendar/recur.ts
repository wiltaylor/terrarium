// The recurrence rules of calendar events (docs/api.md §33.2), after the
// daemon's `calendar/recur.rs`: the RRULE subset, its checks, and the
// expansion into occurrences. The mock uses it to answer as the daemon
// does; the page uses the zone helpers for the dialog.
//
// A rule repeats the wall clock time of the first start in its zone. A
// local time that the zone skips moves on in 30-minute steps; a local time
// that comes twice takes the first. Weeks start on Monday; a monthly rule
// skips a month without the day; `count` counts skipped occurrences too.

import type { CalendarDay, CalendarRule } from "../../api/types";

export const DAYS: CalendarDay[] = ["mo", "tu", "we", "th", "fr", "sa", "su"];
const MIN = 60_000;
const DAY = 86_400_000;

export interface Parts {
  y: number;
  /** 1-12. */
  m: number;
  d: number;
  h: number;
  mi: number;
  s: number;
}

const fmtCache = new Map<string, Intl.DateTimeFormat>();
function fmt(tz: string): Intl.DateTimeFormat {
  let f = fmtCache.get(tz);
  if (!f) {
    f = new Intl.DateTimeFormat("en-US", { timeZone: tz, hourCycle: "h23", year: "numeric", month: "numeric", day: "numeric", hour: "numeric", minute: "numeric", second: "numeric" });
    fmtCache.set(tz, f);
  }
  return f;
}

/** Is `tz` a zone that this browser knows? */
export function validZone(tz: string): boolean {
  try {
    fmt(tz);
    return true;
  } catch {
    return false;
  }
}

/** The wall clock of instant `ms` in `tz`. */
export function zoned(ms: number, tz: string): Parts {
  const out: Record<string, number> = {};
  for (const p of fmt(tz).formatToParts(new Date(ms))) if (p.type !== "literal") out[p.type] = Number(p.value);
  return { y: out.year!, m: out.month!, d: out.day!, h: out.hour! % 24, mi: out.minute!, s: out.second! };
}

const naive = (p: Parts) => Date.UTC(p.y, p.m - 1, p.d, p.h, p.mi, p.s);
const offset = (ms: number, tz: string) => naive(zoned(ms, tz)) - Math.floor(ms / 1000) * 1000;
const same = (a: Parts, b: Parts) => a.y === b.y && a.m === b.m && a.d === b.d && a.h === b.h && a.mi === b.mi;

/** The first instant of the local time `p` in `tz` (see the file head for gaps and overlaps). */
export function localInstant(p: Parts, tz: string): number {
  for (let step = 0; step <= 24; step++) {
    const guess = naive(p) + step * 30 * MIN;
    const want = zoned(guess, "UTC");
    const cands = [guess - offset(guess - DAY, tz), guess - offset(guess + DAY, tz)].sort((a, b) => a - b);
    for (const c of cands) if (same(zoned(c, tz), want)) return c;
  }
  return naive(p);
}

/** `YYYY-MM-DD` of the parts. */
export const dateText = (p: Pick<Parts, "y" | "m" | "d">) => `${String(p.y).padStart(4, "0")}-${String(p.m).padStart(2, "0")}-${String(p.d).padStart(2, "0")}`;
/** `HH:MM` of the parts. */
export const timeText = (p: Pick<Parts, "h" | "mi">) => `${String(p.h).padStart(2, "0")}:${String(p.mi).padStart(2, "0")}`;

/** Parse `YYYY-MM-DD`. */
export function parseDate(s: string): { y: number; m: number; d: number } | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(s.trim());
  if (!m) return null;
  const y = +m[1]!,
    mo = +m[2]!,
    d = +m[3]!;
  const t = new Date(Date.UTC(y, mo - 1, d));
  return t.getUTCMonth() === mo - 1 && t.getUTCDate() === d ? { y, m: mo, d } : null;
}

/** Days since the epoch of a civil date. */
const dayNum = (p: { y: number; m: number; d: number }) => Math.floor(Date.UTC(p.y, p.m - 1, p.d) / DAY);
const fromDayNum = (n: number) => {
  const t = new Date(n * DAY);
  return { y: t.getUTCFullYear(), m: t.getUTCMonth() + 1, d: t.getUTCDate() };
};
/** Monday 0 .. Sunday 6. */
const weekday = (n: number) => (((n + 3) % 7) + 7) % 7;

/** Check a rule as the daemon does; null when good, else `field: message`. */
export function checkRule(r: CalendarRule): string | null {
  const interval = r.interval ?? 1;
  if (!Number.isInteger(interval) || interval < 1 || interval > 999) return "interval: use 1 to 999";
  if (r.weekdays?.length && r.freq !== "weekly") return "weekdays: only a weekly rule has weekdays";
  if (r.weekdays?.some((d) => !DAYS.includes(d))) return "weekdays: use mo, tu, we, th, fr, sa, or su";
  if (r.month_day !== undefined) {
    if (r.freq !== "monthly") return "month_day: only a monthly rule has a month day";
    if (!(r.month_day === -1 || (Number.isInteger(r.month_day) && r.month_day >= 1 && r.month_day <= 31))) return "month_day: use 1 to 31, or -1 for the last day";
  }
  if (r.until !== undefined && r.count !== undefined) return "until: give until or count, not both";
  if (r.count !== undefined && (!Number.isInteger(r.count) || r.count < 1 || r.count > 10000)) return "count: use 1 to 10000";
  if (r.until !== undefined && !parseDate(r.until) && Number.isNaN(Date.parse(r.until))) return "until: use a date (2026-12-31) or an RFC 3339 time";
  if (r.tz !== undefined && !validZone(r.tz)) return `tz: unknown time zone \`${r.tz}\``;
  return null;
}

/** Read RRULE text into a rule, or an error message. */
export function parseRrule(text: string): CalendarRule | string {
  const t = text.trim().replace(/^rrule:/i, "");
  const r: CalendarRule = { freq: "daily" };
  let freq: CalendarRule["freq"] | null = null;
  for (const part of t.split(";").map((x) => x.trim()).filter(Boolean)) {
    const [k0, v0] = part.split("=", 2);
    if (v0 === undefined) return `bad RRULE part \`${part}\`: use NAME=VALUE`;
    const k = k0!.trim().toUpperCase();
    const v = v0.trim();
    switch (k) {
      case "FREQ": {
        const f = v.toLowerCase();
        if (f !== "daily" && f !== "weekly" && f !== "monthly") return `FREQ=${v.toUpperCase()} is not supported: use DAILY, WEEKLY, or MONTHLY`;
        freq = f;
        break;
      }
      case "INTERVAL":
        r.interval = Number(v);
        break;
      case "COUNT":
        r.count = Number(v);
        break;
      case "UNTIL": {
        const d = /^(\d{4})(\d{2})(\d{2})(?:T(\d{2})(\d{2})(\d{2})Z)?$/.exec(v);
        if (!d) return `bad UNTIL \`${v}\``;
        r.until = d[4] ? new Date(Date.UTC(+d[1]!, +d[2]! - 1, +d[3]!, +d[4], +d[5]!, +d[6]!)).toISOString() : `${d[1]}-${d[2]}-${d[3]}`;
        break;
      }
      case "BYDAY":
        r.weekdays = [];
        for (const d of v.split(",").map((x) => x.trim().toLowerCase())) {
          if (d.length !== 2 || !DAYS.includes(d as CalendarDay)) return `BYDAY \`${d.toUpperCase()}\` is not supported: use two-letter days (MO,WE) with no number in front`;
          r.weekdays.push(d as CalendarDay);
        }
        break;
      case "BYMONTHDAY":
        if (v.includes(",")) return "BYMONTHDAY with more than one day is not supported";
        r.month_day = Number(v);
        break;
      case "WKST":
        if (v.toUpperCase() !== "MO") return `WKST=${v} is not supported: weeks start on MO`;
        break;
      default:
        return `RRULE part ${k} is not supported: use FREQ, INTERVAL, BYDAY (weekly), BYMONTHDAY (monthly), UNTIL, or COUNT`;
    }
  }
  if (!freq) return "the RRULE has no FREQ";
  r.freq = freq;
  const e = checkRule(r);
  if (e) return e;
  if (r.weekdays) r.weekdays = DAYS.filter((d) => r.weekdays!.includes(d));
  return r;
}

/** The rule as RRULE text. */
export function toRrule(r: CalendarRule): string {
  let s = `FREQ=${r.freq.toUpperCase()}`;
  if ((r.interval ?? 1) !== 1) s += `;INTERVAL=${r.interval}`;
  if (r.weekdays?.length) s += `;BYDAY=${r.weekdays.map((d) => d.toUpperCase()).join(",")}`;
  if (r.month_day !== undefined) s += `;BYMONTHDAY=${r.month_day}`;
  if (r.until) {
    const d = parseDate(r.until);
    s += d ? `;UNTIL=${r.until.replace(/-/g, "")}` : `;UNTIL=${new Date(r.until).toISOString().replace(/[-:]/g, "").replace(/\.\d{3}/, "")}`;
  }
  if (r.count !== undefined) s += `;COUNT=${r.count}`;
  return s;
}

/** The rule in a few words, as the daemon's `describe`. */
export function describeRule(r: CalendarRule): string {
  const unit = { daily: "day", weekly: "week", monthly: "month" }[r.freq];
  const n = r.interval ?? 1;
  let s = n === 1 ? `every ${unit}` : `every ${n} ${unit}s`;
  if (r.weekdays?.length) s += ` on ${r.weekdays.join(", ")}`;
  if (r.month_day === -1) s += " on the last day";
  else if (r.month_day !== undefined) s += ` on day ${r.month_day}`;
  if (r.until) s += ` until ${r.until}`;
  if (r.count !== undefined) s += `, ${r.count} times`;
  if (r.tz) s += ` (${r.tz})`;
  return s;
}

export interface Occ {
  start: number;
  end: number;
}

export interface SeriesShape {
  start: number;
  end: number;
  allDay: boolean;
  tz: string;
  rule: CalendarRule | null;
  skipped: Set<number>;
}

/** The occurrences that touch `[from, to)`, oldest first, skipped ones out. */
export function expand(s: SeriesShape, from: number, to: number, limit = 5000): Occ[] {
  const out: Occ[] = [];
  const touches = (o: Occ) => o.start < to && (o.end > from || o.start >= from);
  if (!s.rule) {
    const o = { start: s.start, end: s.end };
    return touches(o) && !s.skipped.has(o.start) ? [o] : [];
  }
  const r = s.rule;
  const tz = r.tz ?? s.tz;
  const l0 = zoned(s.start, tz);
  const d0 = dayNum(l0);
  const time = s.allDay ? { h: 0, mi: 0, s: 0 } : { h: l0.h, mi: l0.mi, s: l0.s };
  const ez = zoned(s.start, s.tz);
  const days = s.allDay ? Math.max(1, dayNum(zoned(s.end, s.tz)) - dayNum(ez)) : 0;
  const len = s.end - s.start;
  const untilDate = r.until ? parseDate(r.until) : null;
  const untilMs = r.until && !untilDate ? Date.parse(r.until) : null;
  const make = (n: number): Occ => {
    const c = fromDayNum(n);
    const start = localInstant({ ...c, ...time }, tz);
    const end = s.allDay ? localInstant({ ...fromDayNum(n + days), h: 0, mi: 0, s: 0 }, tz) : start + len;
    return { start, end };
  };
  const interval = r.interval ?? 1;
  const week0 = d0 - weekday(d0);
  const wds = r.weekdays?.length ? r.weekdays.map((d) => DAYS.indexOf(d)) : [weekday(d0)];
  let made = 0;
  for (let k = 0; k < 200_000; k++) {
    let dates: number[] = [];
    if (r.freq === "daily") dates = [d0 + k * interval];
    else if (r.freq === "weekly") dates = wds.map((w) => week0 + 7 * k * interval + w).filter((n) => n >= d0);
    else {
      const idx = l0.y * 12 + (l0.m - 1) + k * interval;
      const y = Math.floor(idx / 12),
        m = (idx % 12) + 1;
      const dim = new Date(Date.UTC(y, m, 0)).getUTCDate();
      const want = r.month_day ?? l0.d;
      const day = want === -1 ? dim : want;
      if (day <= dim) {
        const n = dayNum({ y, m, d: day });
        if (n >= d0) dates = [n];
      }
    }
    for (const n of dates) {
      if (untilDate && n > dayNum(untilDate)) return out;
      const o = make(n);
      if (untilMs !== null && o.start > untilMs) return out;
      made++;
      if (r.count !== undefined && made > r.count) return out;
      if (o.start >= to) return out;
      if (touches(o) && !s.skipped.has(o.start)) {
        if (out.length >= limit) return out;
        out.push(o);
      }
    }
  }
  return out;
}

/** Is `start` the start of an occurrence (skipped or not)? */
export function hasOccurrence(s: SeriesShape, start: number): boolean {
  return expand({ ...s, skipped: new Set() }, start, start + 1).some((o) => o.start === start);
}
