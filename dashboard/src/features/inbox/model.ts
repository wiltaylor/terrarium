// The operator's inbox: everything that waits for the operator (questions,
// notices, mail to `operator`, and paused threads). Pure helpers, no Solid
// imports. The daemon filters and sorts (`GET /api/inbox`); these helpers
// keep a list in the same order after a local change, and give the tabs and
// the badge.

import type { AgentEvent, InboxCounts, InboxItem, InboxKind, InboxStatus, InboxWatch, Message, Question, QuestionPriority } from "../../api/types";

/** The kind tabs of the Inbox page. `all` sends no `kind`. */
export const INBOX_TABS = [
  { id: "all", label: "All" },
  { id: "question", label: "Questions" },
  { id: "notice", label: "Notices" },
  { id: "mail", label: "Mail to you" },
  { id: "system", label: "System" },
] as const;
/**
 * A kind tab. `watch` is no tab (the Asked status tab lists the waiting asks), but an older link may name it.
 * The threads have their own screen (TER-466): `?tab=thread` redirects to `/threads` (nav.ts `inboxRedirect`).
 */
export type InboxTab = (typeof INBOX_TABS)[number]["id"] | "watch";

/**
 * The status tabs. System items show only while they are open. Asked
 * (TER-42) lists the asks and forwards that wait for an agent (the open
 * watches); Open never shows them: the agent's reply shows on Open, as a
 * mail item. An answered or cancelled ask is under Done.
 */
export const STATUS_TABS = [
  { id: "open", label: "Open" },
  { id: "done", label: "Done" },
  { id: "asked", label: "Asked" },
] as const;
export type InboxStatusTab = (typeof STATUS_TABS)[number]["id"];

/** The status tab of `?status=`, and of an older `?tab=watch` link (the Asked kind tab of 0.1.0). */
export function statusTabOf(status: string | undefined, tab: string | undefined): InboxStatusTab {
  if (status === "done") return "done";
  if (status === "asked" || tab === "watch") return "asked";
  return "open";
}

export const EMPTY_COUNTS: InboxCounts = { open: 0, urgent: 0, critical: 0, question: 0, notice: 0, mail: 0, system: 0, watch: 0, thread: 0, thread_urgent: 0, thread_critical: 0 };

/** The query of a tab pair, for `GET /api/inbox`. */
export function inboxQuery(tab: InboxTab, status: InboxStatusTab, project: string): { status: InboxStatus; kind?: InboxKind; project?: string } {
  const p = project ? { project } : {};
  // Asked: the open watches, whatever the kind tab. The daemon's `open`
  // includes them; Open filters them out (`matchesTabs`).
  if (status === "asked") return { status: "open", kind: "watch", ...p };
  return { status, ...(tab === "all" ? {} : { kind: tab }), ...p };
}

/** The rank of a priority: critical 2, urgent 1, normal 0. */
export const priorityRank = (p: QuestionPriority | undefined): number => (p === "critical" ? 2 : p === "urgent" ? 1 : 0);

/** Is the item an open critical one? An older daemon sends no `critical`: then the question's priority tells. */
export const isCritical = (i: InboxItem): boolean => i.open && (i.critical ?? i.question?.priority === "critical");

/** The `urgent` and `critical` flags of a question item, as the daemon sets them. */
export const questionFlags = (q: Pick<Question, "status" | "priority">): { open: boolean; urgent: boolean; critical: boolean } => {
  const open = q.status === "open";
  return { open, urgent: open && priorityRank(q.priority) > 0, critical: open && q.priority === "critical" };
};

/** Critical first, then urgent, then the newest (`at`), as the daemon. */
export const byInboxOrder = (a: InboxItem, b: InboxItem): number =>
  Number(isCritical(b)) - Number(isCritical(a)) || Number(b.urgent) - Number(a.urgent) || b.at.localeCompare(a.at) || b.id.localeCompare(a.id);

export const sortItems = (items: InboxItem[]): InboxItem[] => items.slice().sort(byInboxOrder);

/** Does the item show under the tabs? */
export function matchesTabs(i: InboxItem, tab: InboxTab, status: InboxStatusTab, project = ""): boolean {
  if (project && i.project !== project) return false;
  // Asked lists the waiting asks whatever the kind tab.
  if (status === "asked") return i.kind === "watch" && i.open;
  if (tab !== "all" && i.kind !== tab) return false;
  if (status === "open") {
    if (tab === "all") return i.open && (i.kind === "question" || (i.kind === "notice" && isCritical(i)));
    return i.open && i.kind !== "watch";
  }
  return !i.open && i.kind !== "system";
}

/**
 * Put a changed item into the list and keep the order. An item that no
 * longer matches the tabs leaves the list.
 */
export function putItem(list: InboxItem[], item: InboxItem, keep: (i: InboxItem) => boolean): InboxItem[] {
  const rest = list.filter((x) => x.id !== item.id);
  return sortItems(keep(item) ? [item, ...rest] : rest);
}

/**
 * The counts of a list of items, as `GET /api/inbox/counts` gives them for the open items. `open`, `urgent`, and
 * `critical` leave out the watches (they wait for an agent) and the threads (the Threads badge counts them, TER-466).
 */
export function countItems(items: InboxItem[]): InboxCounts {
  const all = items.filter((i) => i.open);
  const open = all.filter((i) => i.kind !== "watch" && i.kind !== "thread");
  const threads = all.filter((i) => i.kind === "thread");
  const n = (k: InboxKind) => all.filter((i) => i.kind === k).length;
  return {
    open: open.length,
    urgent: open.filter((i) => i.urgent).length,
    critical: open.filter(isCritical).length,
    question: n("question"),
    notice: n("notice"),
    mail: n("mail"),
    system: n("system"),
    watch: n("watch"),
    thread: threads.length,
    thread_urgent: threads.filter((i) => i.urgent).length,
    thread_critical: threads.filter(isCritical).length,
  };
}

/** The nav badge: no badge when nothing is open; red when one item is urgent or critical, else the info style. */
export function badge(c: InboxCounts): { n: number; cls: string; title: string } | null {
  if (c.open <= 0) return null;
  const parts = (["question", "notice", "mail", "system"] as const).filter((k) => (c[k] ?? 0) > 0).map((k) => `${c[k]} ${KIND_LABELS[k]}`);
  return {
    n: c.open,
    cls: c.urgent > 0 || (c.critical ?? 0) > 0 ? "nav-count" : "nav-count info",
    title: `Waiting for you: ${parts.join(", ")}${c.critical ? `. ${c.critical} critical` : ""}${c.urgent ? `. ${c.urgent} urgent` : ""}`,
  };
}

export const KIND_LABELS: Record<InboxKind, string> = { question: "question", notice: "notice", mail: "mail", system: "paused thread", watch: "asked", thread: "thread" };

/** Events that change the inbox: an `inbox` event, or a `question` event (an older daemon sends no `inbox` event). */
export const isInboxEvent = (e: Pick<AgentEvent, "kind">): boolean => e.kind === "inbox" || e.kind === "question";

/** The text of the empty list. */
export function emptyText(tab: InboxTab, status: InboxStatusTab): string {
  if (status === "done" && tab === "system") return "A system item shows only while it is open.";
  if (status === "asked") return "You wait for no agent. Ask an agent, or forward a question to one.";
  if (tab === "watch") return "No answered or cancelled asks yet.";
  if (status === "open") return tab === "all" ? "Nothing waits for you." : `No open ${KIND_LABELS[tab]} items.`;
  return "Nothing is done here yet.";
}

/**
 * The dashboard page that a mail of the daemon links: its last `Open: <url>`
 * line, as a path of this dashboard (a quota mail links `/subscriptions`, a
 * budget mail `/p/<project>`). Only a `system` mail, and only a path. The
 * old `/subscriptions` and the new `/settings/subscriptions` get the same
 * label; the old path redirects.
 */
/** The Subscriptions page, at its old and its new path, with or without a query. */
const SUBSCRIPTIONS_PATHS = /^\/(settings\/)?subscriptions\/?(\?|#|$)/;

export function systemMailLink(m: Pick<Message, "from_kind" | "body">): { href: string; label: string } | null {
  if (m.from_kind !== "system") return null;
  const line = m.body
    .split("\n")
    .reverse()
    .find((l) => l.startsWith("Open: "));
  if (!line) return null;
  const url = line.slice(6).trim();
  let href: string;
  if (url.startsWith("/") && !url.startsWith("//")) href = url;
  else {
    try {
      const u = new URL(url);
      if (u.protocol !== "http:" && u.protocol !== "https:") return null;
      href = `${u.pathname}${u.search}`;
    } catch {
      return null;
    }
  }
  const label = SUBSCRIPTIONS_PATHS.test(href) ? "Subscriptions" : href.startsWith("/p/") ? "Project" : "Open";
  return { href, label };
}

// ----- Ask and forward (GitHub #24, docs/api.md §6.9) -----

/** The most characters of the text of an ask, as the daemon. */
export const ASK_TEXT_MAX = 4000;

/** The body of `POST /api/inbox/asks`, or what is wrong. A forward (`question`) may have no text: the text is a note. */
export function askBody(to: string, text: string, question?: string): { body: { to: string; text?: string; question?: string } } | { error: string } {
  const agent = to.trim();
  const t = text.trim();
  if (!agent) return { error: "Pick an agent." };
  if (!question && !t) return { error: "Write the question." };
  if (t.length > ASK_TEXT_MAX) return { error: `The text has ${t.length} characters; at most ${ASK_TEXT_MAX}.` };
  return { body: { to: agent, ...(t ? { text: t } : {}), ...(question ? { question } : {}) } };
}

/** The longest subject of a message from the Inbox (TER-46). */
export const MESSAGE_SUBJECT_MAX = 80;

/**
 * The mail of "Message an agent" (TER-46): to the agent, the subject from the
 * first line of the text (cut at {@link MESSAGE_SUBJECT_MAX} characters), the
 * whole text as the body. A plain mail: no watch, nothing waits for an answer.
 */
export function messageBody(to: string, text: string): { body: { to: string; subject: string; body: string } } | { error: string } {
  const agent = to.trim();
  const t = text.trim();
  if (!agent) return { error: "Pick an agent." };
  if (!t) return { error: "Write the message." };
  if (t.length > ASK_TEXT_MAX) return { error: `The text has ${t.length} characters; at most ${ASK_TEXT_MAX}.` };
  const first = t.split("\n").find((l) => l.trim())!.trim();
  const subject = first.length > MESSAGE_SUBJECT_MAX ? `${first.slice(0, MESSAGE_SUBJECT_MAX - 1)}…` : first;
  return { body: { to: agent, subject, body: t } };
}

/** The state of a watch: waiting for the agent, answered, or cancelled. */
export function watchState(w: Pick<InboxWatch, "waiting" | "cancelled_at" | "replies">): { label: string; chip: string } {
  if (w.cancelled_at) return { label: "cancelled", chip: "" };
  if (w.waiting) return { label: w.replies ? "waiting again" : "waiting", chip: "info" };
  return { label: "answered", chip: "ok" };
}

/** The title line of a watch item: `Waiting for p/y: <subject>` while it waits. */
export function watchTitle(w: Pick<InboxWatch, "waiting" | "cancelled_at" | "agent_name" | "subject">): string {
  const subject = w.subject || "(no subject)";
  if (w.cancelled_at) return `No longer waiting for ${w.agent_name}: ${subject}`;
  return w.waiting ? `Waiting for ${w.agent_name}: ${subject}` : `Answered by ${w.agent_name}: ${subject}`;
}

/** The line on a mail that answers a watch: a forward links its question; an ask names its subject. */
export function answerLine(w: Pick<InboxWatch, "question_id" | "subject">): string {
  return w.question_id ? `answer to your forward of ${w.question_id}` : `answer to your question "${w.subject}"`;
}

/** Can the operator forward this question? Only an open question, bundle, or notice. */
export const canForward = (q: Pick<Question, "status">): boolean => q.status === "open";
