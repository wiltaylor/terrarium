// A minimal 5-field cron parser and next-run calculator, for the schedule
// dialog's live preview (docs/api.md §19). The daemon has the last word: the
// dialog sends the expression even when this parser refuses it, and shows
// the daemon's `422` when the two disagree. Pure; no Solid imports.
//
// Fields: minute (0-59), hour (0-23), day of month (1-31), month (1-12 or
// JAN-DEC), day of week (0-7 or SUN-SAT; 0 and 7 are Sunday). Each field is
// a list of `*`, `n`, `a-b`, with an optional `/step`. The macros `@hourly`,
// `@daily` (`@midnight`), `@weekly`, `@monthly`, `@yearly` (`@annually`) work.
// As in Vixie cron, when both day fields are restricted (neither starts with
// `*`), a day matches either one; otherwise it must match both.

export interface Cron {
  expr: string;
  minute: boolean[];
  hour: boolean[];
  dom: boolean[];
  month: boolean[];
  /** 0-6, Sunday is 0. */
  dow: boolean[];
  domStar: boolean;
  dowStar: boolean;
}

export type CronParse = { ok: true; cron: Cron } | { ok: false; error: string };

const MACROS: Record<string, string> = {
  "@yearly": "0 0 1 1 *",
  "@annually": "0 0 1 1 *",
  "@monthly": "0 0 1 * *",
  "@weekly": "0 0 * * 0",
  "@daily": "0 0 * * *",
  "@midnight": "0 0 * * *",
  "@hourly": "0 * * * *",
};

const MONTHS = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];
export const DAYS = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

interface FieldDef {
  name: string;
  min: number;
  max: number;
  names?: string[];
  /** The value of the first name (JAN is 1, SUN is 0). */
  base?: number;
}

const FIELDS: FieldDef[] = [
  { name: "minute", min: 0, max: 59 },
  { name: "hour", min: 0, max: 23 },
  { name: "day of month", min: 1, max: 31 },
  { name: "month", min: 1, max: 12, names: MONTHS, base: 1 },
  { name: "day of week", min: 0, max: 7, names: DAYS, base: 0 },
];

/** Expand a macro; any other expression comes back trimmed, with single spaces. */
export function expandMacro(expr: string): string {
  const t = expr.trim().replace(/\s+/g, " ");
  return MACROS[t.toLowerCase()] ?? t;
}

function parseValue(raw: string, f: FieldDef): number | string {
  if (/^\d+$/.test(raw)) {
    const n = Number(raw);
    return n < f.min || n > f.max ? `${f.name}: \`${raw}\` is out of range ${f.min}-${f.max}` : n;
  }
  const i = f.names?.indexOf(raw.toUpperCase()) ?? -1;
  if (i >= 0) return i + (f.base ?? 0);
  return `${f.name}: \`${raw}\` is not a number${f.names ? " or a name" : ""}`;
}

/** One field as a table of allowed values, or the reason it is bad. */
function parseField(text: string, f: FieldDef): boolean[] | string {
  const table = new Array<boolean>(f.max + 1).fill(false);
  if (!text) return `${f.name} is empty`;
  for (const part of text.split(",")) {
    const m = /^([^/]+)(?:\/(.+))?$/.exec(part);
    if (!m) return `${f.name}: \`${part}\` is not valid`;
    const [, range, stepRaw] = m;
    let step = 1;
    if (stepRaw !== undefined) {
      if (!/^\d+$/.test(stepRaw) || Number(stepRaw) < 1) return `${f.name}: the step \`${stepRaw}\` is not a positive number`;
      step = Number(stepRaw);
    }
    let lo: number;
    let hi: number;
    if (range === "*") {
      lo = f.min;
      hi = f.max === 7 ? 6 : f.max;
    } else {
      const ends = range!.split("-");
      if (ends.length > 2 || ends.some((e) => !e)) return `${f.name}: \`${range}\` is not a value or a range`;
      const a = parseValue(ends[0]!, f);
      if (typeof a === "string") return a;
      lo = a;
      if (ends.length === 2) {
        const b = parseValue(ends[1]!, f);
        if (typeof b === "string") return b;
        hi = b;
        if (hi < lo) return `${f.name}: the range \`${range}\` goes backwards`;
      } else hi = stepRaw !== undefined ? (f.max === 7 ? 6 : f.max) : lo;
    }
    for (let v = lo; v <= hi; v += step) table[v] = true;
  }
  // Day of week: 7 is Sunday too.
  if (f.max === 7) {
    if (table[7]) table[0] = true;
    table.length = 7;
  }
  return table;
}

/** Parse an expression. The error names the field and quotes the bad part. */
export function parseCron(expr: string): CronParse {
  const raw = expr.trim();
  if (!raw) return { ok: false, error: "the expression is empty" };
  if (raw.startsWith("@") && !MACROS[raw.toLowerCase()]) return { ok: false, error: `\`${raw}\` is not a known macro (@hourly, @daily, @weekly, @monthly, @yearly)` };
  const text = expandMacro(raw);
  const parts = text.split(" ");
  if (parts.length !== 5) return { ok: false, error: `a cron expression has 5 fields (minute hour day-of-month month day-of-week); \`${raw}\` has ${parts.length}` };
  const tables: boolean[][] = [];
  for (let i = 0; i < 5; i++) {
    const t = parseField(parts[i]!, FIELDS[i]!);
    if (typeof t === "string") return { ok: false, error: t };
    tables.push(t);
  }
  const [minute, hour, dom, month, dow] = tables as [boolean[], boolean[], boolean[], boolean[], boolean[]];
  return { ok: true, cron: { expr: raw, minute, hour, dom, month, dow, domStar: parts[2]!.startsWith("*"), dowStar: parts[4]!.startsWith("*") } };
}

/** Does the day (wall clock, from a UTC-based Date) match the two day fields? */
function dayMatches(c: Cron, d: Date): boolean {
  const dom = c.dom[d.getUTCDate()] === true;
  const dow = c.dow[d.getUTCDay()] === true;
  if (c.domStar || c.dowStar) return dom && dow;
  return dom || dow;
}

const formatters = new Map<string, Intl.DateTimeFormat>();
function formatter(tz: string): Intl.DateTimeFormat {
  let f = formatters.get(tz);
  if (!f) {
    f = new Intl.DateTimeFormat("en-US", { timeZone: tz, hourCycle: "h23", year: "numeric", month: "numeric", day: "numeric", hour: "numeric", minute: "numeric" });
    formatters.set(tz, f);
  }
  return f;
}

/** Is `tz` an IANA zone that this browser knows? */
export function validTimeZone(tz: string): boolean {
  if (!tz.trim()) return false;
  try {
    formatter(tz);
    return true;
  } catch {
    return false;
  }
}

const isUtc = (tz: string) => tz === "UTC" || tz === "Etc/UTC" || tz === "Z";

/** The wall clock of `ms` in `tz`, as a UTC timestamp of the same fields, to the minute. */
export function wallOf(ms: number, tz: string): number {
  const floor = ms - (((ms % 60_000) + 60_000) % 60_000);
  if (isUtc(tz)) return floor;
  const p: Record<string, number> = {};
  for (const part of formatter(tz).formatToParts(new Date(ms))) if (part.type !== "literal") p[part.type] = Number(part.value);
  return Date.UTC(p.year!, p.month! - 1, p.day!, p.hour! % 24, p.minute!);
}

/** The instant of a wall clock time in `tz`, or null when the time does not exist (a DST gap). */
export function instantOf(wall: number, tz: string): number | null {
  if (isUtc(tz)) return wall;
  const off1 = wallOf(wall, tz) - wall;
  const guess = wall - off1;
  const off2 = wallOf(guess, tz) - guess;
  const at = wall - off2;
  return wallOf(at, tz) === wall ? at : null;
}

/**
 * The next `count` runs after `from` (ms), as instants (ms). Gives fewer
 * when the expression never fires within `years` (for example `0 0 30 2 *`).
 */
export function nextRuns(c: Cron, from: number, tz = "UTC", count = 3, years = 5): number[] {
  const out: number[] = [];
  let t = wallOf(from, tz) + 60_000;
  const limit = t + years * 366 * 86_400_000;
  while (out.length < count && t < limit) {
    const d = new Date(t);
    const y = d.getUTCFullYear();
    const mo = d.getUTCMonth();
    const day = d.getUTCDate();
    const h = d.getUTCHours();
    if (!c.month[mo + 1]) {
      t = Date.UTC(y, mo + 1, 1);
      continue;
    }
    if (!dayMatches(c, d)) {
      t = Date.UTC(y, mo, day + 1);
      continue;
    }
    if (!c.hour[h]) {
      t = Date.UTC(y, mo, day, h + 1);
      continue;
    }
    if (!c.minute[d.getUTCMinutes()]) {
      t += 60_000;
      continue;
    }
    const at = instantOf(t, tz);
    if (at !== null && at > from && at !== out.at(-1)) out.push(at);
    t += 60_000;
  }
  return out;
}

/** Parse and compute in one step, for the preview. */
export function previewCron(expr: string, tz: string, from: number, count = 3): { runs: number[] } | { error: string } {
  if (!validTimeZone(tz)) return { error: `\`${tz}\` is not a time zone` };
  const p = parseCron(expr);
  if (!p.ok) return { error: p.error };
  const runs = nextRuns(p.cron, from, tz, count);
  return runs.length ? { runs } : { error: `\`${expr}\` does not fire in the next 5 years` };
}
