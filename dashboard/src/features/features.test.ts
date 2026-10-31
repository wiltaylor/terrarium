import { describe, expect, it } from "vitest";
import { seed } from "../api/mock/data";
import { buildTree } from "./wiki/tree";

const world = seed(Date.parse("2026-10-24T12:00:00Z"));

describe("wiki tree", () => {
  it("nests by path with index first", () => {
    const tree = buildTree(world.pages.filter((p) => p.project === "terrarium"));
    expect(tree.map((n) => n.path)).toEqual(["index", "designs", "reports", "security"]);
    expect(tree[1]!.children[0]!.page?.title).toBe("Runner v2");
  });
});
