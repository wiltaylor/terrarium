// The mail log (docs/api.md §7): every message, newest first, with the
// attention rule that drives the log's default view, the sidebar count, and
// the mail items of the Home queue. Pure, testable.

import type { MailQuery, Message } from "../../api/types";

/** The direction filter of the log. A message can match more than one. */
export type MailKind = "to_operator" | "from_operator" | "agents" | "system" | "inbox";
export type TimeRange = "hour" | "day" | "week" | "all";

export interface LogFilter {
  /** A project name, or "" for every project. */
  project: string;
  kind: MailKind | "";
  /** Free text: subject, body, names, and IDs. */
  q: string;
  range: TimeRange;
  /** Only the messages that need the operator. On by default. */
  attention: boolean;
}

export const DEFAULT_FILTER: LogFilter = { project: "", kind: "", q: "", range: "all", attention: true };

export const KIND_LABELS: Record<MailKind, string> = {
  to_operator: "To operator",
  from_operator: "From operator",
  agents: "Agent ↔ agent",
  system: "System",
  inbox: "Project inbox",
};

export const RANGE_LABELS: Record<TimeRange, string> = { hour: "Last hour", day: "Last day", week: "Last week", all: "All time" };

/** What a `system` message is about, from its subject (crates/terrariumd: board.rs, quota_policy.rs, mail.rs). */
export type Notice = "blocked" | "quota" | "needs_human" | "login" | "notice";

/** Why a message needs the operator. */
export type Reason = "unread" | "held" | "paused" | "blocked" | "quota" | "needs_human";

export const REASON_LABELS: Record<Reason, string> = {
  unread: "unread by you",
  held: "held",
  paused: "thread paused",
  blocked: "blocked card",
  quota: "quota",
  needs_human: "needs a human",
};

export interface LogEntry {
  message: Message;
  /** The loop guard held it: delivered with no pointer to the agent. */
  held: boolean;
  notice: Notice | null;
  /** Empty when the message does not need the operator. */
  reasons: Reason[];
  /** The projects that the message touches, from the addresses (and the subject of a system notice). */
  projects: string[];
}

/** `queued` and `delivered`: the recipient has not read it. */
export const isUnread = (m: Pick<Message, "state">) => m.state === "queued" || m.state === "delivered";

export function matchesKind(m: Message, kind: MailKind | ""): boolean {
  switch (kind) {
    case "":
      return true;
    case "to_operator":
      return m.to_kind === "operator";
    case "from_operator":
      return m.from_kind === "operator";
    case "agents":
      return m.from_kind === "agent" && m.to_kind === "agent";
    case "system":
      return m.from_kind === "system";
    case "inbox":
      return m.from_kind === "project" || m.to_kind === "project";
  }
}

export function noticeOf(m: Message): Notice | null {
  if (m.from_kind !== "system") return null;
  if (/^Thread \S+ needs a human/.test(m.subject)) return "needs_human";
  if (/^\S+-\d+ blocked: /.test(m.subject)) return "blocked";
  if (/^(Quota|Budget):/.test(m.subject)) return "quota";
  if (/^Login expired/.test(m.subject)) return "login";
  return "notice";
}

/** The project of a readable address: `project:<name>` or `project/agent`. */
const projectOfName = (name: string): string | undefined => {
  if (name.startsWith("project:")) return name.slice(8) || undefined;
  const i = name.indexOf("/");
  return i > 0 ? name.slice(0, i) : undefined;
};

/** The projects of a message. `prefixes` maps card prefixes to projects (`{TER: "terrarium"}`), for blocked notices. */
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

/** A message from the operator or from or to a project inbox resets the loop guard (docs/api.md §6.2). */
const resetsGuard = (m: Message) => m.from_kind === "operator" || m.from_kind === "project" || m.to_kind === "project";
const agentToAgent = (m: Message) => m.from_kind === "agent" && m.to_kind === "agent";

/**
 * The messages that the loop guard held. A message row does not say it; the
 * `mail` event does (`held: true`), and `live` carries those IDs. For the
 * rest: the daemon sends its "needs a human" notice right after the first
 * held message, and holds every later agent-to-agent message on the thread
 * until the operator or a project inbox writes on it.
 */
export function heldIds(messages: Message[], live: ReadonlySet<string> = new Set()): Set<string> {
  const held = new Set<string>([...live].filter((id) => messages.some((m) => m.id === id)));
  const threads = new Map<string, Message[]>();
  for (const m of messages) threads.set(m.thread_id, [...(threads.get(m.thread_id) ?? []), m]);
  for (const msgs of threads.values()) {
    let holding = false;
    let lastAgent: Message | undefined;
    for (const m of msgs.slice().sort(byTime)) {
      if (resetsGuard(m)) {
        holding = false;
        lastAgent = undefined;
      } else if (agentToAgent(m)) {
        if (holding) held.add(m.id);
        lastAgent = m;
      } else if (noticeOf(m) === "needs_human") {
        if (lastAgent) held.add(lastAgent.id);
        holding = true;
      }
    }
  }
  return held;
}

export interface AnnotateOptions {
  prefixes?: Record<string, string>;
  /** Message IDs that a `mail` event flagged `held: true`. */
  live?: ReadonlySet<string>;
}

/**
 * The attention rule. A message needs the operator when:
 * - it goes to the operator, and the operator has not read it (`unread`);
 *   a system notice to the operator adds its kind (`blocked`, `quota`, `needs_human`);
 * - the loop guard held it, and its thread is still paused (`held`);
 * - it is the newest message of a paused thread (`paused`).
 * A reply from the operator ends the pause, and a read ends `unread`.
 */
export function attentionReasons(m: Message, held: boolean, notice: Notice | null, newestOfThread: boolean): Reason[] {
  const r: Reason[] = [];
  const unreadByOperator = m.to_kind === "operator" && isUnread(m);
  if (unreadByOperator) r.push("unread");
  if (unreadByOperator && (notice === "blocked" || notice === "quota" || notice === "needs_human")) r.push(notice);
  if (held && m.thread_paused) r.push("held");
  if (newestOfThread && m.thread_paused) r.push("paused");
  return r;
}

/** Annotate messages for the log, newest first. */
export function annotate(messages: Message[], opts: AnnotateOptions = {}): LogEntry[] {
  const held = heldIds(messages, opts.live);
  const newest = new Map<string, Message>();
  for (const m of messages) {
    const cur = newest.get(m.thread_id);
    if (!cur || byTime(cur, m) < 0) newest.set(m.thread_id, m);
  }
  return messages
    .slice()
    .sort(newestFirst)
    .map((m) => {
      const notice = noticeOf(m);
      const isHeld = held.has(m.id);
      return {
        message: m,
        held: isHeld,
        notice,
        reasons: attentionReasons(m, isHeld, notice, newest.get(m.thread_id)?.id === m.id),
        projects: projectsOf(m, opts.prefixes),
      };
    });
}

export const needsAttention = (e: Pick<LogEntry, "reasons">) => e.reasons.length > 0;

const RANGE_MS: Record<Exclude<TimeRange, "all">, number> = { hour: 3_600_000, day: 86_400_000, week: 7 * 86_400_000 };

/** The first instant in the range, or undefined for `all`. */
export const sinceOf = (range: TimeRange, now: number): number | undefined => (range === "all" ? undefined : now - RANGE_MS[range]);

/**
 * The part of the filter that `GET /api/mail` does itself (`to`, `from`).
 * The daemon has no project, text, time, or "involves a project inbox"
 * filter, so those run in the browser over the full list (`applyFilter`).
 * `from=system` is a 404 on the daemon ("no agent `system`": store/mail.rs
 * `resolve` does not know the address), so `system` runs in the browser too.
 */
export function serverQuery(f: LogFilter): MailQuery {
  switch (f.kind) {
    case "to_operator":
      return { to: "operator" };
    case "from_operator":
      return { from: "operator" };
    default:
      return {};
  }
}

/** Every filter, in the browser. It also repeats the server part, so extra rows (a live thread fetch) cannot leak in. */
export function applyFilter(entries: LogEntry[], f: LogFilter, now: number): LogEntry[] {
  const since = sinceOf(f.range, now);
  const q = f.q.trim().toLowerCase();
  return entries.filter((e) => {
    const m = e.message;
    if (f.attention && !needsAttention(e)) return false;
    if (!matchesKind(m, f.kind)) return false;
    if (f.project && !e.projects.includes(f.project)) return false;
    if (since !== undefined && Date.parse(m.created_at) < since) return false;
    if (q && ![m.subject, m.body, m.from_name, m.to_name, m.id, m.thread_id].some((s) => s.toLowerCase().includes(q))) return false;
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

/** The messages of one thread, oldest first. */
export const threadOf = (messages: Message[], threadId: string): Message[] => messages.filter((m) => m.thread_id === threadId).sort(byTime);

export const firstLine = (body: string): string => body.split("\n").find((l) => l.trim())?.trim() ?? "";

/** The other party of the operator on a message: whom a new message goes to. Undefined for `system` and operator-only rows. */
export function counterpart(m: Message): string | undefined {
  if (m.from_kind === "agent" || m.from_kind === "project") return m.from_name;
  if (m.to_kind === "agent" || m.to_kind === "project") return m.to_name;
  return undefined;
}

// ----- URL state -----

export type LogParams = Partial<Record<"project" | "kind" | "q" | "range" | "attention" | "thread", string>>;

const KINDS = Object.keys(KIND_LABELS) as MailKind[];
const RANGES = Object.keys(RANGE_LABELS) as TimeRange[];

/** Read the filter from the URL. A bad value falls back to the default. */
export function filterFromParams(p: LogParams): LogFilter {
  return {
    project: p.project ?? "",
    kind: KINDS.includes(p.kind as MailKind) ? (p.kind as MailKind) : "",
    q: p.q ?? "",
    range: RANGES.includes(p.range as TimeRange) ? (p.range as TimeRange) : DEFAULT_FILTER.range,
    attention: p.attention !== "0",
  };
}

/** The URL form of the filter. A default value is left out (undefined clears it). */
export function filterToParams(f: LogFilter): LogParams {
  return {
    project: f.project || undefined,
    kind: f.kind || undefined,
    q: f.q || undefined,
    range: f.range === DEFAULT_FILTER.range ? undefined : f.range,
    attention: f.attention ? undefined : "0",
  };
}
