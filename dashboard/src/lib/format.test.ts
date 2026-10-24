import { describe, expect, it } from "vitest";
import { forceLayout } from "./force";
import { ago, bytes, duration, sparkPath, timeUntil, usageLevel } from "./format";

describe("format", () => {
  it("formats durations", () => {
    expect(duration(0)).toBe("now");
    expect(duration(5_000)).toBe("5s");
    expect(duration(3 * 60_000)).toBe("3m");
    expect(duration(97 * 60_000)).toBe("1h 37m");
    expect(duration((3 * 24 + 2) * 3600_000)).toBe("3d 2h");
  });
  it("formats relative times", () => {
    const now = Date.parse("2026-10-24T12:00:00Z");
    expect(timeUntil("2026-10-24T13:30:00Z", now)).toBe("1h 30m");
    expect(ago("2026-10-24T11:58:00Z", now)).toBe("2m ago");
    expect(ago(undefined, now)).toBe("never");
  });
  it("grades usage against the alert threshold", () => {
    expect(usageLevel(10, 80)).toBe("ok");
    expect(usageLevel(80, 80)).toBe("warn");
    expect(usageLevel(100, 80)).toBe("full");
  });
  it("formats bytes and sparklines", () => {
    expect(bytes(2048)).toBe("2.0 KB");
    expect(sparkPath([0, 100], 10, 10)).toBe("M0.0,10.0 L10.0,0.0");
    expect(sparkPath([], 10, 10)).toBe("");
  });
});

describe("forceLayout", () => {
  it("is deterministic, stays in bounds, and keeps pinned nodes", () => {
    const ids = ["a", "b", "c", "d"];
    const edges = [{ from: "a", to: "b" }, { from: "a", to: "c" }, { from: "c", to: "d" }];
    const opts = { width: 400, height: 300 };
    const one = forceLayout(ids, edges, opts, { a: { x: 200, y: 150 } });
    const two = forceLayout(ids, edges, opts, { a: { x: 200, y: 150 } });
    expect(one).toEqual(two);
    expect(one.a).toMatchObject({ x: 200, y: 150 });
    for (const n of Object.values(one)) {
      expect(n.x).toBeGreaterThanOrEqual(24);
      expect(n.x).toBeLessThanOrEqual(376);
      expect(n.y).toBeGreaterThanOrEqual(24);
      expect(n.y).toBeLessThanOrEqual(276);
    }
    // Linked nodes end up closer than the farthest pair.
    const dist = (p: string, q: string) => Math.hypot(one[p]!.x - one[q]!.x, one[p]!.y - one[q]!.y);
    expect(dist("c", "d")).toBeLessThan(dist("b", "d"));
  });
});
