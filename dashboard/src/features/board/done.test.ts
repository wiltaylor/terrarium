import { describe, expect, it } from "vitest";
import type { Board, Card } from "../../api/types";
import { boardView, doneToggleHref } from "./done";
import { buildLanes } from "./lanes";

const card = (id: string, column: string, version: string): Card => ({
  id, project: "p", column, position: 0, title: id, body: "",
  labels: [`version:${version}`], depends_on: [], blocked_by: [], comments: [], links: [],
  created_by: "operator", created_at: "t", updated_at: "t", archived: false,
});
const board: Board = {
  project: "p",
  columns: [{ name: "ready", position: 0 }, { name: "done", title: "Released", position: 1 }],
  cards: [card("P-1", "ready", "vnext"), card("P-2", "done", "1.0")],
};

describe("done screen", () => {
  it("hides done cards and columns by default without changing the source board", () => {
    const view = boardView(board);
    expect(view.columns.map((c) => c.name)).toEqual(["ready"]);
    expect(view.cards.map((c) => c.id)).toEqual(["P-1"]);
    expect(board.cards).toHaveLength(2);
    expect(board.columns).toHaveLength(2);
  });
  it("shows only completed cards on the separate screen, even with a custom title", () => {
    const view = boardView(board, true);
    expect(view.columns.map((c) => c.name)).toEqual(["done"]);
    expect(view.cards.map((c) => c.id)).toEqual(["P-2"]);
  });
  it("does not create version lanes for hidden cards", () => {
    expect(buildLanes(boardView(board).cards, true).map((l) => l.version)).toEqual(["vnext", undefined]);
    expect(buildLanes(boardView(board, true).cards, true).map((l) => l.version)).toEqual(["1.0", undefined]);
  });
  it("handles an empty done screen", () => {
    expect(boardView({ ...board, cards: [] }, true).cards).toEqual([]);
  });
});

describe("done toggle", () => {
  it("keeps the version filter and the grouping both ways", () => {
    expect(doneToggleHref("/p/p", false, { version: "vnext", group: "version" })).toBe("/p/p/board/done?version=vnext&group=version");
    expect(doneToggleHref("/p/p", true, { version: "1.0", group: "version" })).toBe("/p/p/board?version=1.0&group=version");
  });
  it("drops empty and other values", () => {
    expect(doneToggleHref("/p/p", false, {})).toBe("/p/p/board/done");
    expect(doneToggleHref("/p/p", true, { version: " ", group: "rows" })).toBe("/p/p/board");
    expect(doneToggleHref("/p/a%20b", false, { version: "v 1" })).toBe("/p/a%20b/board/done?version=v+1");
  });
});
