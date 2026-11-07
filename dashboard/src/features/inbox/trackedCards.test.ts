// The mini kanban rows and the n/m count of the linked cards of a thread (TER-147).
import { describe, expect, it } from "vitest";
import type { Board, Card, Column, ThreadCard } from "../../api/types";
import { cardProgress, inReviewOrLater, kanbanRows, linkedProjects, progressChip } from "./trackedCards";

const col = (name: string, position: number, role?: string): Column => ({ name, position, ...(role ? { role: role as Column["role"] } : {}) });
/** The default board of a new daemon project (docs/api.md §8.1). */
const ROLES = [col("backlog", 0), col("ready", 1), col("in_progress", 2, "working"), col("validating", 3, "validating"), col("review", 4, "review"), col("merge_queue", 5, "merge_queue"), col("accepted", 6, "accepted"), col("blocked", 7, "blocked"), col("done", 8)];
/** An older board: `review` by name only. */
const NAMES = [col("backlog", 0), col("ready", 1), col("in_progress", 2, "working"), col("review", 3), col("blocked", 4, "blocked"), col("done", 5)];

const card = (id: string, project: string, column: string, extra: Partial<Card> = {}): Card => ({
  id,
  project,
  column,
  position: 0,
  title: `Card ${id}`,
  body: "",
  labels: [],
  depends_on: [],
  blocked_by: [],
  comments: [],
  links: [],
  created_by: "operator",
  created_at: "2026-11-07T00:00:00Z",
  updated_at: "2026-11-07T00:00:00Z",
  archived: false,
  ...extra,
});
const link = (project: string, id: string): ThreadCard => ({ project, card: id, linked_by: "operator", linked_at: "2026-11-07T00:00:00Z" });
const board = (project: string, columns: Column[], cards: Card[]): Board => ({ project, columns, cards });

describe("inReviewOrLater", () => {
  it("counts the review column and the later ones, never the blocked one, on a board with roles", () => {
    const yes = ROLES.filter((c) => inReviewOrLater(ROLES, c.name)).map((c) => c.name);
    expect(yes).toEqual(["review", "merge_queue", "accepted", "done"]);
  });

  it("finds the review column by name on a board without the role, and reads the board order, not the list order", () => {
    const shuffled = [...NAMES].reverse();
    expect(shuffled.filter((c) => inReviewOrLater(shuffled, c.name)).map((c) => c.name)).toEqual(["done", "review"]);
  });

  it("counts only `done` on a board with no review column, and nothing for an unknown column", () => {
    const cols = [col("backlog", 0), col("doing", 1, "working"), col("blocked", 2, "blocked"), col("done", 3)];
    expect(cols.filter((c) => inReviewOrLater(cols, c.name)).map((c) => c.name)).toEqual(["done"]);
    expect(inReviewOrLater(cols, "gone")).toBe(false);
  });
});

describe("kanbanRows", () => {
  const links = [link("webshop", "WEB-4"), link("terrarium", "TER-16"), link("webshop", "web-1"), link("webshop", "WEB-9"), link("terrarium", "TER-2")];
  const boards = {
    webshop: board("webshop", NAMES, [card("WEB-4", "webshop", "in_progress"), card("WEB-1", "webshop", "done"), card("WEB-7", "webshop", "in_progress"), card("WEB-9", "webshop", "ready", { archived: true })]),
    terrarium: board("terrarium", ROLES, [card("TER-16", "terrarium", "review"), card("TER-2", "terrarium", "review", { position: 1 })]),
  };

  it("gives one row per project in the order of the first link, with every column of its own board in board order", () => {
    expect(linkedProjects(links)).toEqual(["webshop", "terrarium"]);
    const rows = kanbanRows(links, boards);
    expect(rows.map((r) => r.project)).toEqual(["webshop", "terrarium"]);
    expect(rows[0]!.columns.map((c) => c.name)).toEqual(NAMES.map((c) => c.name));
    expect(rows[1]!.columns.map((c) => c.name)).toEqual(ROLES.map((c) => c.name));
    expect(rows[1]!.columns.find((c) => c.name === "review")!.role).toBe("review");
    expect(rows[0]!.columns[2]!.title).toBe("in progress");
  });

  it("puts only the linked cards in their columns, matches the ID in any case, and keeps the board position", () => {
    const [web, ter] = kanbanRows(links, boards);
    const ids = (r: typeof web, name: string) => r!.columns.find((c) => c.name === name)!.cards.map((c) => c.id);
    expect(ids(web, "in_progress")).toEqual(["WEB-4"]); // WEB-7 is not linked
    expect(ids(web, "done")).toEqual(["WEB-1"]);
    expect(ids(web, "backlog")).toEqual([]);
    expect(ids(ter, "review")).toEqual(["TER-16", "TER-2"]);
  });

  it("lists an archived or unknown linked card as missing, and a project whose board did not load as unavailable", () => {
    expect(kanbanRows(links, boards)[0]!.missing).toEqual(["WEB-9"]);
    const rows = kanbanRows(links, { webshop: boards.webshop });
    expect(rows[1]).toEqual({ project: "terrarium", columns: [], missing: ["TER-16", "TER-2"], unavailable: true });
  });
});

describe("cardProgress and the n/m chip", () => {
  it("counts the linked cards in review or later of the linked cards on their boards", () => {
    const links = [link("webshop", "WEB-4"), link("webshop", "WEB-1"), link("terrarium", "TER-16"), link("terrarium", "TER-3")];
    const boards = {
      webshop: board("webshop", NAMES, [card("WEB-4", "webshop", "in_progress"), card("WEB-1", "webshop", "done")]),
      terrarium: board("terrarium", ROLES, [card("TER-16", "terrarium", "merge_queue"), card("TER-3", "terrarium", "blocked")]),
    };
    expect(cardProgress(links, boards)).toEqual({ done: 2, total: 4 });
    expect(progressChip({ done: 2, total: 4 })).toEqual({ label: "2/4", title: "2 of 4 linked cards are in review or later", complete: false });
    // A board that did not load leaves its cards out of both numbers.
    expect(cardProgress(links, { webshop: boards.webshop })).toEqual({ done: 1, total: 2 });
  });

  it("is complete when every card is in review or later, and has no chip without a card on a board", () => {
    expect(progressChip({ done: 1, total: 1 })).toEqual({ label: "1/1", title: "1 of 1 linked card is in review or later", complete: true });
    expect(progressChip({ done: 0, total: 0 })).toBeNull();
    expect(cardProgress([], {})).toEqual({ done: 0, total: 0 });
  });
});
