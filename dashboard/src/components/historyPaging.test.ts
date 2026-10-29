import { describe, expect, it } from "vitest";
import type { AgentEvent } from "../api/types";
import { historyQuery, livePlace, PAGE_LIMIT } from "./historyPaging";

const ev = (seq: number): AgentEvent => ({ seq, id: `E${seq}`, at: "2026-10-29T00:00:00Z", kind: "status", actor: "system", summary: "" }) as AgentEvent;

describe("historyQuery", () => {
  it("is oldest first from `since` by default", () => {
    expect(historyQuery("40", false)).toEqual({ since: "40", cursor: undefined, limit: PAGE_LIMIT });
    expect(historyQuery(" ", false, "57")).toEqual({ since: 0, cursor: "57", limit: PAGE_LIMIT });
  });

  it("asks for newest first with `since` as the lower bound", () => {
    expect(historyQuery("40", true)).toEqual({ since: "40", cursor: undefined, limit: PAGE_LIMIT, order: "desc" });
    expect(historyQuery("", true, "57")).toEqual({ since: 0, cursor: "57", limit: PAGE_LIMIT, order: "desc" });
  });
});

describe("livePlace", () => {
  it("puts a newer event on top of a newest-first list, older pages or not", () => {
    expect(livePlace([ev(9), ev(8)], ev(10), true, true)).toBe("start");
    expect(livePlace([ev(9), ev(8)], ev(10), true, false)).toBe("start");
    expect(livePlace([], ev(1), true, false)).toBe("start");
  });

  it("drops an event that is not newer than the top", () => {
    expect(livePlace([ev(9), ev(8)], ev(9), true, false)).toBeNull();
    expect(livePlace([ev(9), ev(8)], ev(5), true, false)).toBeNull();
  });

  it("appends to an oldest-first list only after the last page", () => {
    expect(livePlace([ev(8), ev(9)], ev(10), false, false)).toBe("end");
    expect(livePlace([ev(8), ev(9)], ev(10), false, true)).toBeNull();
    expect(livePlace([ev(8), ev(9)], ev(9), false, false)).toBeNull();
    expect(livePlace([], ev(1), false, false)).toBe("end");
  });
});
