// Home, Work in progress (TER-43): the cards in the working column and in
// the research column of every project, with their agents
// (`GET /api/cards`, docs/api.md §8). Pure helpers for the section.

import type { ActiveCard } from "../../api/types";
import { countdown } from "../../lib/eta";

/** The cards of one project, in the order the daemon gave (column, then position). */
export interface ProjectCards {
  project: string;
  cards: ActiveCard[];
}

/** Group the cards by project, projects in name order. */
export function byProject(cards: ActiveCard[]): ProjectCards[] {
  const m = new Map<string, ActiveCard[]>();
  for (const c of cards) {
    const list = m.get(c.project) ?? [];
    list.push(c);
    m.set(c.project, list);
  }
  return [...m.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([project, list]) => ({ project, cards: list }));
}

/** The column as the section names it: `in progress` for the working column. */
export function columnLabel(kind: string): string {
  return kind === "working" ? "in progress" : kind;
}

/** The chip class of an agent status: busy, waiting, at rest, or needs help. */
export function statusTone(status: string | undefined): string {
  switch (status) {
    case "working":
    case "starting":
      return "ok";
    case "waiting":
      return "info";
    case "blocked":
    case "error":
      return "bad";
    default:
      return "";
  }
}

/** The assignee of a card: its agent name, `operator`, or `unassigned`. */
export function assigneeText(c: Pick<ActiveCard, "assignee" | "assignee_name">): string {
  if (!c.assignee) return "unassigned";
  return c.assignee_name ?? c.assignee;
}

/** The ETA text of a card: `1h 12m left`, `overdue 12m`, or none. */
export function etaText(c: ActiveCard, now: number): { label: string; overdue: boolean } | null {
  const d = countdown(c, now);
  return d ? { label: d.label, overdue: d.overdue } : null;
}

/** The counts for the section title: `3 in progress, 1 in research`. */
export function summary(cards: ActiveCard[]): string {
  const working = cards.filter((c) => c.column_kind === "working").length;
  const research = cards.filter((c) => c.column_kind === "research").length;
  const parts = [`${working} in progress`];
  if (research > 0) parts.push(`${research} in research`);
  return parts.join(", ");
}
