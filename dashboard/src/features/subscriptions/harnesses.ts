// The "Harness versions" table of the Subscriptions page (GET /api/harnesses
// ?latest=true, docs/api.md §4). harnesses.toml pins each harness; the daemon
// compares the installed version with the adapter's tested range at every
// agent start, and reads the latest release of each package on npm.

import type { HarnessReport, HarnessVersion, Host } from "../../api/types";
import { HARNESS_ROLLOUT } from "../../lib/harness-update";

export { HARNESS_ROLLOUT };

/** The status chip of one row. */
export type HarnessStatus = "unsupported" | "update available (major)" | "update available" | "latest" | "in range" | "unknown";

/**
 * The chip of one row. In order:
 * - `unsupported`: the installed version is outside the tested range (`in_range` false);
 * - `update available (major)` / `update available` / `latest`: from `update` (pin against npm);
 * - npm did not answer: `in range` when the installed version is in the range, else `unknown`.
 */
export function harnessStatus(h: HarnessVersion): { status: HarnessStatus; cls: string; title: string } {
  if (h.in_range === false)
    return {
      status: "unsupported",
      cls: "bad",
      title: `${h.installed} is outside the tested range ${h.tested}. Rebuild the image (just up), or run just harness::update ${h.harness}.`,
    };
  const latest = h.latest?.version;
  if (h.update === "major")
    return { status: "update available (major)", cls: "warn", title: `${h.pinned} -> ${latest} is a major bump: hooks and flags may have changed.` };
  if (h.update === "minor") return { status: "update available", cls: "info", title: `${h.pinned} -> ${latest} on npm.` };
  if (h.update === "none") return { status: "latest", cls: "ok", title: `${h.pinned} is the latest release on npm.` };
  const why = h.latest_error ? `The latest release is not known: ${h.latest_error}` : "The latest release is not known.";
  if (h.in_range === true) return { status: "in range", cls: "ok", title: `${h.installed} is in ${h.tested}. ${why}` };
  return { status: "unknown", cls: "warn", title: h.error ?? "The daemon could not read the version." };
}

/** Does the installed version differ from the pin (in range or not)? */
export const offPin = (h: HarnessVersion) => h.installed !== null && h.installed !== h.pinned;

/** The harnesses outside their range. */
export const outOfRange = (r: HarnessReport | undefined) => (r?.harnesses ?? []).filter((h) => h.in_range === false);

/** The harnesses with a newer release on npm. */
export const withUpdate = (r: HarnessReport | undefined) => (r?.harnesses ?? []).filter((h) => h.update === "minor" || h.update === "major");

/**
 * The command to run on the Docker host, or null when nothing is newer. One harness:
 * `just harness::update <name>` (with `--allow-major` for a major), then the rollout,
 * because `update` does not roll out. More: `just harness::latest`, which rolls out itself.
 */
export function updateCommand(r: HarnessReport | undefined): string | null {
  const ups = withUpdate(r);
  if (ups.length === 0) return null;
  if (ups.length > 1) return "just harness::latest";
  const h = ups[0]!;
  return `just harness::update ${h.harness}${h.update === "major" ? " --allow-major" : ""} && just harness::rollout`;
}

/** The `names` of the host verb for the same choice: the one harness, or `"all"`. */
export function updateNames(r: HarnessReport | undefined): "all" | string[] {
  const ups = withUpdate(r);
  return ups.length === 1 ? [ups[0]!.harness] : "all";
}

/** The online host agents that run `harness.update` (`terrarium_repo_dir` in their config). */
export const updateHosts = (hosts: Host[] | undefined) =>
  (hosts ?? []).filter((h) => (h.online ?? h.status === "online") && h.capabilities?.can_update_harnesses === true);

/** `2026-10-25T09:30:00Z` as `2026-10-25`. */
export const publishedDay = (at: string | null | undefined) => (at ? at.slice(0, 10) : null);

/** The line under the table: what a start does outside the range. */
export function policyNote(r: HarnessReport): string {
  return r.policy === "refuse"
    ? "An agent start on a harness outside its tested range is refused (TERRARIUM_HARNESS_VERSION_POLICY=refuse)."
    : "An agent start on a harness outside its tested range goes ahead with a warning event (TERRARIUM_HARNESS_VERSION_POLICY=warn).";
}
