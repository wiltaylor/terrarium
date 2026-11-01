import { describe, expect, it } from "vitest";
import type { Card, Version } from "../../api/types";
import {
  alsoIn,
  buildLanes,
  columnIndex,
  findVersion,
  laneCells,
  laneMove,
  laneMovePatches,
  laneVersions,
  menuVersions,
  NO_VERSION_LANE,
  parseRows,
  readLaneState,
  toggleCollapsed,
} from "./lanes";

const v = (id: string, status: Version["status"], cards: string[], created_at: string, over: Partial<Version> = {}): Version => ({
  id,
  project: "p",
  name: id.toLowerCase(),
  status,
  notes: "",
  created_by: "operator",
  created_at,
  cards: cards.map((c) => ({ id: c, title: c, column: "x" })),
  artifacts: [],
  reviews: [],
  ...over,
});
const card = (id: string, column = "ready", position = 0): Card => ({
  id,
  project: "p",
  column,
  position,
  title: id,
  body: "",
  labels: [],
  depends_on: [],
  blocked_by: [],
  comments: [],
  links: [],
  created_by: "operator",
  created_at: "t",
  updated_at: "t",
  archived: false,
});

const versions = [
  v("REL", "released", ["P-1"], "2026-10-01"),
  v("REJ", "rejected", [], "2026-10-02"),
  v("APP", "approved", [], "2026-10-03"),
  v("DRAFT_OLD", "draft", ["P-2"], "2026-10-04"),
  v("DRAFT_NEW", "draft", ["P-2", "P-3"], "2026-10-05"),
  v("REVIEW", "in_review", ["P-4"], "2026-10-06"),
  v("ARCH", "draft", ["P-5", "P-1"], "2026-10-07", { archived: true }),
];
const cards = ["P-1", "P-2", "P-3", "P-4", "P-5", "P-6"].map((id, i) => card(id, i % 2 ? "ready" : "done", i));

describe("version lanes", () => {
  it("orders the lanes by status, then the newest first, with no archived version", () => {
    expect(laneVersions(versions).map((x) => x.id)).toEqual(["DRAFT_NEW", "DRAFT_OLD", "REVIEW", "APP", "REL", "REJ"]);
  });

  it("builds the lanes: empty released and rejected lanes hide; archived-only cards have no version", () => {
    const lanes = buildLanes(versions, cards);
    expect(lanes.map((l) => l.key)).toEqual(["DRAFT_NEW", "DRAFT_OLD", "REVIEW", "APP", "REL", NO_VERSION_LANE]);
    const of = (k: string) => lanes.find((l) => l.key === k)!.cards.map((c) => c.id);
    // A card in two versions is in both lanes.
    expect(of("DRAFT_NEW")).toEqual(["P-2", "P-3"]);
    expect(of("DRAFT_OLD")).toEqual(["P-2"]);
    // An empty approved lane shows; an empty rejected lane hides.
    expect(of("APP")).toEqual([]);
    // P-5 is only in an archived version; P-6 is in none.
    expect(of(NO_VERSION_LANE)).toEqual(["P-5", "P-6"]);
    expect(laneCells(lanes[0]!, "ready").map((c) => c.id)).toEqual(["P-2"]);
    expect(alsoIn("P-2", "DRAFT_NEW", versions).map((x) => x.id)).toEqual(["DRAFT_OLD"]);
    expect(alsoIn("P-1", "REL", versions)).toEqual([]);
  });

  it("says what a drag between lanes does", () => {
    // From one draft to another: remove and add; the card is in no other lane version.
    expect(laneMove("P-3", "DRAFT_NEW", "DRAFT_OLD", versions)).toMatchObject({ remove: { id: "DRAFT_NEW" }, add: { id: "DRAFT_OLD" }, others: [], refused: null });
    // A card in two versions: the page asks first.
    expect(laneMove("P-2", "DRAFT_NEW", NO_VERSION_LANE, versions).others.map((x) => x.id)).toEqual(["DRAFT_OLD"]);
    expect(laneMove("P-2", "DRAFT_NEW", NO_VERSION_LANE, versions).add).toBeUndefined();
    // Into a draft from "No version": add only.
    const m = laneMove("P-6", NO_VERSION_LANE, "DRAFT_NEW", versions);
    expect(m.remove).toBeUndefined();
    expect(m.add?.id).toBe("DRAFT_NEW");
    // A version in review, approved, or released takes no card change.
    expect(laneMove("P-4", "REVIEW", "DRAFT_NEW", versions).refused).toMatch(/in review/);
    expect(laneMove("P-6", NO_VERSION_LANE, "APP", versions).refused).toMatch(/approved/);
  });

  it("makes the version calls of a drag between lanes: remove, then add", () => {
    expect(laneMovePatches("P-3", laneMove("P-3", "DRAFT_NEW", "DRAFT_OLD", versions))).toEqual([
      { version: "DRAFT_NEW", patch: { remove_cards: ["P-3"] } },
      { version: "DRAFT_OLD", patch: { add_cards: ["P-3"] } },
    ]);
    expect(laneMovePatches("P-2", laneMove("P-2", "DRAFT_NEW", NO_VERSION_LANE, versions))).toEqual([{ version: "DRAFT_NEW", patch: { remove_cards: ["P-2"] } }]);
    expect(laneMovePatches("P-6", laneMove("P-6", NO_VERSION_LANE, "DRAFT_OLD", versions))).toEqual([{ version: "DRAFT_OLD", patch: { add_cards: ["P-6"] } }]);
  });

  it("maps a drop in a lane cell to the column index", () => {
    const col = [card("A", "r", 0), card("B", "r", 1), card("C", "r", 2), card("D", "r", 3)];
    const cell = [col[1]!, col[3]!];
    expect(columnIndex(cell, col, 0)).toBe(1);
    expect(columnIndex(cell, col, 1)).toBe(3);
    expect(columnIndex(cell, col, 2)).toBe(4);
    expect(columnIndex([], col, 0)).toBe(4);
  });

  it("keeps the rows mode and the collapsed lanes", () => {
    expect(parseRows("version", null)).toBe("version");
    expect(parseRows(undefined, "version")).toBe("version");
    expect(parseRows("none", "version")).toBe("none");
    expect(parseRows(undefined, null)).toBe("none");
    expect(readLaneState("{bad")).toEqual({ rows: "none", collapsed: [] });
    const s = readLaneState(JSON.stringify({ rows: "version", collapsed: ["A", 3] }));
    expect(s).toEqual({ rows: "version", collapsed: ["A"] });
    expect(toggleCollapsed(s, "A").collapsed).toEqual([]);
    expect(toggleCollapsed(s, "B").collapsed).toEqual(["A", "B"]);
  });

  it("lists the versions of the toolbar menu and finds a version by ID or name", () => {
    expect(menuVersions(versions, false).archived).toEqual([]);
    expect(menuVersions(versions, true).archived.map((x) => x.id)).toEqual(["ARCH"]);
    expect(findVersion(versions, "review")?.id).toBe("REVIEW");
    expect(findVersion(versions, "REL")?.id).toBe("REL");
    expect(findVersion(versions, undefined)).toBeUndefined();
  });
});
