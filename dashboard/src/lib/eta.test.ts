import { describe, expect, it } from "vitest";
import type { Card } from "../api/types";
import { countdown, doneSummary, etaStopped, parseDuration, resolveEta, span, tickInterval } from "./eta";

const NOW = Date.parse("2026-10-30T10:00:00Z");
const at = (min: number) => new Date(NOW + min * 60_000).toISOString();
const card = (extra: Partial<Card> = {}): Card => ({
  id: "TER-1",
  project: "p",
  column: "in_progress",
  position: 0,
  title: "t",
  body: "",
  labels: [],
  depends_on: [],
  blocked_by: [],
  comments: [],
  links: [],
  created_by: "operator",
  created_at: at(-60),
  updated_at: at(-60),
  archived: false,
  ...extra,
});

it("stops ETA by role rather than a renamed column's name", () => {
  expect(etaStopped(card({ column: "shipped", column_role: "accepted" }))).toBe(true);
  expect(etaStopped(card({ column: "accepted", column_role: "working" }))).toBe(false);
  expect(etaStopped(card({ column: "accepted" }))).toBe(true);
});

describe("parseDuration", () => {
  it("takes one or more parts", () => {
    expect(parseDuration("45m")).toBe(45 * 60_000);
    expect(parseDuration("2h30m")).toBe(150 * 60_000);
    expect(parseDuration("1d")).toBe(86_400_000);
    expect(parseDuration(" 1H ")).toBe(3_600_000);
    for (const bad of ["", "45", "2h30", "1w", "0m", "1.5h", "-1h", "1 h"]) expect(parseDuration(bad), bad).toBeNull();
  });
});

describe("resolveEta", () => {
  it("is in the future and at most 30 days away", () => {
    expect(resolveEta("2h", NOW)).toEqual({ at: NOW + 2 * 3_600_000 });
    expect(resolveEta(at(90), NOW)).toEqual({ at: NOW + 90 * 60_000 });
    expect(resolveEta("30d", NOW)).toHaveProperty("at");
    expect((resolveEta("31d", NOW) as { error: string }).error).toContain("30 days");
    expect((resolveEta(at(-1), NOW) as { error: string }).error).toContain("past");
    expect((resolveEta("soon", NOW) as { error: string }).error).toContain("RFC 3339");
  });
});

describe("countdown", () => {
  it("shows seconds under an hour, hours and minutes above", () => {
    expect(span(45_000)).toBe("45s");
    expect(span(34 * 60_000 + 5_000)).toBe("34m 05s");
    expect(span(72 * 60_000)).toBe("1h 12m");
    expect(span(51 * 3_600_000)).toBe("2d 3h");
    expect(countdown(card({ eta_at: at(72) }), NOW)?.label).toBe("1h 12m left");
    expect(countdown(card({ eta_at: at(34.5) }), NOW)?.label).toBe("34m 30s left");
  });

  it("says overdue in minutes", () => {
    const c = countdown(card({ eta_at: at(-12.5) }), NOW)!;
    expect(c.label).toBe("overdue 12m");
    expect(c.overdue).toBe(true);
    expect(c.title).toContain("overdue");
  });

  it("gives the progress and who set it", () => {
    const c = countdown(card({ eta_at: at(30), eta_set_at: at(-90), eta_set_by_name: "p/w1" }), NOW)!;
    expect(c.progress).toBeCloseTo(0.75);
    expect(c.title).toContain("set by p/w1");
    expect(countdown(card({ eta_at: at(-30), eta_set_at: at(-90) }), NOW)!.progress).toBe(1);
    expect(countdown(card({ eta_at: at(30) }), NOW)!.progress).toBeNull();
  });

  it("stops on accepted, done, and archived cards, and without an ETA", () => {
    expect(countdown(card({ eta_at: at(30), column: "accepted" }), NOW)).toBeNull();
    expect(countdown(card({ eta_at: at(30), column: "done" }), NOW)).toBeNull();
    expect(countdown(card({ eta_at: at(30), archived: true }), NOW)).toBeNull();
    expect(countdown(card(), NOW)).toBeNull();
  });
});

describe("doneSummary", () => {
  it("compares the real time with the estimate", () => {
    expect(doneSummary(card({ column: "done", eta_set_at: at(-180), eta_at: at(-60), done_at: at(-90) }))).toBe("done in 1h 30m (estimated 2h 00m)");
    expect(doneSummary(card({ column: "done", eta_set_at: at(-180), eta_at: at(-60) }))).toBeNull();
  });
});

describe("tickInterval", () => {
  it("is 1 s while a countdown is under one hour", () => {
    expect(tickInterval([card({ eta_at: at(300) }), card({ eta_at: at(20) })], NOW)).toBe(1000);
  });
  it("is 30 s otherwise, or less when a card comes under one hour sooner", () => {
    expect(tickInterval([card({ eta_at: at(300) })], NOW)).toBe(30_000);
    expect(tickInterval([card({ eta_at: at(-5) })], NOW)).toBe(30_000);
    expect(tickInterval([card({ eta_at: at(60.25) })], NOW)).toBe(15_001);
  });
  it("is null with no countdown: no timer", () => {
    expect(tickInterval([card(), card({ eta_at: at(20), column: "done" })], NOW)).toBeNull();
  });
});
