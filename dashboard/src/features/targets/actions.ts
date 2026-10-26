// The lifecycle verbs and state chips of a target (docs/api.md §17.2, §17.3). Pure.

import type { TargetAction } from "../../api/client";
import type { Target, TargetState } from "../../api/types";

export const STATE_CHIP: Record<TargetState, string> = { running: "ok", stopped: "", absent: "", unknown: "warn" };

/**
 * The verbs to offer (docs/api.md §17.3). `up` for a stopped or absent
 * docker or vmlab target, `down` for a running one. `status` always. On
 * `host` and `ssh` targets, up and down are no-ops, so only `status` shows.
 * A vmlab target takes, restores, and deletes snapshots in its snapshot
 * panel (`./snapshots.ts`), not here.
 * A `lab_from_workspace` target has no verbs of its own: each verb acts on
 * the instance of one holder (`agent` in the body), so the instance rows
 * offer them.
 */
export function targetActions(t: Pick<Target, "kind" | "state" | "lab_from_workspace">): TargetAction[] {
  if (t.lab_from_workspace) return [];
  const out: TargetAction[] = [];
  if (t.kind === "docker" || t.kind === "vmlab") {
    if (t.state === "stopped" || t.state === "absent") out.push("up");
    if (t.state === "running") out.push("down");
  }
  out.push("status");
  return out;
}
