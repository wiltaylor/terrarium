import { describe, expect, it } from "vitest";
import type { Target, TargetKind, TargetState } from "../../api/types";
import { STATE_CHIP, targetActions } from "./actions";

const t = (kind: TargetKind, state: TargetState, snapshots: string[] = []): Pick<Target, "kind" | "state" | "snapshots"> => ({ kind, state, snapshots });

describe("targetActions (docs/api.md §17.3)", () => {
  it("offers up for a stopped or absent docker or vmlab target, and down for a running one", () => {
    expect(targetActions(t("docker", "stopped"))).toEqual(["up", "status"]);
    expect(targetActions(t("docker", "absent"))).toEqual(["up", "status"]);
    expect(targetActions(t("docker", "running"))).toEqual(["down", "status"]);
    expect(targetActions(t("vmlab", "absent"))).toEqual(["up", "status"]);
  });

  it("offers snapshot only on a running vmlab target, and restore when it has the snapshot terrarium", () => {
    expect(targetActions(t("vmlab", "running"))).toEqual(["down", "snapshot", "status"]);
    expect(targetActions(t("vmlab", "running", ["terrarium-clean"]))).toEqual(["down", "snapshot", "status"]);
    expect(targetActions(t("vmlab", "running", ["terrarium"]))).toEqual(["down", "snapshot", "restore", "status"]);
    expect(targetActions(t("vmlab", "stopped", ["terrarium"]))).toEqual(["up", "status"]);
  });

  it("offers only status for an unknown state, and for host and ssh targets, where up and down are no-ops", () => {
    expect(targetActions(t("docker", "unknown"))).toEqual(["status"]);
    expect(targetActions(t("vmlab", "unknown", ["terrarium"]))).toEqual(["status"]);
    expect(targetActions(t("host", "running"))).toEqual(["status"]);
    expect(targetActions(t("ssh", "unknown"))).toEqual(["status"]);
  });

  it("has a chip class for every daemon state", () => {
    expect(Object.keys(STATE_CHIP).sort()).toEqual(["absent", "running", "stopped", "unknown"]);
    expect(STATE_CHIP.running).toBe("ok");
  });

  it("offers no target verbs on a lab_from_workspace target: each verb acts on one holder's instance", () => {
    expect(targetActions({ ...t("vmlab", "unknown"), lab_from_workspace: true })).toEqual([]);
    expect(targetActions({ ...t("vmlab", "running", ["terrarium"]), lab_from_workspace: true })).toEqual([]);
  });
});
