import { describe, expect, it } from "vitest";
import { blockedReason, canRollback, imageText, outcomeChip, updateTarget } from "./self-update";
import { createMockApi } from "../api/mock";
import type { SelfUpdateReport } from "../api/types";

describe("self-update (docs/api.md §34)", () => {
  it("names each outcome", () => {
    expect(outcomeChip("succeeded")).toEqual({ label: "succeeded", cls: "chip ok" });
    expect(outcomeChip("rolled_back").label).toBe("failed, rolled back");
    expect(outcomeChip("rollback_failed").cls).toContain("bad");
  });

  it("blocks the buttons without an updater or during a run", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const r = await api.selfUpdate.status();
    expect(blockedReason(r)).toBeNull();
    expect(blockedReason(undefined)).toBe("Loading…");
    const down: SelfUpdateReport = { ...r, updater: { ...r.updater, reachable: false, status: null } };
    expect(blockedReason(down)).toContain("terrarium-updater");
    expect(canRollback(down)).toBe(false);
    expect(updateTarget(r)).toBe(r.latest!.version);
    expect(updateTarget({ ...r, latest: null, channel: "pre" })).toBe("pre");
    expect(imageText({ reference: "terrarium:dev" })).toBe("terrarium:dev");
    expect(imageText({ reference: "x@sha256:1", version: "0.2.0" })).toBe("0.2.0 (x@sha256:1)");
  });

  it("the mock updates, then rolls back to the previous image", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    let r = await api.selfUpdate.status();
    expect(r.update_available).toBe(true);
    expect(canRollback(r)).toBe(false);
    const started = await api.selfUpdate.apply();
    expect(started.target).toBe(r.latest!.version);
    r = await api.selfUpdate.status();
    expect(r.last_result).toMatchObject({ id: started.run, kind: "update", outcome: "succeeded" });
    expect(r.update_available).toBe(false);
    expect(canRollback(r)).toBe(true);
    const back = await api.selfUpdate.rollback();
    r = await api.selfUpdate.status();
    expect(r.last_result).toMatchObject({ id: back.run, kind: "rollback", outcome: "succeeded" });
    expect(r.update_available).toBe(true);
    await expect(api.selfUpdate.apply("not a version!")).rejects.toMatchObject({ status: 422 });
  });
});
