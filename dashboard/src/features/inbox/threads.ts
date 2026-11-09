// Inbox threads (docs/api.md §6.13): pure helpers for the Threads screen
// (TER-466) and the chat page of one thread. No Solid imports.

import type { InboxCounts, NewThread, Question, Thread, ThreadClose, ThreadHandoff, ThreadLink, ThreadLinkKind, ThreadMessage, ThreadQuery } from "../../api/types";

/** The limits of the daemon (store/threads.rs). Texts count bytes; the topic and a link count characters. */
export const THREAD_TOPIC_MAX = 200;
export const THREAD_MESSAGE_MAX = 32 * 1024;
export const THREAD_RESPONSE_MAX = 16 * 1024;
export const THREAD_SUMMARY_MAX = 8 * 1024;
export const THREAD_LINKS_MAX = 50;
export const THREAD_REF_MAX = 500;
export const THREAD_LINK_KINDS: readonly ThreadLinkKind[] = ["card", "mail", "wiki", "memory", "url", "other"];

/** The chat page of a thread (TER-466: on the Threads screen; `/inbox/threads/:id` redirects here). */
export const threadHref = (id: string) => `/threads/${encodeURIComponent(id)}`;

/** The open questions of a thread: the `questions` of a full thread, else `open_questions` of a list row. */
export const openQuestionCount = (t: Pick<Thread, "questions" | "open_questions">): number => (t.questions ? t.questions.filter((q) => q.status === "open").length : (t.open_questions ?? 0));

/** Does the thread wait for the operator? It is `waiting_operator`, or it is not closed and has an open question (as the Inbox item). */
export const waitsOnYou = (t: Pick<Thread, "status" | "questions" | "open_questions">): boolean => t.status === "waiting_operator" || (t.status !== "closed" && openQuestionCount(t) > 0);

/** The agent's close proposal that waits for the operator (TER-95), or `null`: none, or the thread is closed. */
export const pendingProposal = (t: Pick<Thread, "status" | "close_proposal">): ThreadHandoff | null => (t.status === "closed" ? null : (t.close_proposal ?? null));

/** The status chip of a thread. */
export function threadChip(t: Pick<Thread, "status" | "questions" | "open_questions" | "close_proposal">): { label: string; chip: string } {
  if (t.status === "closed") return { label: "closed", chip: "" };
  if (pendingProposal(t)) return { label: "close proposed", chip: "warn" };
  if (waitsOnYou(t)) return { label: "waiting on you", chip: "warn" };
  if (t.status === "tracking") return { label: "tracking", chip: "" };
  return { label: "waiting on the agent", chip: "info" };
}

/**
 * The filters of the Threads screen (TER-466): Open has every thread that is
 * not closed, the tracking ones too; Archived has the closed ones.
 */
export type ThreadsViewId = "open" | "archived";

/** The filter of `?status=`: `archived`, else Open. */
export const threadsViewOf = (status: string | undefined): ThreadsViewId => (status === "archived" ? "archived" : "open");

/** The `status` of `GET /api/threads` for a filter. */
export const threadListStatus = (v: ThreadsViewId): NonNullable<ThreadQuery["status"]> => (v === "archived" ? "closed" : "open");

/** Does the thread show under the filter? */
export const inThreadsView = (v: ThreadsViewId, t: Pick<Thread, "status">): boolean => (v === "archived" ? t.status === "closed" : t.status !== "closed");

/** The status icon of a row (TER-466): a person waits on you, a robot on the agent, tracking for handed-off work, an archive box when closed. */
export type ThreadIconId = "you" | "agent" | "tracking" | "closed";

/** The icon of a thread and its tooltip. The tooltip of the person says why the thread waits on you. */
export function threadIcon(t: Pick<Thread, "status" | "questions" | "open_questions" | "close_proposal">): { icon: ThreadIconId; label: string } {
  if (t.status === "closed") return { icon: "closed", label: "Closed" };
  if (pendingProposal(t)) return { icon: "you", label: "Waiting on you: a close proposal" };
  const n = openQuestionCount(t);
  if (n > 0) return { icon: "you", label: `Waiting on you: ${n === 1 ? "an open question" : `${n} open questions`}` };
  if (t.status === "waiting_operator") return { icon: "you", label: "Waiting on you: your turn" };
  if (t.status === "tracking") return { icon: "tracking", label: "Tracking: the work is handed off" };
  return { icon: "agent", label: "Waiting on the agent" };
}

/**
 * The Threads badge of the sidebar (TER-466): `thread` of the counts, red when
 * one of those threads is urgent or critical, else the info style. No badge
 * when no thread waits on you.
 */
export function threadsBadge(c: Pick<InboxCounts, "thread" | "thread_urgent" | "thread_critical">): { n: number; cls: string; title: string } | null {
  const n = c.thread ?? 0;
  if (n <= 0) return null;
  const urgent = c.thread_urgent ?? 0;
  const critical = c.thread_critical ?? 0;
  return {
    n,
    cls: urgent > 0 || critical > 0 ? "nav-count" : "nav-count info",
    title: `${n} ${n === 1 ? "thread waits" : "threads wait"} on you${critical ? `. ${critical} critical` : ""}${urgent ? `. ${urgent} urgent` : ""}`,
  };
}

/** The newest activity of a thread: `last_at` of a list row, else the newest message or change. */
export const lastActivity = (t: Pick<Thread, "last_at" | "updated_at" | "messages">): string => t.last_at ?? [t.updated_at, ...(t.messages ?? []).map((m) => m.at)].sort().at(-1)!;

/** The ones that wait for the operator first, then the newest activity first (as the daemon's list). */
export const byThreadOrder = (a: Thread, b: Thread): number =>
  Number(waitsOnYou(b)) - Number(waitsOnYou(a)) || lastActivity(b).localeCompare(lastActivity(a)) || b.id.localeCompare(a.id);

export const sortThreads = (ts: Thread[]): Thread[] => ts.slice().sort(byThreadOrder);

/** The scope as the page shows it. */
export const scopeLabel = (scope: string): string => (scope === "global" ? "global" : `project ${scope}`);

/** One entry of the chat page: a message, or a question asked in the thread. */
export type TimelineEntry = { kind: "message"; at: string; id: string; message: ThreadMessage } | { kind: "question"; at: string; id: string; question: Question };

/**
 * The conversation in order: the messages and the questions by time. The
 * messages keep their own order (two writes in the same millisecond keep
 * the order of the daemon); a question goes after the messages of the same
 * time.
 */
export function timeline(t: Pick<Thread, "messages" | "questions">): TimelineEntry[] {
  const msgs: TimelineEntry[] = (t.messages ?? []).map((m) => ({ kind: "message", at: m.at, id: m.id, message: m }));
  const qs: TimelineEntry[] = (t.questions ?? []).map((q) => ({ kind: "question", at: q.created_at, id: q.id, question: q }));
  qs.sort((a, b) => a.at.localeCompare(b.at) || a.id.localeCompare(b.id));
  const out: TimelineEntry[] = [];
  let i = 0;
  for (const m of msgs) {
    while (i < qs.length && qs[i]!.at < m.at) out.push(qs[i++]!);
    out.push(m);
  }
  return out.concat(qs.slice(i));
}

const bytes = (s: string): number => new TextEncoder().encode(s).length;

/** The text of a message or a response, or what is wrong. */
export function threadText(text: string, max: number, what = "message"): { text: string } | { error: string } {
  const t = text.trim();
  if (!t) return { error: `Write the ${what}.` };
  const n = bytes(t);
  if (n > max) return { error: `The ${what} has ${n} bytes; at most ${max}.` };
  return { text: t };
}

/** The form of the New thread dialog. */
export interface NewThreadForm {
  template: string;
  scope: string;
  topic: string;
  text: string;
  agent: string;
}

/** The body of `POST /api/threads`, or what is wrong. An empty template, agent, or first message is left out. */
export function newThreadBody(f: NewThreadForm): { body: NewThread } | { error: string } {
  const topic = f.topic.trim();
  if (!topic) return { error: "Write the topic." };
  if (/[\r\n]/.test(topic)) return { error: "The topic is one line." };
  if ([...topic].length > THREAD_TOPIC_MAX) return { error: `The topic has ${[...topic].length} characters; at most ${THREAD_TOPIC_MAX}.` };
  const scope = f.scope.trim() || "global";
  const body: NewThread = { topic, scope };
  if (f.template.trim()) body.template = f.template.trim();
  if (f.agent.trim()) body.agent = f.agent.trim();
  if (f.text.trim()) {
    const t = threadText(f.text, THREAD_MESSAGE_MAX, "first message");
    if ("error" in t) return t;
    body.text = t.text;
  }
  return { body };
}

/**
 * The handoff of a close, or what is wrong. `links` is one link per line:
 * `<kind> <ref>`, with a kind of {@link THREAD_LINK_KINDS}; a line with no
 * known kind is a `url` when it starts with `http`, else `other`.
 */
export function closeBody(summary: string, links: string): { body: ThreadClose } | { error: string } {
  const body: ThreadClose = {};
  const s = summary.trim();
  if (s) {
    const n = bytes(s);
    if (n > THREAD_SUMMARY_MAX) return { error: `The summary has ${n} bytes; at most ${THREAD_SUMMARY_MAX}.` };
    body.summary = s;
  }
  const lines = links
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
  if (lines.length > THREAD_LINKS_MAX) return { error: `A handoff has at most ${THREAD_LINKS_MAX} links; you gave ${lines.length}.` };
  const parsed: ThreadLink[] = lines.map((l) => {
    const [head, ...rest] = l.split(/\s+/);
    const kind = THREAD_LINK_KINDS.find((k) => k === head?.toLowerCase());
    if (kind && rest.length) return { kind, ref: rest.join(" ") };
    return { kind: /^https?:\/\//i.test(l) ? "url" : "other", ref: l };
  });
  const long = parsed.findIndex((l) => [...l.ref].length > THREAD_REF_MAX);
  if (long >= 0) return { error: `Link ${long + 1} is longer than ${THREAD_REF_MAX} characters.` };
  if (parsed.length) body.links = parsed;
  return { body };
}

/** The name of an author or a `by`: `operator` is "you"; the attached agent is its name. */
export const whoName = (t: Pick<Thread, "agent" | "agent_name">, by: string | null): string => (by === "operator" ? "you" : by && by === t.agent ? (t.agent_name ?? by) : (by ?? "unknown"));

/** A URL link of a handoff is a link only for http and https. */
export const safeUrl = (l: ThreadLink): string | null => (l.kind === "url" && /^https?:\/\//i.test(l.ref) ? l.ref : null);

/** Is the author the operator? */
export const byOperator = (m: Pick<ThreadMessage, "author">): boolean => m.author === "operator";
