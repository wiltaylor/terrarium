import { describe, expect, it } from "vitest";
import type { DashboardBlock, Monitor, MonitorResult } from "../../api/types";
import { blockFor, blockState, columnsOf, isStale, kindsFor, layoutProblems, linePath, moveBlock, nearestIndex, newBlockId, niceTicks, noteBlock, numericPoints, ownerText, rangeOf, stateCounts, unitOf, valueText, yScale } from "./model";

const NOW = Date.parse("2026-11-02T12:00:00Z");
const result = (over: Partial<MonitorResult> = {}): MonitorResult => ({ id: 1, monitor: "m", at: new Date(NOW - 10_000).toISOString(), status: "ok", duration_ms: 5, ...over });
const mon = (over: Partial<Monitor> = {}): Monitor =>
  ({
    id: "m1",
    project: "demo",
    name: "load",
    kind: "number",
    target: "box",
    script: "x",
    schedule: { kind: "interval", every: "30s" },
    description: null,
    timeout_secs: 30,
    unit: null,
    warn: null,
    crit: null,
    direction: "above",
    enabled: true,
    retention_days: 7,
    period_secs: 30,
    created_by: "operator",
    run_as: "operator",
    created_at: "",
    updated_at: "",
    next_run_at: null,
    skipped: 0,
    last: result(),
    running: false,
    ...over,
  }) as Monitor;

describe("block state", () => {
  it("shows the status, or off, pending, stale", () => {
    expect(blockState(mon(), NOW)).toBe("ok");
    expect(blockState(mon({ last: result({ status: "crit" }) }), NOW)).toBe("crit");
    expect(blockState(mon({ enabled: false }), NOW)).toBe("off");
    expect(blockState(mon({ last: null }), NOW)).toBe("pending");
  });

  it("is stale after three periods and the timeout with no result", () => {
    // 3 × 30 s + 30 s = 120 s.
    const at = (ms: number) => mon({ last: result({ at: new Date(NOW - ms).toISOString() }) });
    expect(isStale(at(119_000), NOW)).toBe(false);
    expect(isStale(at(121_000), NOW)).toBe(true);
    expect(blockState(at(121_000), NOW)).toBe("stale");
    expect(isStale(mon({ last: null }), NOW)).toBe(false);
  });

  it("counts the states", () => {
    const c = stateCounts([mon(), mon({ enabled: false }), mon({ last: result({ status: "warn" }) })], NOW);
    expect(c).toMatchObject({ ok: 1, off: 1, warn: 1, crit: 0 });
  });
});

describe("values", () => {
  it("picks the decimals by size, or as asked", () => {
    expect(valueText(0.4234)).toBe("0.42");
    expect(valueText(42.25)).toBe("42.3");
    expect(valueText(420.6)).toBe("421");
    expect(valueText(3)).toBe("3");
    expect(valueText(3, 2)).toBe("3.00");
    expect(valueText(1284)).toBe("1,284");
    expect(valueText(12_900)).toBe("12.9K");
    expect(valueText(-5.5)).toBe("-5.5");
    expect(valueText(undefined)).toBe("–");
    expect(valueText(NaN)).toBe("–");
  });

  it("puts the unit after the value", () => {
    expect(valueText(85.5, 1, "%")).toBe("85.5%");
    expect(valueText(120, 0, "ms")).toBe("120 ms");
    expect(unitOf(mon({ unit: "%" }))).toBe("%");
    expect(unitOf(mon({ unit: "%", last: result({ json: { unit: "GB" } }) }))).toBe("GB");
  });
});

describe("layout", () => {
  const monitors = [mon(), mon({ id: "s1", kind: "status", name: "svc" }), mon({ id: "j1", kind: "json", name: "procs" })];

  it("gives each monitor kind its block kinds, the default first", () => {
    expect(kindsFor("number")[0]).toBe("stat");
    expect(kindsFor("status")).not.toContain("chart");
    expect(kindsFor("json")).toContain("table");
    expect(kindsFor("text")).not.toContain("stat");
    expect(blockFor(monitors[2]!, []).kind).toBe("stat");
    expect(blockFor(mon({ kind: "text" }), []).size).toBe("m");
  });

  it("makes new IDs that are free", () => {
    expect(newBlockId([])).toBe("b1");
    expect(newBlockId([{ id: "b2" }])).toBe("b3");
    expect(newBlockId([{ id: "b2" }, { id: "b3" }])).toBe("b4");
    expect(noteBlock([]).kind).toBe("note");
  });

  it("finds the problems the daemon refuses", () => {
    const ok: DashboardBlock[] = [noteBlock([]), { id: "a", kind: "chart", monitor: "m1", size: "m", options: {} }, { id: "c", kind: "table", monitor: "j1", size: "l", options: {} }];
    expect(layoutProblems(ok, monitors)).toEqual([]);
    const bad: DashboardBlock[] = [
      { id: "a", kind: "stat", monitor: "s1", size: "s", options: {} },
      { id: "a", kind: "note", text: " ", size: "s", options: {} },
      { id: "c", kind: "chart", monitor: "gone", size: "s", options: {} },
    ];
    const p = layoutProblems(bad, monitors);
    expect(p).toHaveLength(4);
    expect(p[0]).toContain("does not fit the status monitor svc");
    expect(p[1]).toContain("used twice");
    expect(p[2]).toContain("needs text");
    expect(p[3]).toContain("pick a monitor");
  });

  it("moves blocks up, down, and by drag", () => {
    expect(moveBlock(["a", "b", "c"], 0, 1)).toEqual(["b", "a", "c"]);
    expect(moveBlock(["a", "b", "c"], 2, 0)).toEqual(["c", "a", "b"]);
    expect(moveBlock(["a", "b", "c"], 0, -1)).toEqual(["a", "b", "c"]);
    expect(moveBlock(["a", "b", "c"], 1, 9)).toEqual(["a", "c", "b"]);
  });

  it("gives the chart range, default 24 h", () => {
    expect(rangeOf({ options: {} }).id).toBe("24h");
    expect(rangeOf({ options: { range: "7d" } }).bucket).toBe(3600);
  });
});

describe("tables", () => {
  it("takes the columns in order of first use, at most 20", () => {
    expect(columnsOf([{ pid: 1, cmd: "a" }, { cpu: 2, pid: 3 }])).toEqual(["pid", "cmd", "cpu"]);
    const wide = Object.fromEntries(Array.from({ length: 25 }, (_, i) => [`c${i}`, i]));
    expect(columnsOf([wide])).toHaveLength(20);
  });
});

describe("charts", () => {
  it("makes clean ticks", () => {
    expect(niceTicks(0, 100)).toEqual([0, 50, 100]);
    expect(niceTicks(0.1, 0.9)).toEqual([0, 0.5, 1]);
    expect(niceTicks(3, 3).length).toBeGreaterThan(1);
  });

  it("scales to the data, with thresholds near it and zero when close", () => {
    const pts = [{ value: 60 }, { value: 70 }];
    const s = yScale(pts, [80, 90]);
    expect(s.lo).toBeLessThanOrEqual(60);
    expect(s.hi).toBeGreaterThanOrEqual(90);
    // A threshold far from the data stays out.
    expect(yScale([{ value: 1 }, { value: 2 }], [1000]).hi).toBeLessThan(100);
    // Zero is in when the data is near it.
    expect(yScale([{ value: 2 }, { value: 10 }]).lo).toBe(0);
    expect(yScale([]).ticks).toEqual([0, 1]);
  });

  it("breaks the line at a gap, and finds the nearest point", () => {
    const pts = numericPoints([
      { at: "2026-11-02T10:00:00Z", value: 1, status: "ok" },
      { at: "2026-11-02T10:01:00Z", value: 2, status: "ok" },
      { at: "2026-11-02T10:02:00Z", status: "unknown" },
      { at: "2026-11-02T11:00:00Z", value: 3, status: "ok" },
    ]);
    expect(pts).toHaveLength(3);
    const since = pts[0]!.at;
    const d = linePath(pts, since, since + 3_600_000, 0, 4, 100, 40, 120_000);
    expect(d.match(/M/g)).toHaveLength(2);
    expect(d).toContain("l0.1,0");
    expect(nearestIndex(pts, since + 50_000)).toBe(1);
    expect(nearestIndex([], 0)).toBe(-1);
  });
});

describe("table cells", () => {
  it("keeps integers as they are and aligns number columns", async () => {
    const { cellText, numericColumns } = await import("./model");
    expect(cellText(1074)).toBe("1074");
    expect(cellText(24.456)).toBe("24.5");
    expect(cellText(null)).toBe("");
    expect([...numericColumns([{ pid: 1, cmd: "a" }, { pid: 2, cmd: "b" }], ["pid", "cmd"])]).toEqual(["pid"]);
  });
});

describe("ownerText", () => {
  it("names the owner, or the actor a run acts as", () => {
    expect(ownerText({ owner: "project", run_as: "agent:01X" })).toBe("owned by the project");
    expect(ownerText({ owner: "system", run_as: "operator" })).toBe("owned by the system");
    expect(ownerText({ owner: "actor", run_as: "agent:01X" })).toBe("runs as agent:01X");
    expect(ownerText({ run_as: "operator" })).toBe("runs as operator");
  });
});
