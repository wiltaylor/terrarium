import { describe, expect, it } from "vitest";
import type { BoardGroup, Card } from "../../api/types";
import { groupSections } from "./groups";

const group: BoardGroup = { id: "P-3", key: "batch", title: "Batch", column: "review", status: "", done: 1, total: 2, members: ["P-2", "P-1"], eta_at: null, closed_at: null };
const card = (id: string, column: string, kind: "card" | "group" = "card"): Card => ({ id, column, kind, group } as Card);

describe("board groups", () => {
  it("renders one header per occupied column without moving members", () => {
    const cards = [card("P-1", "ready"), card("P-2", "review"), card("P-3", "review", "group")];
    expect(groupSections(cards, "ready")).toEqual([{ group, cards: [cards[0]] }]);
    expect(groupSections(cards, "review")).toEqual([{ group, cards: [cards[1]] }]);
    expect(groupSections(cards, "done")).toEqual([]);
  });

  it("keeps membership order and never renders a draggable group card", () => {
    const cards = [card("P-1", "review"), card("P-3", "review", "group"), card("P-2", "review")];
    expect(groupSections(cards, "review")[0]!.cards.map((c) => c.id)).toEqual(["P-2", "P-1"]);
  });

  it("shows the home header even before members are added", () => {
    expect(groupSections([card("P-3", "review", "group")], "review")).toEqual([{ group, cards: [] }]);
  });
});
