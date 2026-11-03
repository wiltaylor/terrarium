import { describe, expect, it } from "vitest";
import type { Pin } from "../../api/types";
import { activeAfterUnpin, dropPin, isPinned, pickActive, tabKeyTarget } from "./model";

const pin = (agent: string): Pin => ({ agent, project: "p", name: agent.toLowerCase(), pinned_at: "2026-11-03T09:00:00Z" });
const list = [pin("A"), pin("B"), pin("C")];

describe("pinned conversations: the open tab", () => {
  it("keeps the wanted tab while it is pinned, else opens the first pin", () => {
    expect(pickActive(list, "B")).toBe("B");
    expect(pickActive(list, "Z")).toBe("A");
    expect(pickActive(list, null)).toBe("A");
    expect(pickActive([], "A")).toBeNull();
  });

  it("opens the right neighbour, else the left one, after the unpin of the open tab", () => {
    expect(activeAfterUnpin(list, "B", "B")).toBe("C");
    expect(activeAfterUnpin(list, "C", "C")).toBe("B");
    expect(activeAfterUnpin([pin("A")], "A", "A")).toBeNull();
  });

  it("keeps the open tab when another tab goes", () => {
    expect(activeAfterUnpin(list, "A", "C")).toBe("A");
    expect(activeAfterUnpin(list, "Z", "A")).toBe("B");
  });

  it("drops a pin, and gives the same array when there is none", () => {
    expect(dropPin(list, "B").map((p) => p.agent)).toEqual(["A", "C"]);
    expect(dropPin(list, "Z")).toBe(list);
    expect(dropPin(list, undefined)).toBe(list);
    expect(isPinned(list, "C")).toBe(true);
    expect(isPinned(list, undefined)).toBe(false);
  });

  it("moves between the tabs with the arrow keys, Home, and End, and wraps", () => {
    expect(tabKeyTarget(list, "A", "ArrowRight")).toBe("B");
    expect(tabKeyTarget(list, "C", "ArrowRight")).toBe("A");
    expect(tabKeyTarget(list, "A", "ArrowLeft")).toBe("C");
    expect(tabKeyTarget(list, "B", "Home")).toBe("A");
    expect(tabKeyTarget(list, "A", "End")).toBe("C");
    expect(tabKeyTarget(list, "A", "x")).toBeNull();
    expect(tabKeyTarget([], null, "ArrowRight")).toBeNull();
  });
});
