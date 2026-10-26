import { describe, expect, it } from "vitest";
import { instantOf, nextRuns, parseCron, previewCron, validTimeZone, wallOf } from "./cron";

const at = (iso: string) => Date.parse(iso);
const runs = (expr: string, from: string, tz = "UTC", n = 3) => {
  const p = parseCron(expr);
  if (!p.ok) throw new Error(p.error);
  return nextRuns(p.cron, at(from), tz, n).map((ms) => new Date(ms).toISOString());
};
const on = (t: boolean[]) => t.map((v, i) => (v ? i : -1)).filter((i) => i >= 0);

describe("parseCron", () => {
  it("reads stars, values, ranges, steps, and lists", () => {
    const p = parseCron("*/15 9-17/4 1,15 * 1-5");
    expect(p.ok).toBe(true);
    if (!p.ok) return;
    expect(on(p.cron.minute)).toEqual([0, 15, 30, 45]);
    expect(on(p.cron.hour)).toEqual([9, 13, 17]);
    expect(on(p.cron.dom)).toEqual([1, 15]);
    expect(on(p.cron.month)).toHaveLength(12);
    expect(on(p.cron.dow)).toEqual([1, 2, 3, 4, 5]);
    expect(p.cron.domStar).toBe(false);
  });

  it("reads month and day names, 7 as Sunday, and a/step from a to the end", () => {
    const p = parseCron("5/20 0 * jan,Jul SUN,7,sat");
    expect(p.ok).toBe(true);
    if (!p.ok) return;
    expect(on(p.cron.minute)).toEqual([5, 25, 45]);
    expect(on(p.cron.month)).toEqual([1, 7]);
    expect(on(p.cron.dow)).toEqual([0, 6]);
  });

  it("expands the macros", () => {
    const p = parseCron("@daily");
    expect(p.ok && on(p.cron.hour)).toEqual([0]);
    expect(parseCron("@HOURLY").ok).toBe(true);
    expect(parseCron("@reboot")).toEqual({ ok: false, error: expect.stringContaining("@reboot") });
  });

  it("says which field is bad and quotes the bad part", () => {
    expect(parseCron("")).toEqual({ ok: false, error: "the expression is empty" });
    expect(parseCron("* * *")).toEqual({ ok: false, error: expect.stringContaining("has 3") });
    expect(parseCron("61 * * * *")).toEqual({ ok: false, error: "minute: `61` is out of range 0-59" });
    expect(parseCron("0 24 * * *")).toEqual({ ok: false, error: "hour: `24` is out of range 0-23" });
    expect(parseCron("0 0 0 * *")).toEqual({ ok: false, error: "day of month: `0` is out of range 1-31" });
    expect(parseCron("*/0 * * * *")).toEqual({ ok: false, error: expect.stringContaining("step `0`") });
    expect(parseCron("0 5-2 * * *")).toEqual({ ok: false, error: expect.stringContaining("backwards") });
    expect(parseCron("0 0 * foo *")).toEqual({ ok: false, error: "month: `foo` is not a number or a name" });
    expect(parseCron("0 0 * * 1,")).toEqual({ ok: false, error: expect.stringContaining("day of week") });
  });
});

describe("nextRuns", () => {
  it("gives the next three runs of a step after a time, never the time itself", () => {
    expect(runs("*/15 * * * *", "2026-10-26T10:07:30Z")).toEqual(["2026-10-26T10:15:00.000Z", "2026-10-26T10:30:00.000Z", "2026-10-26T10:45:00.000Z"]);
    expect(runs("*/15 * * * *", "2026-10-26T10:15:00Z", "UTC", 1)).toEqual(["2026-10-26T10:30:00.000Z"]);
  });

  it("crosses days, months, and years", () => {
    expect(runs("0 9 * * *", "2026-12-31T10:00:00Z", "UTC", 2)).toEqual(["2027-01-01T09:00:00.000Z", "2027-01-02T09:00:00.000Z"]);
    expect(runs("30 6 1 * *", "2026-10-26T00:00:00Z", "UTC", 2)).toEqual(["2026-11-01T06:30:00.000Z", "2026-12-01T06:30:00.000Z"]);
  });

  it("takes weekdays only", () => {
    // 2026-10-30 is a Friday.
    expect(runs("0 9 * * 1-5", "2026-10-30T10:00:00Z", "UTC", 2)).toEqual(["2026-11-02T09:00:00.000Z", "2026-11-03T09:00:00.000Z"]);
  });

  it("matches either day field when both are restricted, as Vixie cron does", () => {
    // The 13th, or any Friday: Fri 2026-10-30, then Fri 2026-11-06, then Fri the 13th.
    expect(runs("0 0 13 * 5", "2026-10-29T00:00:00Z")).toEqual(["2026-10-30T00:00:00.000Z", "2026-11-06T00:00:00.000Z", "2026-11-13T00:00:00.000Z"]);
  });

  it("finds a leap day and gives up on a date that never comes", () => {
    expect(runs("0 0 29 2 *", "2026-01-01T00:00:00Z", "UTC", 1)).toEqual(["2028-02-29T00:00:00.000Z"]);
    expect(runs("0 0 30 2 *", "2026-01-01T00:00:00Z")).toEqual([]);
  });

  it("runs in the schedule's time zone, across a DST change", () => {
    // 09:00 in New York: EDT (-4) until 2026-11-01, then EST (-5).
    expect(runs("0 9 * * *", "2026-10-31T00:00:00Z", "America/New_York")).toEqual(["2026-10-31T13:00:00.000Z", "2026-11-01T14:00:00.000Z", "2026-11-02T14:00:00.000Z"]);
    expect(runs("0 9 * * *", "2026-10-26T00:00:00Z", "Europe/Berlin", 1)).toEqual(["2026-10-26T08:00:00.000Z"]);
  });

  it("skips a wall time that a DST gap removes", () => {
    // 2026-03-08 02:30 does not exist in New York.
    expect(runs("30 2 * * *", "2026-03-07T12:00:00Z", "America/New_York", 2)).toEqual(["2026-03-09T06:30:00.000Z", "2026-03-10T06:30:00.000Z"]);
  });
});

describe("time zones", () => {
  it("checks a zone name", () => {
    expect(validTimeZone("UTC")).toBe(true);
    expect(validTimeZone("Australia/Sydney")).toBe(true);
    expect(validTimeZone("Mars/Base")).toBe(false);
    expect(validTimeZone("")).toBe(false);
  });

  it("goes from an instant to the wall clock and back", () => {
    const t = at("2026-07-01T12:34:56Z");
    const wall = wallOf(t, "Australia/Sydney");
    expect(new Date(wall).toISOString()).toBe("2026-07-01T22:34:00.000Z");
    expect(instantOf(wall, "Australia/Sydney")).toBe(at("2026-07-01T12:34:00Z"));
  });
});

describe("previewCron", () => {
  it("gives runs, or the parse error, or a bad zone", () => {
    expect(previewCron("0 * * * *", "UTC", at("2026-10-26T10:30:00Z"), 2)).toEqual({ runs: [at("2026-10-26T11:00:00Z"), at("2026-10-26T12:00:00Z")] });
    expect(previewCron("0 * * *", "UTC", 0)).toEqual({ error: expect.stringContaining("5 fields") });
    expect(previewCron("0 * * * *", "Nowhere/City", 0)).toEqual({ error: "`Nowhere/City` is not a time zone" });
    expect(previewCron("0 0 31 2 *", "UTC", 0)).toEqual({ error: expect.stringContaining("does not fire") });
  });
});
