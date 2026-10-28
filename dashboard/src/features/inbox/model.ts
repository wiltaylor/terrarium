// The operator's inbox: everything that waits for the operator (questions,
// notices, mail to `operator`, and paused threads). Pure helpers, no Solid
// imports. The daemon filters and sorts (`GET /api/inbox`); these helpers
// keep a list in the same order after a local change, and give the tabs and
// the badge.

import type { AgentEvent, InboxCounts, InboxItem, InboxKind, InboxStatus, Message, Question, QuestionPriority } from "../../api/types";

/** The kind tabs of the Inbox page. `all` sends no `kind`. */
export const INBOX_TABS = [
  { id: "all", label: "All" },
  { id: "question", label: "Questions" },
  { id: "notice", label: "Notices" },
  { id: "mail", label: "Mail to you" },
  { id: "system", label: "System" },
] as const;
export type InboxTab = (typeof INBOX_TABS)[number]["id"];

/** The status tabs. System items show only while they are open. */
export const STATUS_TABS = [
  { id: "open", label: "Open" },
  { id: "done", label: "Done" },
] as const;
export type InboxStatusTab = (typeof STATUS_TABS)[number]["id"];

export const EMPTY_COUNTS: InboxCounts = { open: 0, urgent: 0, critical: 0, question: 0, notice: 0, mail: 0, system: 0 };

/** The query of a tab pair, for `GET /api/inbox`. */
export function inboxQuery(tab: InboxTab, status: InboxStatusTab, project: string): { status: InboxStatus; kind?: InboxKind; project?: string } {
  return { status, ...(tab === "all" ? {} : { kind: tab }), ...(project ? { project } : {}) };
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
  if (tab !== "all" && i.kind !== tab) return false;
  if (project && i.project !== project) return false;
  return status === "open" ? i.open : !i.open && i.kind !== "system";
}

/**
 * Put a changed item into the list and keep the order. An item that no
 * longer matches the tabs leaves the list.
 */
export function putItem(list: InboxItem[], item: InboxItem, keep: (i: InboxItem) => boolean): InboxItem[] {
  const rest = list.filter((x) => x.id !== item.id);
  return sortItems(keep(item) ? [item, ...rest] : rest);
}

/** The counts of a list of items, as `GET /api/inbox/counts` gives them for the open items. */
export function countItems(items: InboxItem[]): InboxCounts {
  const open = items.filter((i) => i.open);
  const n = (k: InboxKind) => open.filter((i) => i.kind === k).length;
  return {
    open: open.length,
    urgent: open.filter((i) => i.urgent).length,
    critical: open.filter(isCritical).length,
    question: n("question"),
    notice: n("notice"),
    mail: n("mail"),
    system: n("system"),
  };
}

/** The nav badge: no badge when nothing is open; red when one item is urgent or critical, else the info style. */
export function badge(c: InboxCounts): { n: number; cls: string; title: string } | null {
  if (c.open <= 0) return null;
  const parts = (["question", "notice", "mail", "system"] as const).filter((k) => c[k] > 0).map((k) => `${c[k]} ${KIND_LABELS[k]}`);
  return {
    n: c.open,
    cls: c.urgent > 0 || (c.critical ?? 0) > 0 ? "nav-count" : "nav-count info",
    title: `Waiting for you: ${parts.join(", ")}${c.critical ? `. ${c.critical} critical` : ""}${c.urgent ? `. ${c.urgent} urgent` : ""}`,
  };
}

export const KIND_LABELS: Record<InboxKind, string> = { question: "question", notice: "notice", mail: "mail", system: "paused thread" };

/** Events that change the inbox: an `inbox` event, or a `question` event (an older daemon sends no `inbox` event). */
export const isInboxEvent = (e: Pick<AgentEvent, "kind">): boolean => e.kind === "inbox" || e.kind === "question";

/** The text of the empty list. */
export function emptyText(tab: InboxTab, status: InboxStatusTab): string {
  if (status === "done" && tab === "system") return "A system item shows only while it is open.";
  if (status === "open") return tab === "all" ? "Nothing waits for you." : `No open ${KIND_LABELS[tab]} items.`;
  return "Nothing is done here yet.";
}

/**
 * The dashboard page that a mail of the daemon links: its last `Open: <url>`
 * line, as a path of this dashboard (a quota mail links `/subscriptions`, a
 * budget mail `/p/<project>`). Only a `system` mail, and only a path.
 */
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
  const label = href === "/subscriptions" ? "Subscriptions" : href.startsWith("/p/") ? "Project" : "Open";
  return { href, label };
}
