// The self-update (docs/api.md §34, `GET /api/system/update`): what the
// Settings > Software tab shows and allows.

import type { SelfUpdateReport, UpdateOutcome, UpdateRun } from "../api/types";

/** The chip of an outcome: its text and its class. */
export function outcomeChip(o: UpdateOutcome): { label: string; cls: string } {
  switch (o) {
    case "running":
      return { label: "running", cls: "chip" };
    case "succeeded":
      return { label: "succeeded", cls: "chip ok" };
    case "current":
      return { label: "already current", cls: "chip" };
    case "failed":
      return { label: "failed, nothing changed", cls: "chip warn" };
    case "rolled_back":
      return { label: "failed, rolled back", cls: "chip warn" };
    case "rollback_failed":
      return { label: "failed, rollback failed", cls: "chip bad" };
  }
}

/** The run that the updater runs now, if any. */
export function runningRun(r: SelfUpdateReport | null | undefined): UpdateRun | null {
  return r?.updater.status?.running ?? null;
}

/** Why the Update and Roll back buttons are off; null when they work. */
export function blockedReason(r: SelfUpdateReport | null | undefined): string | null {
  if (!r) return "Loading…";
  if (!r.updater.reachable) return "The updater does not answer: start the terrarium-updater service (compose profile `updater`).";
  if (runningRun(r)) return "An update or a rollback runs now.";
  return null;
}

/** The target of the Update button: the newest release of the channel, else the channel tag. */
export function updateTarget(r: SelfUpdateReport): string {
  return r.latest?.version ?? (r.channel === "pre" ? "pre" : "latest");
}

/** True when a rollback has a target. */
export function canRollback(r: SelfUpdateReport | null | undefined): boolean {
  return blockedReason(r) === null && !!r?.updater.status?.previous;
}

/** One line for an image: the version label, else the reference. */
export function imageText(i: { reference: string; version?: string } | null | undefined): string {
  if (!i) return "—";
  return i.version ? `${i.version} (${i.reference})` : i.reference;
}
