// The snapshot panel of a vmlab target (docs/api.md §17.3): the list from
// `GET /api/targets/{name}/snapshots`, take, restore, and delete. Pure.

import type { SnapshotDeleted, SnapshotScope, Target, TargetReport, TargetSnapshotList } from "../../api/types";

/** The snapshot that `snapshot_before_task` takes and restores. */
export const CLEAN_SNAPSHOT = "terrarium-clean";

/** Only a vmlab target has snapshots. A docker, host, or ssh target shows no panel. */
export const hasSnapshots = (t: Pick<Target, "kind">) => t.kind === "vmlab";

/** The scopes to offer: the machine (when the target has one) and the whole lab. The first is the daemon's default. */
export const snapshotScopes = (t: Pick<Target, "machine">): SnapshotScope[] => (t.machine ? ["machine", "lab"] : ["lab"]);

/** Why a snapshot name is refused, or null. Empty is fine: the daemon takes `terrarium`. The daemon refuses a leading `-`, a space, and a control character. */
export function snapshotNameError(name: string): string | null {
  if (name === "") return null;
  if (name.startsWith("-")) return "A snapshot name cannot start with -.";
  // eslint-disable-next-line no-control-regex
  if (/[\s\u0000-\u001f\u007f]/.test(name)) return "A snapshot name has no spaces.";
  return null;
}

/** A delete of `terrarium-clean` while `snapshot_before_task` is on needs `force` (else `409 conflict`). */
export const deleteNeedsForce = (t: Pick<Target, "snapshot_before_task">, name: string) => name === CLEAN_SNAPSHOT && !!t.snapshot_before_task;

/** One line above the list: what the host has, on which machines, and the names the list dropped. */
export function listSummary(l: TargetSnapshotList): string {
  if (l.lab_running === false) return "The lab does not run, so vmlab names no machines. Bring it up to see lab-wide snapshots.";
  const n = l.snapshots.length;
  const managed = l.snapshots.filter((s) => s.managed).length;
  const where = l.machines.length ? ` on ${l.machines.join(", ")}` : "";
  let out = `${n} snapshot${n === 1 ? "" : "s"}${where} · ${managed} taken by Terrarium`;
  if (l.dropped.length) out += `. The host no longer has ${l.dropped.join(", ")}: removed from the target.`;
  return out;
}

/** The notice of a take or a restore. */
export const verbNotice = (r: TargetReport, who?: string) =>
  `${r.target}${who ? ` (${who})` : ""}: ${r.verb === "restore" ? "restored" : "took"} snapshot ${r.snapshot ?? "terrarium"}${r.scope === "lab" ? " (whole lab)" : ""} → ${r.state}`;

/** The notice of a delete. */
export const deleteNotice = (d: SnapshotDeleted, who?: string) => `${d.target}${who ? ` (${who})` : ""}: deleted snapshot ${d.snapshot} on ${d.machines.join(", ") || "no machine"}`;
