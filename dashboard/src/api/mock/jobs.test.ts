// The job-queue stats of the mock answer in the daemon's shapes
// (docs/api.md §35.4) and fold the window the way the daemon's store does.

import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import { jobStats, seedJobs, sinceMs, type MockJobRow } from "./jobs";

const NOW = Date.parse("2026-11-08T12:00:00Z");
const DAY = 86_400_000;
const HOUR = 3_600_000;

describe("mock job stats", () => {
  it("counts a first attempt, and a rerun that clears it", () => {
    const rows: MockJobRow[] = [
      { at: NOW - HOUR, kind: "merge", state: "passed", attempt: 1 },
      { at: NOW - 2 * HOUR, kind: "merge", state: "failed", attempt: 1, step: "test", targets: ["-p terrariumd --test jobs"] },
      { at: NOW - 2 * HOUR, kind: "merge", state: "passed", attempt: 2 },
    ];
    const s = jobStats("p", rows, NOW - DAY);
    expect(s.jobs).toBe(3);
    expect(s.first_attempt).toEqual({ ended: 2, passed: 1, failed: 1, pass_rate: 0.5 });
    expect(s.merge).toEqual({ ended: 2, passed: 1, failed: 1, pass_rate: 0.5 });
    expect(s.validate).toEqual({ ended: 0, passed: 0, failed: 0, pass_rate: null });
    expect(s.reruns).toEqual({ ended: 1, passed: 1, failed: 0, pass_rate: 1 });
    expect(s.failed_targets).toEqual([{ target: "-p terrariumd --test jobs", failed: 1, first_attempt: 1 }]);
    expect(s.failed_steps).toEqual([{ step: "test", failed: 1 }]);
  });

  it("keeps conflicts, errors, split halves, and open jobs out of the rate", () => {
    const rows: MockJobRow[] = [
      { at: NOW - HOUR, kind: "merge", state: "passed", attempt: 1 },
      { at: NOW - 2 * HOUR, kind: "merge", state: "failed", attempt: 1, outcome: "conflict" },
      { at: NOW - 3 * HOUR, kind: "merge", state: "failed", attempt: 1, outcome: "error" },
      { at: NOW - 4 * HOUR, kind: "merge", state: "failed", attempt: 1, parent: true, step: "test", targets: ["-p core --lib"] },
      { at: NOW - 4 * HOUR, kind: "merge", state: "failed", attempt: 1, parent: true, step: "lint" },
      { at: NOW - 5 * HOUR, kind: "merge", state: "queued", attempt: 1 },
    ];
    const s = jobStats("p", rows, NOW - DAY);
    expect(s.jobs).toBe(6);
    expect(s.first_attempt).toEqual({ ended: 1, passed: 1, failed: 0, pass_rate: 1 });
    expect(s.conflicts).toBe(1);
    expect(s.errors).toBe(1);
    expect(s.splits).toBe(2);
    expect(s.open).toBe(1);
    // A split half still names its failed target and step, but not as a first attempt.
    expect(s.failed_targets).toEqual([{ target: "-p core --lib", failed: 1, first_attempt: 0 }]);
    expect(s.failed_steps[0]).toEqual({ step: "lint", failed: 1 });
  });

  it("counts a failed test step with no target as untargeted", () => {
    const rows: MockJobRow[] = [{ at: NOW - HOUR, kind: "merge", state: "failed", attempt: 1, step: "test" }];
    const s = jobStats("p", rows, NOW - DAY);
    expect(s.untargeted_test_fails).toBe(1);
    expect(s.failed_targets).toEqual([]);
  });

  it("keeps a rerun in the window of its first try, and drops what ended before it", () => {
    const rows: MockJobRow[] = [
      { at: NOW - 3 * DAY, kind: "merge", state: "failed", attempt: 1, step: "test", targets: ["t"] },
      { at: NOW - 3 * DAY, kind: "merge", state: "passed", attempt: 2 },
      { at: NOW - 8 * DAY, kind: "merge", state: "passed", attempt: 1 },
    ];
    const week = jobStats("p", rows, NOW - 7 * DAY);
    expect(week.jobs).toBe(2);
    const two = jobStats("p", rows, NOW - 2 * DAY);
    expect(two.jobs).toBe(0);
    expect(two.first_attempt.pass_rate).toBeNull();
  });

  it("sorts the failed targets and steps most first, then by name", () => {
    const rows: MockJobRow[] = [
      { at: NOW - HOUR, kind: "merge", state: "failed", attempt: 1, step: "test", targets: ["b", "a"] },
      { at: NOW - 2 * HOUR, kind: "merge", state: "failed", attempt: 1, step: "lint" },
      { at: NOW - 3 * HOUR, kind: "merge", state: "failed", attempt: 1, step: "test", targets: ["a"] },
    ];
    const s = jobStats("p", rows, NOW - DAY);
    expect(s.failed_targets.map((t) => t.target)).toEqual(["a", "b"]);
    expect(s.failed_targets[0]).toMatchObject({ failed: 2, first_attempt: 2 });
    expect(s.failed_steps.map((x) => x.step)).toEqual(["test", "lint"]);
  });

  it("parses since as a duration, an RFC 3339 time, or the 7d default", () => {
    expect(sinceMs("24h", NOW)).toBe(NOW - DAY);
    expect(sinceMs(new Date(NOW - 3 * DAY).toISOString(), NOW)).toBe(NOW - 3 * DAY);
    expect(sinceMs(undefined, NOW)).toBe(NOW - 7 * DAY);
    expect(sinceMs("", NOW)).toBe(NOW - 7 * DAY);
    expect(sinceMs(" 7d ", NOW)).toBe(NOW - 7 * DAY);
    expect(sinceMs("last week", NOW)).toMatch(/bad since/);
  });

  it("answers any window of the seeded month, and hides nothing the daemon counts", () => {
    const rows = seedJobs(NOW);
    const day = jobStats("terrarium", rows, NOW - DAY);
    expect(day.jobs).toBeGreaterThan(3);
    expect(day.first_attempt.ended).toBeGreaterThanOrEqual(3);
    expect(day.open).toBe(1);
    expect(day.merge.ended).toBe(day.first_attempt.ended);
    const week = jobStats("terrarium", rows, NOW - 7 * DAY);
    expect(week.first_attempt.ended).toBeGreaterThan(day.first_attempt.ended);
    expect(week.conflicts).toBe(1);
    expect(week.errors).toBe(1);
    const month = jobStats("terrarium", rows, NOW - 30 * DAY);
    expect(month.splits).toBe(2);
    expect(month.untargeted_test_fails).toBe(1);
    expect(month.reruns.passed).toBeGreaterThan(0);
    expect(month.failed_targets[0]!.target).toBe("-p terrariumd --test jobs");
    expect(month.since).toBe(new Date(NOW - 30 * DAY).toISOString());
  });

  it("serves the endpoint for a known project and nothing for one without a queue", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, autoLoginMs: 0, now: () => NOW });
    const s = await api.jobs.stats("terrarium", "7d");
    expect(s.project).toBe("terrarium");
    expect(s.jobs).toBeGreaterThan(0);
    expect(s.first_attempt.pass_rate).toBeGreaterThanOrEqual(0);
    expect(s.first_attempt.pass_rate).toBeLessThanOrEqual(1);
    // webshop has no job queue: the daemon answers an empty window, not 404.
    const none = await api.jobs.stats("webshop");
    expect(none).toMatchObject({ project: "webshop", jobs: 0, open: 0 });
    expect(none.first_attempt.pass_rate).toBeNull();
  });

  it("refuses an unknown project and a bad since", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, autoLoginMs: 0, now: () => NOW });
    await expect(api.jobs.stats("nope")).rejects.toMatchObject({ status: 404 });
    await expect(api.jobs.stats("terrarium", "yesterday")).rejects.toMatchObject({ status: 400 });
  });
});
