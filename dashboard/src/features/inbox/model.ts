// The operator's inbox: everything that waits for the operator (questions,
// notices, mail to `operator`, and paused threads). Pure helpers, no Solid
// imports. The daemon filters and sorts (`GET /api/inbox`); these helpers
// keep a list in the same order after a local change, and give the tabs and
// the badge.

import type { AgentEvent, InboxCounts, InboxItem, InboxKind, InboxStatus } from "../../api/types";

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

export const EMPTY_COUNTS: InboxCounts = { open: 0, urgent: 0, question: 0, notice: 0, mail: 0, system: 0 };

/** The query of a tab pair, for `GET /api/inbox`. */
export function inboxQuery(tab: InboxTab, status: InboxStatusTab, project: string): { status: InboxStatus; kind?: InboxKind; project?: string } {
  return { status, ...(tab === "all" ? {} : { kind: tab }), ...(project ? { project } : {}) };
}

/** Urgent first, then the newest (`at`), as the daemon. */
export const byInboxOrder = (a: InboxItem, b: InboxItem): number => Number(b.urgent) - Number(a.urgent) || b.at.localeCompare(a.at) || b.id.localeCompare(a.id);

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
  return { open: open.length, urgent: open.filter((i) => i.urgent).length, question: n("question"), notice: n("notice"), mail: n("mail"), system: n("system") };
}

/** The nav badge: no badge when nothing is open; red when one item is urgent, else the info style. */
export function badge(c: InboxCounts): { n: number; cls: string; title: string } | null {
  if (c.open <= 0) return null;
  const parts = (["question", "notice", "mail", "system"] as const).filter((k) => c[k] > 0).map((k) => `${c[k]} ${KIND_LABELS[k]}`);
  return {
    n: c.open,
    cls: c.urgent > 0 ? "nav-count" : "nav-count info",
    title: `Waiting for you: ${parts.join(", ")}${c.urgent ? `. ${c.urgent} urgent` : ""}`,
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
