// The "waiting on you" queue (§17 Home). Pure assembly, testable.

import type { Board, Version, WikiPageSummary } from "../../api/types";
import type { LogEntry } from "../mail/log";

export type QueueItem =
  | { kind: "version"; project: string; at: string; version: Version }
  | { kind: "page"; project: string; at: string; page: WikiPageSummary }
  | { kind: "card"; project: string; at: string; card: Board["cards"][number] }
  /** Mail that needs the operator: the attention rule of the mail log (features/mail/log.ts). */
  | { kind: "mail"; project: string; at: string; entry: LogEntry };

export interface ProjectSnapshot {
  project: string;
  versions: Version[];
  pages: WikiPageSummary[];
  board: Board;
}

/** `mail` is the attention list (`attention.items()`): only entries with a reason go in. */
export function buildQueue(snaps: ProjectSnapshot[], mail: LogEntry[] = []): QueueItem[] {
  const items: QueueItem[] = [];
  for (const s of snaps) {
    for (const v of s.versions) if (v.status === "in_review") items.push({ kind: "version", project: s.project, at: v.submitted_at ?? v.created_at, version: v });
    for (const p of s.pages) if (p.status === "ready_for_review") items.push({ kind: "page", project: s.project, at: p.updated_at, page: p });
    for (const c of s.board.cards) if (c.column === "blocked") items.push({ kind: "card", project: s.project, at: c.updated_at, card: c });
  }
  for (const e of mail) if (e.reasons.length) items.push({ kind: "mail", project: e.projects[0] ?? "", at: e.message.created_at, entry: e });
  const order = { version: 0, page: 1, card: 2, mail: 3 } as const;
  return items.sort((a, b) => order[a.kind] - order[b.kind] || b.at.localeCompare(a.at));
}
