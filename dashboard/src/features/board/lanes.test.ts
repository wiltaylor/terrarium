import { describe, expect, it } from "vitest";
import type { Card } from "../../api/types";
import { ALL_LANE, alsoIn, buildLanes, columnIndex, isPlain, laneCells, laneMoveLabels, NO_VERSION_LANE, readLaneState, toggleCollapsed } from "./lanes";

const card = (id: string, labels: string[] = [], column = "ready", position = 0): Card => ({
  id,
  project: "p",
  column,
  position,
  title: id,
  body: "",
  labels,
  depends_on: [],
  blocked_by: [],
  comments: [],
  links: [],
  created_by: "operator",
  created_at: "t",
  updated_at: "t",
  archived: false,
});

const cards = [
  card("P-1", ["version:0.9.0"], "done", 0),
  card("P-2", ["feature", "version:vnext", "version:1.0.0"], "ready", 1),
  card("P-3", ["version:vnext"], "done", 2),
  card("P-4", ["version:1.0.0-rc.1"], "ready", 3),
  card("P-5", ["bug"], "done", 4),
  card("P-6", [], "ready", 5),
];

describe("lanes", () => {
  it("shows one plain lane with every card when the board is not grouped", () => {
    const lanes = buildLanes(cards, false);
    expect(lanes.map((l) => l.key)).toEqual([ALL_LANE]);
    expect(lanes[0]!.cards).toHaveLength(cards.length);
    expect(isPlain(lanes)).toBe(true);
    expect(alsoIn(cards[1]!, ALL_LANE)).toEqual([]);
  });

  it("groups the cards by version label: vnext first, the newest release next, No version last", () => {
    const lanes = buildLanes(cards, true);
    expect(lanes.map((l) => l.key)).toEqual(["version:vnext", "version:1.0.0", "version:1.0.0-rc.1", "version:0.9.0", NO_VERSION_LANE]);
    expect(lanes[0]!.version).toBe("vnext");
    expect(isPlain(lanes)).toBe(false);
    const of = (k: string) => lanes.find((l) => l.key === k)!.cards.map((c) => c.id);
    // A card with two version labels is in both lanes.
    expect(of("version:vnext")).toEqual(["P-2", "P-3"]);
    expect(of("version:1.0.0")).toEqual(["P-2"]);
    expect(of(NO_VERSION_LANE)).toEqual(["P-5", "P-6"]);
    expect(laneCells(lanes[0]!, "ready").map((c) => c.id)).toEqual(["P-2"]);
    expect(alsoIn(cards[1]!, "version:vnext")).toEqual(["1.0.0"]);
    expect(alsoIn(cards[2]!, "version:vnext")).toEqual([]);
  });

  it("keeps the No version lane on a board with no version label", () => {
    const lanes = buildLanes([card("A"), card("B", ["bug"])], true);
    expect(lanes.map((l) => l.key)).toEqual([NO_VERSION_LANE]);
    expect(lanes[0]!.cards).toHaveLength(2);
  });

  it("changes the version labels of a card on a drag between lanes", () => {
    expect(laneMoveLabels(["feature", "version:vnext"], "version:vnext", "version:1.0.0")).toEqual(["feature", "version:1.0.0"]);
    expect(laneMoveLabels(["version:vnext", "version:1.0.0"], "version:1.0.0", NO_VERSION_LANE)).toEqual(["version:vnext"]);
    expect(laneMoveLabels(["bug"], NO_VERSION_LANE, "version:vnext")).toEqual(["bug", "version:vnext"]);
    // Into a lane whose label the card has already: only the source label goes.
    expect(laneMoveLabels(["version:vnext", "version:1.0.0"], "version:vnext", "version:1.0.0")).toEqual(["version:1.0.0"]);
    // No change: the same lane, or the plain board.
    expect(laneMoveLabels(["version:vnext"], "version:vnext", "version:vnext")).toBeNull();
    expect(laneMoveLabels([], ALL_LANE, ALL_LANE)).toBeNull();
  });

  it("maps a drop in a lane cell to the column index", () => {
    const col = [card("A", [], "r", 0), card("B", [], "r", 1), card("C", [], "r", 2), card("D", [], "r", 3)];
    const cell = [col[1]!, col[3]!];
    expect(columnIndex(cell, col, 0)).toBe(1);
    expect(columnIndex(cell, col, 1)).toBe(3);
    expect(columnIndex(cell, col, 2)).toBe(4);
    expect(columnIndex([], col, 0)).toBe(4);
  });

  it("keeps the collapsed lanes and ignores an old rows mode", () => {
    expect(readLaneState("{bad")).toEqual({ collapsed: [] });
    expect(readLaneState(null)).toEqual({ collapsed: [] });
    const s = readLaneState(JSON.stringify({ rows: "none", collapsed: ["A", 3] }));
    expect(s).toEqual({ collapsed: ["A"] });
    expect(toggleCollapsed(s, "A").collapsed).toEqual([]);
    expect(toggleCollapsed(s, "B").collapsed).toEqual(["A", "B"]);
  });
});
