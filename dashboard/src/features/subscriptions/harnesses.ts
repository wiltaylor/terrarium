// The "Harness versions" table of the Subscriptions page (GET /api/harnesses,
// docs/api.md §4). harnesses.toml pins each harness; the daemon compares the
// installed version with the adapter's tested range at every agent start.

import type { HarnessReport, HarnessVersion } from "../../api/types";

/** The chip of one row: in range, out of range, or not known. */
export function harnessBadge(h: HarnessVersion): { cls: string; text: string; title: string } {
  if (h.in_range === true) return { cls: "ok", text: "in range", title: `${h.installed} is in ${h.tested}` };
  if (h.in_range === false)
    return {
      cls: "bad",
      text: "out of range",
      title: `${h.installed} is outside ${h.tested}. Rebuild the image (just up) or run just harness::update ${h.harness}.`,
    };
  return { cls: "warn", text: "unknown", title: h.error ?? "The daemon could not read the version." };
}

/** Does the installed version differ from the pin (in range or not)? */
export const offPin = (h: HarnessVersion) => h.installed !== null && h.installed !== h.pinned;

/** The harnesses outside their range. */
export const outOfRange = (r: HarnessReport | undefined) => (r?.harnesses ?? []).filter((h) => h.in_range === false);

/** The line under the table: what a start does outside the range. */
export function policyNote(r: HarnessReport): string {
  return r.policy === "refuse"
    ? "An agent start on a harness outside its tested range is refused (TERRARIUM_HARNESS_VERSION_POLICY=refuse)."
    : "An agent start on a harness outside its tested range goes ahead with a warning event (TERRARIUM_HARNESS_VERSION_POLICY=warn).";
}
