import { describe, expect, it } from "vitest";
import type { ActiveCard } from "../../api/types";
import { blockedFor, blockedSummary, reasonLine } from "./blocked";

describe("blocked helpers", () => {
  it("cuts the reason to its first line", () => {
    expect(reasonLine({ blocked_reason: "  need a target\nsecond line" })).toBe("need a target");
    expect(reasonLine({ blocked_reason: "one\r\ntwo" })).toBe("one");
    expect(reasonLine({})).toBe("");
  });

  it("gives how long a card has been blocked", () => {
    const now = Date.parse("2026-11-06T12:00:00Z");
    expect(blockedFor({ blocked_since: "2026-11-06T09:55:00Z" }, now)).toBe("2h 05m");
    expect(blockedFor({ blocked_since: "2026-11-03T12:00:00Z" }, now)).toBe("3d 0h");
    expect(blockedFor({}, now)).toBe("");
    expect(blockedFor({ blocked_since: "bad" }, now)).toBe("");
  });

  it("counts the cards", () => {
    expect(blockedSummary([{} as ActiveCard])).toBe("1 card");
    expect(blockedSummary([{} as ActiveCard, {} as ActiveCard])).toBe("2 cards");
  });
});
