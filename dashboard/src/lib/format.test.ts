import { describe, expect, it } from "vitest";
import { forceLayout } from "./force";
import { ago, balanceText, bytes, windowValue, duration, orderWindows, primaryWindow, sparkPath, subscriptionTitle, timeUntil, usageLevel, windowLabel, windowSeconds } from "./format";

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

describe("balanceText", () => {
  it("shows dollars, then other currencies by code", () => {
    expect(balanceText({ total: 60, remaining: 10.647, currency: "USD" })).toBe("$10.65 left of $60.00");
    expect(balanceText({ total: 5, remaining: 0, currency: "EUR" })).toBe("0.00 EUR left of 5.00 EUR");
  });
});

describe("windowValue", () => {
  it("shows money left for a balance, else the percentage", () => {
    expect(windowValue({ used_pct: 82.3, balance: { remaining: 10.647, currency: "USD" } })).toBe("$10.65 left");
    expect(windowValue({ used_pct: 42.4 })).toBe("42%");
  });

  it("labels the windows clearly", () => {
    expect(windowLabel("5h")).toBe("5-hour");
    expect(windowLabel("7d")).toBe("Weekly");
    expect(windowLabel("7d_fable")).toBe("Weekly, Fable");
    expect(windowLabel("7d_claude_opus")).toBe("Weekly, Claude opus");
    expect(windowLabel("7d_oauth_apps")).toBe("Weekly, OAuth apps");
    expect(windowLabel("30d")).toBe("30-day");
    expect(windowLabel("24h")).toBe("24-hour");
    expect(windowLabel("extra")).toBe("Extra usage");
    expect(windowLabel("monthly")).toBe("Monthly");
    expect(windowLabel("premium_interactions")).toBe("premium interactions");
    expect(windowSeconds("7d_fable")).toBe(604_800);
    expect(windowSeconds("credits")).toBeUndefined();
  });

  it("puts the most relevant window first, with no 5-hour window on an Ultra plan", () => {
    const names = (ws: { window: string }[]) => orderWindows(ws).map((w) => w.window);
    // The API order is by name.
    expect(names([{ window: "5h" }, { window: "7d" }, { window: "7d_fable" }])).toEqual(["5h", "7d", "7d_fable"]);
    expect(names([{ window: "7d_fable" }, { window: "extra" }, { window: "7d" }, { window: "30d" }, { window: "5h" }])).toEqual(["5h", "7d", "7d_fable", "30d", "extra"]);
    // Ultra: no 5-hour window; the weekly one leads.
    expect(primaryWindow([{ window: "7d_fable" }, { window: "7d" }])?.window).toBe("7d");
    expect(primaryWindow([{ window: "credits" }, { window: "monthly" }])?.window).toBe("monthly");
    expect(primaryWindow([])).toBeUndefined();
  });

  it("titles a subscription by its display name", () => {
    expect(subscriptionTitle({ name: "anthropic-max-20x", display_name: "Claude Ultra", plan: "Max 20x" })).toBe("Claude Ultra");
    expect(subscriptionTitle({ name: "anthropic-max-20x", plan: "Max 20x" })).toBe("Max 20x");
    expect(subscriptionTitle({ name: "x" })).toBe("x");
  });
});
