// The "waiting on you" queue (§17 Home). Pure assembly, testable.

import type { Board, Message, Version, WikiPageSummary } from "../../api/types";

export type QueueItem =
  | { kind: "version"; project: string; at: string; version: Version }
  | { kind: "page"; project: string; at: string; page: WikiPageSummary }
  | { kind: "card"; project: string; at: string; card: Board["cards"][number] }
  | { kind: "inbox"; project: string; at: string; message: Message };

export interface ProjectSnapshot {
  project: string;
  versions: Version[];
  pages: WikiPageSummary[];
  board: Board;
  inbox: Message[];
}

export function buildQueue(snaps: ProjectSnapshot[]): QueueItem[] {
  const items: QueueItem[] = [];
  for (const s of snaps) {
    for (const v of s.versions) if (v.status === "in_review") items.push({ kind: "version", project: s.project, at: v.submitted_at ?? v.created_at, version: v });
    for (const p of s.pages) if (p.status === "ready_for_review") items.push({ kind: "page", project: s.project, at: p.updated_at, page: p });
    for (const c of s.board.cards) if (c.column === "blocked") items.push({ kind: "card", project: s.project, at: c.updated_at, card: c });
    for (const m of s.inbox) if (m.state === "queued" || m.state === "delivered") items.push({ kind: "inbox", project: s.project, at: m.created_at, message: m });
  }
  const order = { version: 0, page: 1, card: 2, inbox: 3 } as const;
  return items.sort((a, b) => order[a.kind] - order[b.kind] || b.at.localeCompare(a.at));
}
