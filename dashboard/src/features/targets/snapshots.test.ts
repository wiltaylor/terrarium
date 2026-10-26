import { describe, expect, it } from "vitest";
import type { TargetSnapshotList } from "../../api/types";
import { CLEAN_SNAPSHOT, deleteNeedsForce, deleteNotice, hasSnapshots, listSummary, snapshotNameError, snapshotScopes, verbNotice } from "./snapshots";

const list = (over: Partial<TargetSnapshotList> = {}): TargetSnapshotList => ({ target: "win", scope: "machine", machines: ["win11"], snapshots: [], dropped: [], ...over });

describe("the snapshot panel (docs/api.md §17.3)", () => {
  it("shows only for a vmlab target", () => {
    expect(hasSnapshots({ kind: "vmlab" })).toBe(true);
    for (const kind of ["docker", "host", "ssh"] as const) expect(hasSnapshots({ kind })).toBe(false);
  });

  it("offers the machine scope only when the target has a machine, first, as the daemon's default", () => {
    expect(snapshotScopes({ machine: "win11" })).toEqual(["machine", "lab"]);
    expect(snapshotScopes({})).toEqual(["lab"]);
  });

  it("refuses the names the daemon refuses, and takes an empty name (the daemon's terrarium)", () => {
    expect(snapshotNameError("")).toBeNull();
    expect(snapshotNameError("pre-upgrade")).toBeNull();
    expect(snapshotNameError("-x")).not.toBeNull();
    expect(snapshotNameError("a b")).not.toBeNull();
    expect(snapshotNameError("a\tb")).not.toBeNull();
  });

  it("asks for force only to delete terrarium-clean while snapshot_before_task is on", () => {
    expect(deleteNeedsForce({ snapshot_before_task: true }, CLEAN_SNAPSHOT)).toBe(true);
    expect(deleteNeedsForce({ snapshot_before_task: false }, CLEAN_SNAPSHOT)).toBe(false);
    expect(deleteNeedsForce({ snapshot_before_task: true }, "pre")).toBe(false);
  });

  it("sums up the list, the dropped names, and a lab that does not run", () => {
    const l = list({
      snapshots: [
        { name: "terrarium-clean", machines: ["win11"], managed: true },
        { name: "pre", machines: ["win11"], managed: false },
      ],
      dropped: ["old"],
    });
    expect(listSummary(l)).toBe("2 snapshots on win11 · 1 taken by Terrarium. The host no longer has old: removed from the target.");
    expect(listSummary(list({ snapshots: [{ name: "a", machines: ["win11"], managed: false }] }))).toBe("1 snapshot on win11 · 0 taken by Terrarium");
    expect(listSummary(list({ scope: "lab", lab_running: false, machines: [] }))).toMatch(/does not run/);
  });

  it("words the notices of a take, a restore, and a delete", () => {
    expect(verbNotice({ target: "win", verb: "snapshot", state: "running", snapshot: "pre", scope: "lab" })).toBe("win: took snapshot pre (whole lab) → running");
    expect(verbNotice({ target: "ws", verb: "restore", state: "running", snapshot: "pre", scope: "machine" }, "terra/w1")).toBe("ws (terra/w1): restored snapshot pre → running");
    expect(deleteNotice({ target: "win", verb: "snapshot_delete", snapshot: "pre", scope: "lab", machines: ["a", "b"] })).toBe("win: deleted snapshot pre on a, b");
  });
});
