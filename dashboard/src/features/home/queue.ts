// The "waiting on you" queue (§17 Home). Pure assembly, testable.

import type { Board, InboxItem, Schedule, Version, WikiPageSummary } from "../../api/types";

export type QueueItem =
  /** An open item of the Inbox: a question, a notice, mail to the operator, or a paused thread. */
  | { kind: "inbox"; project: string; at: string; item: InboxItem }
  | { kind: "version"; project: string; at: string; version: Version }
  | { kind: "page"; project: string; at: string; page: WikiPageSummary }
  | { kind: "card"; project: string; at: string; card: Board["cards"][number] }
  /** A schedule whose last run failed in the last 24 h (features/schedules/model.ts `failedRecently`). */
  | { kind: "schedule"; project: string; at: string; schedule: Schedule };

export interface ProjectSnapshot {
  project: string;
  versions: Version[];
  pages: WikiPageSummary[];
  board: Board;
}

/**
 * `inbox` is the open list of the Inbox (`inbox.open()`), urgent first: the
 * queue shows it first, in its own order. `failedSchedules` are the
 * schedules whose last run failed (`failedRecently`); each one is an item at
 * the time of that run. The other kinds follow, the newest first in each.
 */
export function buildQueue(snaps: ProjectSnapshot[], inbox: InboxItem[] = [], failedSchedules: Schedule[] = []): QueueItem[] {
  const items: QueueItem[] = [];
  for (const s of snaps) {
    for (const v of s.versions) if (v.status === "in_review") items.push({ kind: "version", project: s.project, at: v.submitted_at ?? v.created_at, version: v });
    for (const p of s.pages) if (p.status === "ready_for_review") items.push({ kind: "page", project: s.project, at: p.updated_at, page: p });
    for (const c of s.board.cards) if (c.column === "blocked") items.push({ kind: "card", project: s.project, at: c.updated_at, card: c });
  }
  for (const s of failedSchedules) if (s.last_run?.status === "failed") items.push({ kind: "schedule", project: s.project ?? "", at: s.last_run.at, schedule: s });
  const order = { version: 0, page: 1, card: 2, schedule: 3 } as const;
  items.sort((a, b) => order[a.kind as keyof typeof order] - order[b.kind as keyof typeof order] || b.at.localeCompare(a.at));
  const asked: QueueItem[] = inbox.filter((i) => i.open).map((i) => ({ kind: "inbox", project: i.project ?? "", at: i.at, item: i }));
  return [...asked, ...items];
}
