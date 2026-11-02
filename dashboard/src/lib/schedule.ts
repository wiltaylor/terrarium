// Schedules in words (docs/api.md §19): the spec ("every 15 min", "daily
// 09:00 UTC", "once at ..."), the action ("mail → lead: Standup"), the next
// run, and the command line of an exec action. Pure; no Solid imports.

import { DAYS, expandMacro, parseCron } from "./cron";
import { duration } from "./format";
import type { MatchRule, Schedule, ScheduleAction, ScheduleSpec, TriggerSpec } from "../api/types";

const pad = (n: number) => String(n).padStart(2, "0");

/**
 * An interval as seconds: `15m`, `1h30m`, `90s`, `2d`, `15 min`, `1 h`, or a
 * plain number of seconds. Null when it does not read.
 */
export function parseEvery(raw: string | number): number | null {
  if (typeof raw === "number") return Number.isFinite(raw) && raw > 0 ? raw : null;
  const s = raw.trim().toLowerCase();
  if (!s) return null;
  if (/^\d+$/.test(s)) return Number(s) || null;
  const unit: Record<string, number> = { d: 86400, day: 86400, days: 86400, h: 3600, hr: 3600, hour: 3600, hours: 3600, m: 60, min: 60, mins: 60, minute: 60, minutes: 60, s: 1, sec: 1, secs: 1, second: 1, seconds: 1 };
  const re = /(\d+)\s*([a-z]+)\s*/gy;
  let total = 0;
  let m: RegExpExecArray | null;
  let end = 0;
  while ((m = re.exec(s))) {
    const u = unit[m[2]!];
    if (u === undefined) return null;
    total += Number(m[1]) * u;
    end = re.lastIndex;
  }
  return end === s.length && total > 0 ? total : null;
}

/** Seconds in words: "minute", "15 min", "hour", "2 h", "1 h 30 min", "day", "2 days", "30 s". */
export function everyWords(sec: number): string {
  if (sec % 86400 === 0) return sec === 86400 ? "day" : `${sec / 86400} days`;
  if (sec % 3600 === 0) return sec === 3600 ? "hour" : `${sec / 3600} h`;
  if (sec % 60 === 0) {
    const min = sec / 60;
    if (min === 1) return "minute";
    if (min < 60) return `${min} min`;
    return `${Math.floor(min / 60)} h ${min % 60} min`;
  }
  if (sec < 60) return `${sec} s`;
  return `${Math.floor(sec / 60)} min ${sec % 60} s`;
}

/** An instant as `YYYY-MM-DD HH:MM UTC`. */
export function utcText(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())} UTC`;
}

const DAY_WORDS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_WORDS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

const nums = (field: string, max: number): number[] | null => {
  if (!/^\d+(,\d+)*$/.test(field)) return null;
  const list = field.split(",").map(Number);
  return list.every((n) => n <= max) ? list : null;
};

/** The days of a day-of-week table in words: "weekdays", "weekends", "Mon", "Mon, Wed, Fri". */
function daysWords(days: boolean[]): { words: string; one: boolean } {
  const on = days.map((v, i) => (v ? i : -1)).filter((i) => i >= 0);
  const key = on.join(",");
  if (key === "1,2,3,4,5") return { words: "weekdays", one: false };
  if (key === "0,6") return { words: "weekends", one: false };
  // Monday first reads better: Sunday goes last.
  const sorted = [...on.filter((i) => i !== 0), ...on.filter((i) => i === 0)];
  return { words: sorted.map((i) => DAY_WORDS[i]).join(", "), one: on.length === 1 };
}

/** A cron expression in words, or `cron <expr>` when it has no short form. */
export function cronWords(expr: string, tz = "UTC"): string {
  const zone = tz || "UTC";
  const fallback = `cron ${expr.trim()} (${zone})`;
  const parsed = parseCron(expr);
  if (!parsed.ok) return `cron ${expr.trim()} (invalid)`;
  const [mi, h, dom, mon, dow] = expandMacro(expr).split(" ") as [string, string, string, string, string];
  const restStar = dom === "*" && mon === "*" && dow === "*";
  if (restStar && h === "*") {
    if (mi === "*") return "every minute";
    const step = /^\*\/(\d+)$/.exec(mi);
    if (step) return step[1] === "1" ? "every minute" : `every ${step[1]} min`;
    if (/^\d+$/.test(mi)) return mi === "0" ? "hourly" : `hourly at :${pad(Number(mi))}`;
    return fallback;
  }
  if (!/^\d+$/.test(mi)) return fallback;
  const minute = Number(mi);
  const hStep = /^\*\/(\d+)$/.exec(h);
  if (hStep && restStar) return `every ${hStep[1] === "1" ? "hour" : `${hStep[1]} h`}${minute ? ` at :${pad(minute)}` : ""}`;
  const hours = nums(h, 23);
  if (!hours) return fallback;
  const times = `${hours.map((x) => `${pad(x)}:${pad(minute)}`).join(", ")} ${zone}`;
  if (restStar) return `daily ${times}`;
  if (mon === "*" && dom === "*") {
    const d = daysWords(parsed.cron.dow);
    return d.one ? `weekly ${d.words} ${times}` : `${d.words} ${times}`;
  }
  const days = nums(dom, 31);
  if (days && dow === "*") {
    if (mon === "*") return `monthly on day ${days.join(", ")} ${times}`;
    const months = nums(mon, 12);
    if (months?.length === 1 && days.length === 1) return `yearly on ${MONTH_WORDS[months[0]! - 1]} ${days[0]} ${times}`;
  }
  return fallback;
}

/** The spec in words: "every 15 min", "daily 09:00 UTC", "weekly Mon 09:00 UTC", "once at 2026-11-01 09:00 UTC". */
export function specWords(spec: ScheduleSpec): string {
  switch (spec.kind) {
    case "interval": {
      const sec = parseEvery(spec.every);
      return sec === null ? `every ${spec.every}` : `every ${everyWords(sec)}`;
    }
    case "at":
      return `once at ${utcText(spec.at)}`;
    case "cron":
      return cronWords(spec.expr, spec.tz);
    case "on":
      return `on ${spec.kinds.join(", ")}`;
  }
}

/** One match rule in words: "data.branch = main", "data.repo in a, b", "kind ~ ci.*". */
export function matchWords(m: MatchRule): string {
  const v = Array.isArray(m.value) ? m.value.map(String).join(", ") : String(m.value);
  const op = { eq: "=", ne: "≠", glob: "~", in: "in" }[m.op ?? "eq"];
  return `${m.path} ${op} ${v}`;
}

/** The match rules of a trigger in words, joined with "and"; "" when it has none. */
export const matchesWords = (spec: Pick<TriggerSpec, "match">): string => (spec.match ?? []).map(matchWords).join(" and ");

/** The guards of a trigger in words: "dedupe 10m · escalate after 3 → operator · max 30/h". */
export function guardWords(spec: TriggerSpec): string {
  const parts: string[] = [];
  if (spec.dedupe) parts.push(`dedupe ${spec.dedupe.window}`);
  if (spec.escalate) parts.push(`escalate after ${spec.escalate.after} → ${spec.escalate.to ?? "operator"}`);
  parts.push(`max ${spec.max_per_hour ?? 30}/h`);
  return parts.join(" · ");
}

/**
 * An address in short form, from the schedule's project: `operator`;
 * `project:<name>` is "<name> inbox"; `<project>/<agent>` of the same
 * project is the agent name; any other stays as it is.
 */
export function shortAddress(addr: string, project: string | null): string {
  if (addr.startsWith("project:")) return `${addr.slice("project:".length)} inbox`;
  if (project && addr.startsWith(`${project}/`)) return addr.slice(project.length + 1);
  return addr;
}

/** Quote an argument for display when it holds spaces or quotes. */
const quoteArg = (a: string) => (a === "" || /[\s'"\\]/.test(a) ? `'${a.replace(/'/g, `'\\''`)}'` : a);

/** An argv as one line. */
export const commandLine = (cmd: readonly string[]): string => cmd.map(quoteArg).join(" ");

/**
 * Split a command line into an argv: spaces split, `'...'` and `"..."`
 * group, a backslash escapes the next character (not inside single quotes).
 * No variables, globs, or pipes: the daemon runs the argv as it is.
 */
export function splitCommand(line: string): { argv: string[] } | { error: string } {
  const argv: string[] = [];
  let cur = "";
  let has = false;
  let quote: "'" | '"' | null = null;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i]!;
    if (quote === "'") {
      if (ch === "'") quote = null;
      else cur += ch;
    } else if (ch === "\\") {
      if (i + 1 >= line.length) return { error: "the line ends with a backslash" };
      cur += line[++i];
      has = true;
    } else if (quote === '"') {
      if (ch === '"') quote = null;
      else cur += ch;
    } else if (ch === "'" || ch === '"') {
      quote = ch;
      has = true;
    } else if (/\s/.test(ch)) {
      if (has) argv.push(cur);
      cur = "";
      has = false;
    } else {
      cur += ch;
      has = true;
    }
  }
  if (quote) return { error: `a ${quote === "'" ? "single" : "double"} quote is not closed` };
  if (has) argv.push(cur);
  return { argv };
}

/** The action in words: "mail → lead: Standup", "turn → w1", "exec on svc-dev: make test", "restart → w1", "probe anthropic-max". */
export function actionWords(action: ScheduleAction, project: string | null): string {
  switch (action.type) {
    case "mail":
      return `mail → ${shortAddress(action.to, project)}: ${action.subject || "(no subject)"}`;
    case "turn":
      return `turn → ${shortAddress(action.agent, project)}`;
    case "exec":
      return `exec on ${action.target}: ${commandLine(action.cmd)}`;
    case "agent":
      return `${action.verb} → ${shortAddress(action.agent, project)}`;
    case "probe":
      return `probe ${action.subscription}`;
    case "card":
      return `card ${action.op} ${action.key}${action.title ? `: ${action.title}` : ""}`;
    case "webhook":
      return `webhook ${action.method ?? "POST"} ${action.url_secret ? `(secret ${action.url_secret})` : webhookHost(action.url ?? "")} · ${action.format ?? "json"}`;
  }
}

/** The host of a webhook URL (no path: a path can hold a token), or the text when it does not parse. */
export function webhookHost(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

/** The next run, relative: "in 14m", "due now", or why there is none. */
export function nextRunWords(s: Pick<Schedule, "enabled" | "next_run_at" | "spec" | "last_run">, now = Date.now()): string {
  if (!s.enabled) return "disabled";
  if (s.spec.kind === "on") return "on each match";
  if (!s.next_run_at) return s.spec.kind === "at" && s.last_run ? "done (ran once)" : "not planned";
  const ms = Date.parse(s.next_run_at) - now;
  return ms <= 0 ? "due now" : `in ${duration(ms)}`;
}

/** The days of the week, Monday first, for the weekly preset. */
export const WEEKDAYS: readonly { value: number; label: string }[] = [1, 2, 3, 4, 5, 6, 0].map((i) => ({ value: i, label: DAYS[i]![0] + DAYS[i]!.slice(1).toLowerCase() }));
