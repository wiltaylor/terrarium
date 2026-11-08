import { describe, expect, it } from "vitest";
import { stepTiming } from "./stepTiming";

describe("group step timing", () => {
  const step = { name: "e2e", state: "running" as const, text: "", eta_at: "2026-11-08T12:01:00Z" };
  const now = Date.parse("2026-11-08T12:00:00Z");
  it("counts down and marks overdue steps", () => {
    expect(stepTiming(step, now)).toBe("1m 00s left");
    expect(stepTiming(step, now + 120000)).toBe("overdue 1m 00s");
  });
  it("shows actual duration instead of the old ETA", () => {
    expect(stepTiming({ ...step, state: "done", duration_secs: 90 }, now)).toBe("took 1m 30s");
  });
  it("does not invent an estimate without samples", () => {
    expect(stepTiming({ ...step, eta_at: null }, now)).toBe("ETA unavailable (no history)");
  });
});
