// The versions of the cards on the board (docs/api.md §13): pure helpers.
// No Solid imports. A card is in a version when `Version.cards` has it. A
// card can be in several versions, or in none. The version lanes of the
// Cards tab are in lanes.ts.

import type { Version, VersionStatus } from "../../api/types";

/** The chip class of a version status. */
export const VERSION_CHIP: Record<VersionStatus, string> = { draft: "", in_review: "warn", approved: "ok", rejected: "bad", released: "info" };

/** The versions of each card, by card ID, in the order of the list. */
export function versionsByCard(versions: readonly Version[]): Map<string, Version[]> {
  const out = new Map<string, Version[]>();
  for (const v of versions) {
    for (const c of v.cards) {
      const list = out.get(c.id) ?? [];
      // A card that is two times in one version shows one chip.
      if (!list.includes(v)) list.push(v);
      out.set(c.id, list);
    }
  }
  return out;
}
