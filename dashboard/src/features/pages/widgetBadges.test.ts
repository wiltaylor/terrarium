import { describe, expect, it } from "vitest";
import { pageWidgetBadge } from "./widgetBadges";

const counts = { blocked_cards: 3, questions: { open: 2, urgent: 0, critical: 0 } };

describe("pageWidgetBadge", () => {
  it("deduplicates repeated instances of each source within a page", () => {
    expect(pageWidgetBadge(["agents", "agents", "board", "board", "dashboard"], 1, counts)).toEqual({
      n: 6, cls: "nav-count warn",
      text: "1 agent blocked or in error, 3 blocked cards, 2 open questions",
    });
  });

  it("counts the same source independently on different pages", () => {
    expect(pageWidgetBadge(["board"], 0, counts)?.n).toBe(3);
    expect(pageWidgetBadge(["board", "board"], 0, counts)?.n).toBe(3);
    expect(pageWidgetBadge([], 0, counts)).toBeNull();
  });

  it("follows the supplied location after removal or movement", () => {
    expect(pageWidgetBadge(["dashboard"], 0, counts)?.text).toBe("2 open questions");
    expect(pageWidgetBadge([], 0, counts)).toBeNull();
    expect(pageWidgetBadge(["dashboard", "dashboard"], 0, counts)?.n).toBe(2);
  });

  it("handles missing counts without inventing a badge", () => {
    expect(pageWidgetBadge(["board", "dashboard"], 0, null)).toBeNull();
    expect(pageWidgetBadge(["agents"], 2, null)?.n).toBe(2);
  });

  it("preserves question urgency in accessible text and severity", () => {
    expect(pageWidgetBadge(["dashboard"], 0, {
      blocked_cards: 0, questions: { open: 4, urgent: 2, critical: 1 },
    })).toEqual({ n: 4, cls: "nav-count warn", text: "4 open questions, 1 critical" });
  });
});
