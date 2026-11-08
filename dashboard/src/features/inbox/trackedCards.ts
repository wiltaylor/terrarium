// The linked cards of an Inbox thread (TER-147, D3 of TER-144 with the
// scope change M1-M3): a mini kanban on the thread page, one row per project
// with that project's own board columns in board order, and the n/m chip of
// the Threads list. The thread lists only the references (`Thread.cards`,
// already cut to the projects the reader may read); the cards and the columns
// come live from the board of each project. Pure functions, no Solid imports.

import type { Board, CardSummary as Card, Column, ThreadCard } from "../../api/types";

/** One column of a project row: its cards among the linked ones, in board order. */
export interface KanbanColumn {
  name: string;
  title: string;
  role?: string;
  cards: Card[];
}

/** One project row of the mini kanban. */
export interface KanbanRow {
  project: string;
  columns: KanbanColumn[];
  /** Linked cards of the project that its board does not show: archived, or the board did not load. */
  missing: string[];
  /** The board of the project did not load (no read right any more, or an error). */
  unavailable: boolean;
}

/** n of m: the linked cards in review or later, of all linked cards on their boards. */
export interface CardProgress {
  done: number;
  total: number;
}

/** The projects of the links, in the order of their first link. */
export const linkedProjects = (links: readonly ThreadCard[]): string[] => [...new Set(links.map((l) => l.project))];

const sameId = (a: string, b: string) => a.toUpperCase() === b.toUpperCase();

/**
 * Whether column `column` is the review column or a later one of `columns`
 * (board order): the anchor is the column with the role `review` (docs/api.md
 * §8.1), else the column named `review`. The column with the role `blocked`
 * never counts. A board with neither counts only the column named `done`.
 */
export function inReviewOrLater(columns: readonly Column[], column: string): boolean {
  const ordered = [...columns].sort((a, b) => a.position - b.position);
  const at = ordered.findIndex((c) => c.name === column);
  if (at < 0) return false;
  const col = ordered[at]!;
  if ((col.role as string | undefined) === "blocked") return false;
  let anchor = ordered.findIndex((c) => (c.role as string | undefined) === "review");
  if (anchor < 0) anchor = ordered.findIndex((c) => c.name === "review");
  if (anchor < 0) return col.name === "done";
  return at >= anchor;
}

/** The linked cards of `project` on its board, in link order. */
function linkedOn(project: string, links: readonly ThreadCard[], board: Board | undefined): { cards: Card[]; missing: string[] } {
  const cards: Card[] = [];
  const missing: string[] = [];
  for (const l of links) {
    if (l.project !== project) continue;
    const c = board?.cards.find((x) => sameId(x.id, l.card) && !x.archived);
    if (c) {
      if (!cards.includes(c)) cards.push(c);
    } else missing.push(l.card);
  }
  return { cards, missing };
}

/**
 * The rows of the mini kanban: one per project in the order of its first
 * link, each with every column of that project's board in board order (an
 * empty column too: the view draws it narrow). `boards` has the boards that
 * loaded; a project without one is `unavailable`, and its cards are `missing`.
 */
export function kanbanRows(links: readonly ThreadCard[], boards: Readonly<Record<string, Board | undefined>>): KanbanRow[] {
  return linkedProjects(links).map((project) => {
    const board = boards[project];
    const { cards, missing } = linkedOn(project, links, board);
    const columns = [...(board?.columns ?? [])]
      .sort((a, b) => a.position - b.position)
      .map((c) => {
        const inCol = cards.filter((x) => x.column === c.name).sort((a, b) => a.position - b.position);
        return { name: c.name, title: c.title || c.name.replace(/_/g, " "), ...(c.role ? { role: c.role as string } : {}), cards: inCol };
      });
    return { project, columns, missing, unavailable: !board };
  });
}

/** The n/m count of a thread: the linked cards in review or later, of the linked cards that their boards show. */
export function cardProgress(links: readonly ThreadCard[], boards: Readonly<Record<string, Board | undefined>>): CardProgress {
  let done = 0;
  let total = 0;
  for (const project of linkedProjects(links)) {
    const board = boards[project];
    if (!board) continue;
    for (const c of linkedOn(project, links, board).cards) {
      total++;
      if (inReviewOrLater(board.columns, c.column)) done++;
    }
  }
  return { done, total };
}

/** The chip text and tooltip of a count; `null` when no linked card is on a board. */
export function progressChip(p: CardProgress): { label: string; title: string; complete: boolean } | null {
  if (p.total === 0) return null;
  return {
    label: `${p.done}/${p.total}`,
    title: `${p.done} of ${p.total} linked ${p.total === 1 ? "card is" : "cards are"} in review or later`,
    complete: p.done === p.total,
  };
}
