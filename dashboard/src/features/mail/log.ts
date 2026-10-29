// The mail log (docs/api.md §7): every message, newest first, with filters.
// The log is a record only: it keeps no unread state for the operator. Mail
// that needs the operator goes to the Inbox (features/inbox). Pure, testable.

import type { MailQuery, Message } from "../../api/types";

export type TimeRange = "hour" | "day" | "week" | "all" | "custom";

export interface LogFilter {
  /** Part of the sender's name or address. */
  from: string;
  /** Part of the recipient's name or address. */
  to: string;
  /** A project name, or "" for every project. */
  project: string;
  /** A thread ID, or "" for every thread. */
  thread: string;
  /** Free text: subject, body, names, and IDs. */
  q: string;
  range: TimeRange;
  /** The custom range: `datetime-local` values, or "". */
  since: string;
  until: string;
}

export const DEFAULT_FILTER: LogFilter = { from: "", to: "", project: "", thread: "", q: "", range: "all", since: "", until: "" };

export const RANGE_LABELS: Record<TimeRange, string> = { hour: "Last hour", day: "Last day", week: "Last week", all: "All time", custom: "From – to" };

/** The project of a readable address: `project:<name>` or `project/agent`. */
const projectOfName = (name: string): string | undefined => {
  if (name.startsWith("project:")) return name.slice(8) || undefined;
  const i = name.indexOf("/");
  return i > 0 ? name.slice(0, i) : undefined;
};

/** The projects of a message. `prefixes` maps card prefixes to projects (`{TER: "terrarium"}`), for blocked-card notices. */
export function projectsOf(m: Message, prefixes: Record<string, string> = {}): string[] {
  const out = new Set<string>();
  for (const n of [m.to_name, m.from_name]) {
    const p = projectOfName(n);
    if (p) out.add(p);
  }
  if (m.from_kind === "system") {
    const card = /^(\S+)-\d+ blocked: /.exec(m.subject);
    if (card && prefixes[card[1]!]) out.add(prefixes[card[1]!]!);
    const budget = /^Budget: project:([^/\s]+)/.exec(m.subject);
    if (budget) out.add(budget[1]!);
  }
  return [...out];
}

/** Oldest first; a system notice sorts after the agent message of the same instant that caused it. */
const byTime = (a: Message, b: Message) =>
  a.created_at.localeCompare(b.created_at) || Number(a.from_kind === "system") - Number(b.from_kind === "system") || a.id.localeCompare(b.id);

/** Newest first. */
export const newestFirst = (a: Message, b: Message) => b.created_at.localeCompare(a.created_at) || b.id.localeCompare(a.id);

const RANGE_MS: Record<"hour" | "day" | "week", number> = { hour: 3_600_000, day: 86_400_000, week: 7 * 86_400_000 };

/** The time window of the filter in ms, each end optional. A custom value that does not parse is no limit. */
export function windowOf(f: Pick<LogFilter, "range" | "since" | "until">, now: number): { since?: number; until?: number } {
  if (f.range === "all") return {};
  if (f.range !== "custom") return { since: now - RANGE_MS[f.range] };
  const at = (v: string) => {
    const t = v ? Date.parse(v) : NaN;
    return Number.isNaN(t) ? undefined : t;
  };
  return { since: at(f.since), until: at(f.until) };
}

/**
 * The part of the filter that `GET /api/mail` does: a whole thread ID (a
 * ULID), so a half-typed ID does not ask the daemon at each key. The other
 * filters run in the browser (`applyFilter`): `from` and `to` match part of
 * a name there, and the daemon's `from` and `to` take a whole address only.
 */
export function serverQuery(f: LogFilter): MailQuery {
  return /^[0-9A-Z]{26}$/i.test(f.thread) ? { thread: f.thread } : {};
}

const has = (hay: string, needle: string) => hay.toLowerCase().includes(needle);

/**
 * Every filter, in the browser. `prefixes` names the project of a blocked-card
 * notice. It also repeats the server part, so extra rows (a live thread fetch)
 * cannot leak in.
 */
export function applyFilter(messages: Message[], f: LogFilter, now: number, prefixes: Record<string, string> = {}): Message[] {
  const { since, until } = windowOf(f, now);
  const q = f.q.trim().toLowerCase();
  const from = f.from.trim().toLowerCase();
  const to = f.to.trim().toLowerCase();
  return messages.filter((m) => {
    if (f.thread && m.thread_id !== f.thread) return false;
    if (from && !has(m.from_name, from) && !has(m.from, from)) return false;
    if (to && !has(m.to_name, to) && !has(m.to, to)) return false;
    if (f.project && !projectsOf(m, prefixes).includes(f.project)) return false;
    const t = Date.parse(m.created_at);
    if (since !== undefined && t < since) return false;
    if (until !== undefined && t > until) return false;
    if (q && ![m.subject, m.body, m.from_name, m.to_name, m.id, m.thread_id].some((s) => has(s, q))) return false;
    return true;
  });
}

/** Merge new or changed messages into a list. The newer copy of a message wins. Newest first. */
export function mergeMessages(current: Message[], incoming: Message[]): Message[] {
  const by = new Map(current.map((m) => [m.id, m]));
  for (const m of incoming) by.set(m.id, m);
  return [...by.values()].sort(newestFirst);
}

/** The biggest message ID: the `cursor` that asks the daemon for later messages only (IDs are ULIDs, the list is `id > cursor`). */
export const newestId = (messages: Message[]): string | undefined => messages.reduce<string | undefined>((a, m) => (!a || m.id > a ? m.id : a), undefined);

/**
 * The open row of the log. `open` is a message ID (a click on a row) or a
 * thread ID (a link from the Inbox). A message ID opens that row, also when
 * the thread has newer rows: mail to the operator usually has a later reply
 * from the operator. A thread ID opens the newest shown row of the thread.
 */
export function openRowOf(rows: Message[], open: string | undefined): Message | undefined {
  if (!open) return undefined;
  return rows.find((m) => m.id === open) ?? rows.find((m) => m.thread_id === open);
}

/** The thread of `open` (a message ID or a thread ID), to load in full. */
export function openThreadOf(messages: Message[], open: string | undefined): string | undefined {
  if (!open) return undefined;
  return messages.find((m) => m.id === open)?.thread_id ?? open;
}

/** The messages of one thread, oldest first. */
export const threadOf = (messages: Message[], threadId: string): Message[] => messages.filter((m) => m.thread_id === threadId).sort(byTime);

export const firstLine = (body: string): string => body.split("\n").find((l) => l.trim())?.trim() ?? "";

/** The short form of a thread ID for a table cell: its last 6 characters. */
export const shortThread = (id: string): string => (id.length > 8 ? `…${id.slice(-6)}` : id);

/** The other party of the operator on a message: whom a new message goes to. Undefined for `system` and operator-only rows. */
export function counterpart(m: Message): string | undefined {
  if (m.from_kind === "agent" || m.from_kind === "project") return m.from_name;
  if (m.to_kind === "agent" || m.to_kind === "project") return m.to_name;
  return undefined;
}

/** The names in a list, for the `from` and `to` suggestions. */
export const namesOf = (messages: Message[]): string[] => [...new Set(messages.flatMap((m) => [m.from_name, m.to_name]))].sort();

// ----- live tail -----

/**
 * Live tail: on, a `mail` event pulls the new messages at once. Off, the
 * log stays as it is, and counts the events, so the page can say "N new".
 */
export interface TailState {
  live: boolean;
  /** `mail` events while the tail was off. */
  missed: number;
  /** Threads that those events touched, to pull when the tail goes on again. */
  threads: string[];
}

export const TAIL_START: TailState = { live: true, missed: 0, threads: [] };

/** A `mail` event: pull now (live), or count it. */
export function tailEvent(s: TailState, thread: string | undefined): { state: TailState; pull: boolean } {
  if (s.live) return { state: s, pull: true };
  return { state: { ...s, missed: s.missed + 1, threads: thread && !s.threads.includes(thread) ? [...s.threads, thread] : s.threads }, pull: false };
}

/** Turn the tail on or off. On again: pull the threads that the missed events touched. */
export function setTail(s: TailState, live: boolean): { state: TailState; pull: string[] | null } {
  if (live === s.live) return { state: s, pull: null };
  if (!live) return { state: { live: false, missed: 0, threads: [] }, pull: null };
  return { state: TAIL_START, pull: s.threads };
}

// ----- URL state -----

export type LogParams = Partial<Record<"from" | "to" | "project" | "thread" | "q" | "range" | "since" | "until" | "open", string>>;

const RANGES = Object.keys(RANGE_LABELS) as TimeRange[];

/** Read the filter from the URL. A bad value falls back to the default. */
export function filterFromParams(p: LogParams): LogFilter {
  const range = RANGES.includes(p.range as TimeRange) ? (p.range as TimeRange) : DEFAULT_FILTER.range;
  return {
    from: p.from ?? "",
    to: p.to ?? "",
    project: p.project ?? "",
    thread: p.thread ?? "",
    q: p.q ?? "",
    range,
    since: range === "custom" ? (p.since ?? "") : "",
    until: range === "custom" ? (p.until ?? "") : "",
  };
}

/** The URL form of the filter. A default value is left out (undefined clears it). */
export function filterToParams(f: LogFilter): LogParams {
  const custom = f.range === "custom";
  return {
    from: f.from || undefined,
    to: f.to || undefined,
    project: f.project || undefined,
    thread: f.thread || undefined,
    q: f.q || undefined,
    range: f.range === DEFAULT_FILTER.range ? undefined : f.range,
    since: (custom && f.since) || undefined,
    until: (custom && f.until) || undefined,
  };
}
