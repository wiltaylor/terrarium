import { describe, expect, it } from "vitest";
import type { Agent } from "../api/types";
import { compactAction, idleStopped, lastCompactedText, lifecycleActions, lifecycleBadge, queuedLabel } from "./lifecycle";

type L = Pick<Agent, "status" | "paused" | "status_reason" | "waiting_on" | "queued">;
const a = (over: Partial<L> = {}): L => ({ status: "idle", paused: false, ...over });

describe("lifecycleBadge", () => {
  it("shows the status of an agent that runs", () => {
    expect(lifecycleBadge(a())).toMatchObject({ label: "idle", cls: "st-idle", title: "Status: idle" });
    expect(lifecycleBadge(a({ status: "working" }))).toMatchObject({ label: "working", cls: "st-working" });
    expect(lifecycleBadge(a({ status: "waiting", waiting_on: "time 10m" })).title).toBe("Status: waiting on time 10m");
  });

  it("shows `stopped (idle)` for an idle stop, and `stopped` for another stop", () => {
    expect(lifecycleBadge(a({ status: "stopped", status_reason: "idle" }))).toMatchObject({ label: "stopped (idle)", cls: "st-stopped" });
    expect(lifecycleBadge(a({ status: "stopped", status_reason: "stopped" }))).toMatchObject({ label: "stopped", cls: "st-stopped" });
    expect(lifecycleBadge(a({ status: "stopped" })).label).toBe("stopped");
    // An idle reason on a live status is old: the status wins.
    expect(lifecycleBadge(a({ status: "starting", status_reason: "idle" })).label).toBe("starting");
  });

  it("shows `paused` for a paused agent, whatever its status", () => {
    expect(lifecycleBadge(a({ status: "stopped", paused: true, status_reason: "paused" }))).toMatchObject({ label: "paused", cls: "st-paused" });
    expect(lifecycleBadge(a({ status: "stopped", paused: true, status_reason: "idle" })).label).toBe("paused");
    expect(lifecycleBadge(a({ status: "working", paused: true })).label).toBe("paused");
  });

  it("knows an idle stop", () => {
    expect(idleStopped({ status: "stopped", status_reason: "idle" })).toBe(true);
    expect(idleStopped({ status: "stopped", status_reason: "paused" })).toBe(false);
    expect(idleStopped({ status: "idle", status_reason: "idle" })).toBe(false);
  });
});

describe("queuedLabel", () => {
  it("counts the work that waits, and is null when there is none", () => {
    expect(queuedLabel({ queued: 3 })).toBe("3 queued");
    expect(queuedLabel({ queued: 0 })).toBeNull();
    expect(queuedLabel({})).toBeNull();
  });
});

describe("lifecycleActions", () => {
  it("has Pause and Restart, and no Start or Stop, for an agent that is not paused", () => {
    for (const status of ["idle", "working", "stopped", "error"] as const) {
      const b = lifecycleActions({ ...a({ status }) });
      expect(b.map((x) => [x.kind, x.label, x.disabled])).toEqual([
        ["pause", "Pause", false],
        ["restart", "Restart", false],
      ]);
    }
  });

  it("has Resume, and Restart off, for a paused agent", () => {
    const b = lifecycleActions({ paused: true });
    expect(b.map((x) => [x.kind, x.label, x.disabled])).toEqual([
      ["resume", "Resume", false],
      ["restart", "Restart", true],
    ]);
    expect(b[1]!.title).toMatch(/Resume it first/);
  });
});

describe("compactAction", () => {
  type C = Parameters<typeof compactAction>[0];
  const c = (over: Partial<C> = {}): C => ({ harness: "claude", status: "idle", paused: false, compact_supported: true, ...over });

  it("is on for an agent at rest", () => {
    expect(compactAction(c()).disabled).toBe(false);
    expect(compactAction(c({ status: "waiting" })).disabled).toBe(false);
  });

  it("is off, with the reason, when the daemon would refuse it", () => {
    expect(compactAction(c({ compact_supported: false, harness: "pi" }))).toEqual({ disabled: true, title: "auto compact not supported for pi" });
    expect(compactAction(c({ paused: true, status: "stopped" })).title).toMatch(/paused/);
    expect(compactAction(c(), true).title).toMatch(/hold/);
    expect(compactAction(c({ status: "working", status_reason: "compacting" })).title).toMatch(/compacts its session now/);
    expect(compactAction(c({ status: "working" })).title).toMatch(/working.*wait until it is idle/);
    expect(compactAction(c({ status: "stopped" })).title).toMatch(/stopped/);
    expect(compactAction(c({ status: "starting" })).disabled).toBe(true);
  });

  it("says when the last compaction was, and when it failed", () => {
    const at = (iso: string) => `at ${iso}`;
    expect(lastCompactedText({}, at)).toBeNull();
    expect(lastCompactedText({ last_compact_at: "T", last_compact_ok: true }, at)).toBe("last compacted at T");
    expect(lastCompactedText({ last_compact_at: "T", last_compact_ok: false }, at)).toBe("last compacted at T (failed)");
  });
});
