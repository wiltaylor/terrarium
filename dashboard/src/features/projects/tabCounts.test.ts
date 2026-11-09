import { describe, expect, it } from "vitest";
import { blockedAgents, changesCounts, tabBadge } from "./tabCounts";

const counts = (blocked_cards: number, open: number, urgent = 0, critical = 0) => ({ blocked_cards, questions: { open, urgent, critical } });

describe("tabBadge", () => {
  it("gives no badge for a zero count or a tab without a count", () => {
    expect(tabBadge("agents", 0, counts(0, 0))).toBeNull();
    expect(tabBadge("board", 0, counts(0, 0))).toBeNull();
    expect(tabBadge("dashboard", 0, counts(0, 0))).toBeNull();
    expect(tabBadge("tests", 4, counts(4, 4))).toBeNull();
    expect(tabBadge("board", 0, null)).toBeNull();
    expect(tabBadge("dashboard", 0, { blocked_cards: 0, questions: null })).toBeNull();
  });

  it("counts the blocked agents and the blocked cards in words", () => {
    expect(tabBadge("agents", 1, null)).toEqual({ n: 1, cls: "nav-count warn", text: "1 agent blocked or in error" });
    expect(tabBadge("board", 0, counts(3, 0))).toEqual({ n: 3, cls: "nav-count warn", text: "3 blocked cards" });
  });

  it("marks urgent and critical questions in the words and the colour", () => {
    expect(tabBadge("dashboard", 0, counts(0, 1))).toEqual({ n: 1, cls: "nav-count info", text: "1 open question" });
    expect(tabBadge("dashboard", 0, counts(0, 2, 1))).toEqual({ n: 2, cls: "nav-count warn", text: "2 open questions, 1 urgent" });
    expect(tabBadge("dashboard", 0, counts(0, 4, 2, 1))).toEqual({ n: 4, cls: "nav-count warn", text: "4 open questions, 1 critical" });
  });
});

describe("blockedAgents and changesCounts", () => {
  it("counts blocked and error agents only", () => {
    expect(blockedAgents([{ status: "blocked" }, { status: "error" }, { status: "idle" }, { status: "working" }])).toBe(2);
  });

  it("reacts to card and question events of the project only", () => {
    expect(changesCounts({ kind: "card", project: "p" }, "p")).toBe(true);
    expect(changesCounts({ kind: "question", project: "p" }, "p")).toBe(true);
    expect(changesCounts({ kind: "card", project: "q" }, "p")).toBe(false);
    expect(changesCounts({ kind: "status", project: "p" }, "p")).toBe(false);
  });
});
