// The pure parts of the Calendar page (docs/api.md §33): the kinds and
// their colours, the month grid, the week and the agenda, and the dialog
// form with its mapping to the API body. The page shows times in the
// browser's zone; the dialog edits the times in the event's zone.

import type { CalendarDay, CalendarEvent, CalendarInput, CalendarKind, CalendarOccurrence, CalendarRule } from "../../api/types";
import { dateText, DAYS, parseDate, timeText, validZone, zoned } from "./recur";

export const KINDS: { kind: CalendarKind; label: string }[] = [
  { kind: "maintenance", label: "Maintenance" },
  { kind: "window", label: "Window" },
  { kind: "freeze", label: "Change freeze" },
  { kind: "deploy", label: "Deploy" },
  { kind: "task", label: "Task" },
  { kind: "other", label: "Other" },
];

export const kindLabel = (k: CalendarKind) => KINDS.find((x) => x.kind === k)?.label ?? k;

/** The CSS class of a kind's colour. */
export const kindClass = (k: CalendarKind) => `cal-kind-${k}`;

export const DAY_LABELS: Record<CalendarDay, string> = { mo: "Mon", tu: "Tue", we: "Wed", th: "Thu", fr: "Fri", sa: "Sat", su: "Sun" };

const DAY_MS = 86_400_000;

/** Local midnight of the day of `ms` (the browser's zone). */
export function dayStart(ms: number): number {
  const d = new Date(ms);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
}

/** `YYYY-MM-DD` of the local day of `ms`. */
export function dayKey(ms: number): string {
  const d = new Date(ms);
  return dateText({ y: d.getFullYear(), m: d.getMonth() + 1, d: d.getDate() });
}

const addDays = (ms: number, n: number) => {
  const d = new Date(ms);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() + n).getTime();
};

/** Monday of the local week of `ms`. */
export function weekStart(ms: number): number {
  const d = new Date(dayStart(ms));
  return addDays(d.getTime(), -((d.getDay() + 6) % 7));
}

/** Does the occurrence touch the local day that starts at `day`? */
export function onDay(o: Pick<CalendarOccurrence, "start" | "end">, day: number): boolean {
  const s = Date.parse(o.start);
  const e = Date.parse(o.end);
  const next = addDays(day, 1);
  return s < next && (e > day || (s === e && s >= day));
}

export interface GridDay {
  day: number;
  key: string;
  inMonth: boolean;
  today: boolean;
  items: CalendarOccurrence[];
}

/** The month of `ms` as whole weeks from Monday, each day with its occurrences. */
export function monthGrid(ms: number, occ: CalendarOccurrence[], now: number): GridDay[][] {
  const d = new Date(ms);
  const first = new Date(d.getFullYear(), d.getMonth(), 1).getTime();
  const month = d.getMonth();
  let day = weekStart(first);
  const weeks: GridDay[][] = [];
  const today = dayKey(now);
  do {
    const week: GridDay[] = [];
    for (let i = 0; i < 7; i++) {
      week.push({ day, key: dayKey(day), inMonth: new Date(day).getMonth() === month, today: dayKey(day) === today, items: occ.filter((o) => onDay(o, day)) });
      day = addDays(day, 1);
    }
    weeks.push(week);
  } while (new Date(day).getMonth() === month);
  return weeks;
}

/** The window that a month view loads: its whole weeks. */
export function monthRange(ms: number): { from: number; to: number } {
  const d = new Date(ms);
  const first = new Date(d.getFullYear(), d.getMonth(), 1).getTime();
  const grid = monthGrid(ms, [], 0);
  const last = grid[grid.length - 1]![6]!.day;
  return { from: weekStart(first), to: addDays(last, 1) };
}

/** The seven days of the week of `ms`, each with its occurrences. */
export function weekDays(ms: number, occ: CalendarOccurrence[], now: number): GridDay[] {
  const start = weekStart(ms);
  const today = dayKey(now);
  return Array.from({ length: 7 }, (_, i) => {
    const day = addDays(start, i);
    return { day, key: dayKey(day), inMonth: true, today: dayKey(day) === today, items: occ.filter((o) => onDay(o, day)) };
  });
}

export interface AgendaDay {
  key: string;
  day: number;
  items: CalendarOccurrence[];
}

/** The occurrences by the local day of their start (an ongoing one under the first day), days in order. */
export function agenda(occ: CalendarOccurrence[], from: number): AgendaDay[] {
  const by = new Map<string, AgendaDay>();
  const sorted = [...occ].sort((a, b) => a.start.localeCompare(b.start) || a.title.localeCompare(b.title));
  for (const o of sorted) {
    const day = Math.max(dayStart(Date.parse(o.start)), dayStart(from));
    const key = dayKey(day);
    let g = by.get(key);
    if (!g) by.set(key, (g = { key, day, items: [] }));
    g.items.push(o);
  }
  return [...by.values()].sort((a, b) => a.day - b.day);
}

const hm = (ms: number) => {
  const d = new Date(ms);
  return timeText({ h: d.getHours(), mi: d.getMinutes() });
};

/** The time of an occurrence in the browser's zone: `09:00–10:00`, `all day`, or a range of days. */
export function timeLabel(o: Pick<CalendarOccurrence, "start" | "end" | "all_day">): string {
  const s = Date.parse(o.start);
  const e = Date.parse(o.end);
  if (o.all_day) {
    const last = e - 1;
    return dayKey(s) === dayKey(last) ? "all day" : `all day, to ${dayKey(last)}`;
  }
  if (s === e) return hm(s);
  if (dayKey(s) === dayKey(e) || (e - s < DAY_MS && hm(e) === "00:00")) return `${hm(s)}–${hm(e)}`;
  return `${hm(s)} – ${dayKey(e)} ${hm(e)}`;
}

/** Is the occurrence running at `now`? */
export const isRunning = (o: Pick<CalendarOccurrence, "start" | "end">, now: number) => Date.parse(o.start) <= now && Date.parse(o.end) > now;

// --------------------------------------------------------------- the form

export type RepeatMode = "none" | "daily" | "weekly" | "monthly";
export type EndMode = "never" | "until" | "count";

export interface EventForm {
  title: string;
  kind: CalendarKind;
  description: string;
  allDay: boolean;
  /** Local to `tz`: `YYYY-MM-DD` and `HH:MM`. All-day: the first and the last day. */
  startDate: string;
  startTime: string;
  endDate: string;
  endTime: string;
  tz: string;
  repeat: RepeatMode;
  interval: string;
  weekdays: CalendarDay[];
  monthDay: string;
  endMode: EndMode;
  until: string;
  count: string;
  assignees: string[];
  card: string;
  target: string;
  repo: string;
  url: string;
}

/** The browser's zone, else UTC. */
export function browserZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  } catch {
    return "UTC";
  }
}

/** A new form: the next whole hour for one hour, or the day of `day` (a click on a month cell). */
export function emptyForm(now: number, day?: number): EventForm {
  const tz = browserZone();
  const base = day ?? now;
  const startMs = day !== undefined ? new Date(day).setHours(9, 0, 0, 0) : Math.ceil(base / 3_600_000) * 3_600_000;
  const s = zoned(startMs, tz);
  const e = zoned(startMs + 3_600_000, tz);
  return {
    title: "",
    kind: "maintenance",
    description: "",
    allDay: false,
    startDate: dateText(s),
    startTime: timeText(s),
    endDate: dateText(e),
    endTime: timeText(e),
    tz,
    repeat: "none",
    interval: "1",
    weekdays: [],
    monthDay: "",
    endMode: "never",
    until: "",
    count: "",
    assignees: [],
    card: "",
    target: "",
    repo: "",
    url: "",
  };
}

/** The form of an event: its first occurrence in its own zone. */
export function formOf(e: CalendarEvent): EventForm {
  const s = zoned(Date.parse(e.start), e.tz);
  const en = zoned(Date.parse(e.end), e.tz);
  const r = e.recurrence;
  return {
    title: e.title,
    kind: e.kind,
    description: e.description,
    allDay: e.all_day,
    startDate: e.all_day ? (e.start_date ?? dateText(s)) : dateText(s),
    startTime: e.all_day ? "09:00" : timeText(s),
    endDate: e.all_day ? (e.end_date ?? dateText(en)) : dateText(en),
    endTime: e.all_day ? "10:00" : timeText(en),
    tz: e.tz,
    repeat: r ? r.freq : "none",
    interval: String(r?.interval ?? 1),
    weekdays: r?.weekdays ?? [],
    monthDay: r?.month_day !== undefined ? String(r.month_day) : "",
    endMode: r?.until ? "until" : r?.count !== undefined ? "count" : "never",
    until: r?.until ?? "",
    count: r?.count !== undefined ? String(r.count) : "",
    assignees: e.assignees.map((a) => a.id),
    card: e.links.card ?? "",
    target: e.links.target ?? "",
    repo: e.links.repo ?? "",
    url: e.links.url ?? "",
  };
}

/** The rule of the form, or null. */
export function ruleOf(f: EventForm): CalendarRule | null {
  if (f.repeat === "none") return null;
  const r: CalendarRule = { freq: f.repeat };
  const n = Number(f.interval);
  if (f.interval.trim() && n !== 1) r.interval = n;
  if (f.repeat === "weekly" && f.weekdays.length) r.weekdays = DAYS.filter((d) => f.weekdays.includes(d));
  if (f.repeat === "monthly" && f.monthDay.trim()) r.month_day = Number(f.monthDay);
  if (f.endMode === "until" && f.until.trim()) r.until = f.until.trim();
  if (f.endMode === "count" && f.count.trim()) r.count = Number(f.count);
  return r;
}

/** What is wrong with the form, or null: the checks a person can fix before a save. */
export function formError(f: EventForm): string | null {
  if (!f.title.trim()) return "Give a title.";
  if (!validZone(f.tz)) return `Unknown time zone ${f.tz}.`;
  if (!parseDate(f.startDate)) return "Give a start date.";
  if (!parseDate(f.endDate)) return f.allDay ? "Give the last day." : "Give an end date.";
  if (f.allDay) {
    if (f.endDate < f.startDate) return "The last day is before the first day.";
  } else {
    if (!/^\d{2}:\d{2}$/.test(f.startTime) || !/^\d{2}:\d{2}$/.test(f.endTime)) return "Give the times as HH:MM.";
    if (`${f.endDate}T${f.endTime}` < `${f.startDate}T${f.startTime}`) return "The end is before the start.";
  }
  if (f.repeat !== "none") {
    const n = Number(f.interval);
    if (!Number.isInteger(n) || n < 1 || n > 999) return "Repeat every 1 to 999.";
    if (f.repeat === "monthly" && f.monthDay.trim()) {
      const d = Number(f.monthDay);
      if (!(d === -1 || (Number.isInteger(d) && d >= 1 && d <= 31))) return "The day of the month is 1 to 31, or -1 for the last day.";
    }
    if (f.endMode === "until" && !parseDate(f.until)) return "Give the last date of the repeat.";
    if (f.endMode === "count") {
      const c = Number(f.count);
      if (!Number.isInteger(c) || c < 1 || c > 10000) return "Repeat 1 to 10000 times.";
    }
  }
  if (f.url.trim() && !/^https?:\/\//i.test(f.url.trim())) return "The link URL starts with http:// or https://.";
  return null;
}

/** The API body of the form. A new event leaves out the empty fields; an edit sends `null` to clear them. */
export function inputOf(f: EventForm, edit = false): CalendarInput {
  const links: Record<string, string> = {};
  for (const k of ["card", "target", "repo", "url"] as const) if (f[k].trim()) links[k] = f[k].trim();
  const body: CalendarInput = {
    title: f.title.trim(),
    kind: f.kind,
    description: f.description,
    all_day: f.allDay,
    tz: f.tz,
    start: f.allDay ? f.startDate : `${f.startDate}T${f.startTime}`,
    end: f.allDay ? f.endDate : `${f.endDate}T${f.endTime}`,
    assignees: f.assignees,
  };
  const rule = ruleOf(f);
  if (rule) body.recurrence = rule;
  else if (edit) body.recurrence = null;
  if (Object.keys(links).length) body.links = links;
  else if (edit) body.links = null;
  return body;
}

/** The scope of an event for display: `global` or its project. */
export const scopeLabel = (o: Pick<CalendarOccurrence, "global" | "project">) => (o.global ? "global" : (o.project ?? ""));
